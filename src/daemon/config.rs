//! pand configuration — ONE file, `~/.config/pan/config.yml`, no switches.
//!
//! What it declares (Rob, 2026-09-03): the list of store directories this
//! machine's one pand manages, which of them is the default, and the model
//! endpoints pand calls. A missing file means one standalone store at
//! `~/.pan` and no models — pand still runs, stores still land, the model
//! stages simply report "off".
//!
//! `deny_unknown_fields`: a mistyped key is a loud error at start, never a
//! silently-ignored line.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 7401;
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// A configured model endpoint for one stage. `url` is where pand posts the
/// image; `model` is the name pand records as `pan:model` on every record the
/// stage writes, so "which model produced this" is data in the graph, never a
/// guess from a URL.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelEndpoint {
    pub url: String,
    pub model: String,
    /// For an `embed` endpoint that is really `/see_embed` (one image load
    /// gives caption AND vector): the captioning model's name, so the caption
    /// it returns is recorded under the right `pan:model`. Absent = the
    /// caption is not recorded from this stage.
    pub caption_model: Option<String>,
    /// Test mode (Rob, 2026-09-03): `enabled: false` keeps the stage declared
    /// but pand never calls it — ingest still lands, `pan state` says "off",
    /// and turning it back on later picks up every image missing this model's
    /// record (the graph is the queue). Default true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// How many calls to this endpoint may be in flight at once across ALL
    /// stores. pand is the one funnel for model traffic on the machine.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

fn default_concurrency() -> usize {
    1
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ConfigYml {
    #[serde(default)]
    stores: Vec<PathBuf>,
    default: Option<PathBuf>,
    /// Where big media lives when not on the system drive: every store's
    /// media root becomes `<media_volume>/<store id>/media`. Absent = each
    /// store's own `_ignore/media` pocket.
    media_volume: Option<PathBuf>,
    port: Option<u16>,
    #[serde(default)]
    models: BTreeMap<String, ModelEndpoint>,
    /// Seconds between two passes of the stage ladder over every store.
    interval_secs: Option<u64>,
    /// How many images one stage handles per pass per store. Bounded so one
    /// store with a backlog cannot starve the others.
    batch: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub path: PathBuf,
    /// Store entries exactly as written (a soul repo root or a bare store
    /// directory); the registry resolves each into an id + store root.
    pub stores: Vec<PathBuf>,
    pub default: Option<PathBuf>,
    pub media_volume: Option<PathBuf>,
    pub bind: String,
    pub port: u16,
    /// stage name → endpoint. Known stage names: embed, caption, sam3, pose.
    pub models: BTreeMap<String, ModelEndpoint>,
    pub interval_secs: u64,
    pub batch: usize,
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".config").join("pan"))
        .unwrap_or_else(|| PathBuf::from(".config/pan"))
}

pub fn default_store_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".pan"))
        .unwrap_or_else(|| PathBuf::from(".pan"))
}

fn expand_home(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(rest);
        }
    }
    p.to_path_buf()
}

impl DaemonConfig {
    /// The media root for one store under this config, or None for the pocket default.
    pub fn media_root_for(&self, store_id: &str) -> Option<PathBuf> {
        self.media_volume.as_ref().map(|v| v.join(store_id).join("media"))
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&config_dir().join("config.yml"))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let yml: ConfigYml = if path.exists() {
            let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
            serde_yaml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?
        } else {
            ConfigYml::default()
        };
        let mut stores: Vec<PathBuf> = yml.stores.iter().map(|p| expand_home(p)).collect();
        if stores.is_empty() {
            stores.push(default_store_dir());
        }
        for m in yml.models.values() {
            if m.url.is_empty() || m.model.is_empty() {
                return Err(anyhow!("{}: every model needs both url and model", path.display()));
            }
            if m.concurrency == 0 {
                return Err(anyhow!("{}: model concurrency must be at least 1", path.display()));
            }
        }
        Ok(DaemonConfig {
            path: path.to_path_buf(),
            stores,
            default: yml.default.map(|p| expand_home(&p)),
            media_volume: yml.media_volume.map(|p| expand_home(&p)),
            bind: DEFAULT_BIND.to_string(),
            port: yml.port.unwrap_or(DEFAULT_PORT),
            models: yml.models,
            interval_secs: yml.interval_secs.unwrap_or(5),
            batch: yml.batch.unwrap_or(8),
        })
    }

    /// The stages pand will actually run: configured AND enabled.
    pub fn active_models(&self) -> impl Iterator<Item = (&String, &ModelEndpoint)> {
        self.models.iter().filter(|(_, m)| m.enabled)
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.bind, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_one_home_store_and_no_models() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = DaemonConfig::load_from(&dir.path().join("config.yml")).unwrap();
        assert_eq!(cfg.stores, vec![default_store_dir()]);
        assert!(cfg.models.is_empty());
        assert_eq!(cfg.port, DEFAULT_PORT);
    }

    #[test]
    fn full_file_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "stores:\n  - /souls/a\n  - ~/.pan\ndefault: /souls/a\nport: 7402\nmodels:\n  embed:\n    url: http://127.0.0.1:1215/see_embed\n    model: qwen-vl-2b\n    concurrency: 2\n",
        )
        .unwrap();
        let cfg = DaemonConfig::load_from(&p).unwrap();
        assert_eq!(cfg.stores.len(), 2);
        assert!(!cfg.stores[1].to_string_lossy().starts_with('~'), "home expanded");
        assert_eq!(cfg.default, Some(PathBuf::from("/souls/a")));
        assert_eq!(cfg.port, 7402);
        assert_eq!(cfg.models["embed"].concurrency, 2);
        assert!(cfg.models["embed"].enabled, "enabled defaults to true");
    }

    #[test]
    fn disabled_stage_is_declared_but_not_active() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(&p, "models:\n  pose:\n    url: http://x/see_pose\n    model: rtmw\n    enabled: false\n").unwrap();
        let cfg = DaemonConfig::load_from(&p).unwrap();
        assert!(cfg.models.contains_key("pose"));
        assert_eq!(cfg.active_models().count(), 0);
    }

    #[test]
    fn unknown_key_is_loud() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(&p, "stroes:\n  - /x\n").unwrap();
        assert!(DaemonConfig::load_from(&p).is_err());
    }
}
