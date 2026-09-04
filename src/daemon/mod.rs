//! pand — the one daemon per machine that owns every Pan store on it.
//!
//! What pand is (Rob, 2026-09-03): the only process that touches a store's
//! filesystem or writes its graph; the single funnel through which model
//! calls leave the machine; and the thing that keeps filesystem and graph in
//! step in small atomic steps — never a pull-down-and-rewalk. `pan` (the CLI)
//! and Horae are clients of pand; git-lex and Syrinx read the stores.
//!
//! An image does not exist in the system until pand has committed its graph
//! node. Everything after that — thumbnail is part of ingest; embedding,
//! caption, pose, regions are STAGES — is found by asking the graph what is
//! missing, done, and recorded, one image at a time.

pub mod config;
pub mod http;
pub mod iris;
pub mod registry;
pub mod stages;

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use crate::Pan;
use config::DaemonConfig;
use registry::StoreEntry;

/// One managed store: its registry entry and the open store.
pub struct StoreHandle {
    pub entry: StoreEntry,
    pub pan: Pan,
}

/// Why a stage did not complete for one image, remembered so the next pass
/// does not hammer the same failure. In memory only: a restart forgets it,
/// which is the retry you want after fixing the cause. The GRAPH holds what
/// succeeded; this holds only what to wait on.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub at: Instant,
    pub error: String,
    pub terminal: bool,
}

pub const TRANSIENT_BACKOFF: Duration = Duration::from_secs(600);

pub struct Daemon {
    pub cfg: DaemonConfig,
    pub stores: Vec<Arc<StoreHandle>>,
    pub default_id: String,
    pub iris: iris::Iris,
    /// stage name → the funnel: at most `concurrency` calls in flight.
    pub funnels: HashMap<String, Arc<Semaphore>>,
    /// (store id, media id, stage) → last failed attempt.
    pub attempts: Mutex<HashMap<(String, String, String), Attempt>>,
    pub started: Instant,
}

impl Daemon {
    pub fn open(cfg: DaemonConfig) -> Result<Self> {
        let entries = registry::resolve_all(&cfg.stores)?;
        let mut stores = Vec::with_capacity(entries.len());
        for e in entries {
            if e.is_repo {
                warn_if_not_ignored(&e.declared);
            }
            let media_root = cfg.media_root_for(&e.id);
            let pan = Pan::open_with(&e.root, &e.id, media_root.as_deref())
                .with_context(|| format!("open store {} at {}", e.id, e.root.display()))?;
            tracing::info!(id = %e.id, root = %e.root.display(), media = %pan.layout.media_root.display(), "store open");
            stores.push(Arc::new(StoreHandle { entry: e, pan }));
        }
        let default_id = match &cfg.default {
            Some(p) => stores
                .iter()
                .find(|s| &s.entry.declared == p || &s.entry.root == p || s.entry.id == p.to_string_lossy())
                .map(|s| s.entry.id.clone())
                .ok_or_else(|| anyhow!("default {} is not one of the configured stores", p.display()))?,
            None => stores[0].entry.id.clone(),
        };
        let funnels = cfg
            .models
            .iter()
            .map(|(name, m)| (name.clone(), Arc::new(Semaphore::new(m.concurrency))))
            .collect();
        Ok(Daemon {
            cfg,
            stores,
            default_id,
            iris: iris::Iris::new(),
            funnels,
            attempts: Mutex::new(HashMap::new()),
            started: Instant::now(),
        })
    }

    pub fn store(&self, id: &str) -> Option<Arc<StoreHandle>> {
        self.stores.iter().find(|s| s.entry.id == id).cloned()
    }

    /// The store a request means: the named one, or the default when none is
    /// named. An UNKNOWN name is an error, never a fallback to the default —
    /// the old Door bug (`soul=W4R3Z` answering with lUX's data) must stay
    /// structurally impossible.
    pub fn store_for(&self, named: Option<&str>) -> Result<Arc<StoreHandle>> {
        match named {
            None | Some("") => self.store(&self.default_id).ok_or_else(|| anyhow!("default store missing")),
            Some(id) => self.store(id).ok_or_else(|| anyhow!("unknown store: {id}")),
        }
    }

    /// Find which store holds a media id (ids are random per store; a hit in
    /// more than one store is reported, not silently first-wins).
    pub fn locate(&self, media_id: &str) -> Result<Option<Arc<StoreHandle>>> {
        let mut found: Vec<Arc<StoreHandle>> = Vec::new();
        for s in &self.stores {
            if s.pan.subject_for(media_id)?.is_some() {
                found.push(s.clone());
            }
        }
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            n => Err(anyhow!("id {media_id} exists in {n} stores — ambiguous")),
        }
    }

    pub fn record_attempt(&self, store: &str, media: &str, stage: &str, error: String, terminal: bool) {
        let mut a = self.attempts.lock().unwrap();
        a.insert(
            (store.to_string(), media.to_string(), stage.to_string()),
            Attempt { at: Instant::now(), error, terminal },
        );
    }

    pub fn clear_attempt(&self, store: &str, media: &str, stage: &str) {
        let mut a = self.attempts.lock().unwrap();
        a.remove(&(store.to_string(), media.to_string(), stage.to_string()));
    }

    /// Whether a stage should be skipped for now: a terminal refusal, or a
    /// transient failure still inside its backoff.
    pub fn holding(&self, store: &str, media: &str, stage: &str) -> Option<Attempt> {
        let a = self.attempts.lock().unwrap();
        let att = a.get(&(store.to_string(), media.to_string(), stage.to_string()))?;
        if att.terminal || att.at.elapsed() < TRANSIENT_BACKOFF {
            Some(att.clone())
        } else {
            None
        }
    }

    pub fn last_attempt(&self, store: &str, media: &str, stage: &str) -> Option<Attempt> {
        let a = self.attempts.lock().unwrap();
        a.get(&(store.to_string(), media.to_string(), stage.to_string())).cloned()
    }
}

/// A soul repo's `.pan` must never enter git history. pand does not edit
/// another repo's `.gitignore` (that is the kit's job); it says so loudly.
fn warn_if_not_ignored(repo: &Path) {
    let gi = repo.join(".gitignore");
    let ignored = std::fs::read_to_string(&gi)
        .map(|s| s.lines().any(|l| matches!(l.trim(), ".pan" | ".pan/" | "/.pan" | "/.pan/")))
        .unwrap_or(false);
    if !ignored {
        tracing::warn!(
            repo = %repo.display(),
            ".pan is not in this repo's .gitignore — media would enter git history; add a `.pan/` line"
        );
    }
}
