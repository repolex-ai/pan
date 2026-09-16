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

pub mod calllog;
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
    /// stage name → the funnel: an adaptive window of calls in flight, never
    /// more than the stage's configured `concurrency`.
    pub funnels: HashMap<String, Arc<Limiter>>,
    /// (store id, media id, stage) → last failed attempt.
    pub attempts: Mutex<HashMap<(String, String, String), Attempt>>,
    pub started: Instant,
    /// Since this process started — what `pand status` reports, so anyone
    /// can see at a glance whether the thing that makes model calls is
    /// making them (Rob, 2026-09-04).
    pub counters: Counters,
    /// stage name → until when the WHOLE stage is held. Set when a call fails
    /// before reaching the model (connection refused/reset/timeout): the door
    /// is down, so walking the rest of the batch would only fail the same
    /// way, image after image (2026-09-05: 42 failed calls in 90 s against a
    /// dark :1215). One try per stage per hold, then the batch resumes.
    pub stage_hold: Mutex<HashMap<String, Instant>>,
    /// One JSON line per model call, by day, beside the config; pruned by
    /// `log_keep_days`. Metadata only — see `calllog.rs`.
    pub calls: calllog::CallLog,
}

/// How many calls a stage keeps in flight, decided by the answers it gets —
/// additive increase, multiplicative decrease, the shape TCP and every
/// adaptive-concurrency limiter use. Pan does not know how many nodes stand
/// behind the door and must not need to (Rob, 2026-09-05: Salad is flaky by
/// design; when two nodes are up, use both; stay standard and self-contained).
/// The door's `503 busy` IS the count of nodes, read live:
///
/// - start at 1 in flight;
/// - after [`RAMP_AFTER`] consecutive successes at the current window, +1,
///   up to the configured `concurrency` (the ceiling, never exceeded);
/// - on any `busy`, halve (floor 1) and start the streak over.
///
/// With one node up the window settles at 1 (the second call gets `busy`);
/// with two it climbs to 2 and stays; a node dropping out pulls it back within
/// one answer. Nothing is configured but the ceiling.
pub struct Limiter {
    stage: String,
    sem: Arc<Semaphore>,
    max: usize,
    limit: std::sync::atomic::AtomicUsize,
    streak: std::sync::atomic::AtomicUsize,
}

pub const RAMP_AFTER: usize = 4;

impl Limiter {
    pub fn new(stage: &str, max: usize) -> Self {
        let max = max.max(1);
        Limiter {
            stage: stage.to_string(),
            sem: Arc::new(Semaphore::new(1)),
            max,
            limit: std::sync::atomic::AtomicUsize::new(1),
            streak: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.sem.clone().acquire_owned().await.expect("limiter semaphore never closes")
    }

    /// The window right now.
    pub fn window(&self) -> usize {
        self.limit.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn ceiling(&self) -> usize {
        self.max
    }

    pub fn on_success(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let streak = self.streak.fetch_add(1, Relaxed) + 1;
        let cur = self.limit.load(Relaxed);
        if streak >= RAMP_AFTER && cur < self.max {
            self.limit.store(cur + 1, Relaxed);
            self.streak.store(0, Relaxed);
            self.sem.add_permits(1);
            tracing::info!(stage = %self.stage, window = cur + 1, ceiling = self.max, "widening: {streak} answers in a row at {cur}");
        }
    }

    pub fn on_busy(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.streak.store(0, Relaxed);
        let cur = self.limit.load(Relaxed);
        let next = (cur / 2).max(1);
        if next < cur {
            self.limit.store(next, Relaxed);
            // Permits already in flight are returned by their holders; take
            // that many back as they come free and drop them on the floor.
            for _ in 0..(cur - next) {
                let sem = self.sem.clone();
                tokio::spawn(async move {
                    if let Ok(p) = sem.acquire_owned().await {
                        p.forget();
                    }
                });
            }
            tracing::info!(stage = %self.stage, window = next, ceiling = self.max, "narrowing: door said busy at {cur}");
        }
    }
}

#[derive(Default)]
pub struct Counters {
    pub images_stored: std::sync::atomic::AtomicU64,
    pub model_calls: std::sync::atomic::AtomicU64,
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
            .map(|(name, m)| (name.clone(), Arc::new(Limiter::new(name, m.concurrency))))
            .collect();
        // The log lives beside config.yml, whichever directory that is.
        let calls = calllog::CallLog::new(cfg.path.parent().unwrap_or(Path::new(".")), cfg.log_keep_days);
        Ok(Daemon {
            calls,
            cfg,
            stores,
            default_id,
            iris: iris::Iris::new(),
            funnels,
            attempts: Mutex::new(HashMap::new()),
            counters: Counters::default(),
            stage_hold: Mutex::new(HashMap::new()),
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

/// A soul repo's `.pan/_ignore` (the pocket: graph, index, media) must never
/// enter git history. pand does not edit another repo's `.gitignore` (git-lex
/// manages the `.pan/_ignore/` line); it asks git and says so loudly if the
/// answer is no. Asking git — not grepping for a spelling — means any pattern
/// that covers the pocket counts (`.pan/`, `.pan/_ignore/`, a global excludes
/// file, …).
fn warn_if_not_ignored(repo: &Path) {
    let ignored = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", ".pan/_ignore/oxigraph"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ignored {
        tracing::warn!(
            repo = %repo.display(),
            ".pan/_ignore is not gitignored in this repo — the graph and media would enter git history; run `git lex kit-update` (it adds the `.pan/_ignore/` line)"
        );
    }
}

#[cfg(test)]
mod limiter_tests {
    use super::*;

    #[tokio::test]
    async fn window_widens_on_a_streak_and_halves_on_busy() {
        let l = Arc::new(Limiter::new("embed", 4));
        assert_eq!((l.window(), l.ceiling()), (1, 4));
        // One permit available at the start; a second waits.
        let p1 = l.acquire().await;
        assert!(l.sem.try_acquire().is_err(), "window 1 = one in flight");
        drop(p1);
        for _ in 0..RAMP_AFTER {
            l.on_success();
        }
        assert_eq!(l.window(), 2);
        let _a = l.acquire().await;
        let _b = l.acquire().await;
        assert!(l.sem.try_acquire().is_err(), "window 2 = two in flight");
        for _ in 0..(RAMP_AFTER * 2) {
            l.on_success();
        }
        assert_eq!(l.window(), 4);
        l.on_busy();
        assert_eq!(l.window(), 2);
        l.on_busy();
        l.on_busy();
        assert_eq!(l.window(), 1, "never below one");
        // Never above the ceiling.
        for _ in 0..(RAMP_AFTER * 10) {
            l.on_success();
        }
        assert_eq!(l.window(), 4);
    }
}
