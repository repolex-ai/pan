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
    /// The instruction sent with the image to a captioning endpoint. In the
    /// config file this is the NAME of a plain-text file under
    /// `~/.config/pan/prompts/` (goodlux, 2026-09-08: the prompt text lives
    /// somewhere a person edits, not inside YAML); after `load` it holds
    /// the file's text. Required for the caption stage. The prompt is the
    /// schema: the model answers with the property names it names.
    pub prompt: Option<String>,
    /// Provider-side request fields for a captioning endpoint, sent VERBATIM
    /// as the `extra_body` form field; the door merges them into the
    /// provider's request body untouched (m3rc, 2026-09-05). Qwen's thinking
    /// switch lives here — `chat_template_kwargs: {enable_thinking: false}` —
    /// and so do `max_tokens` / `temperature`. Pan has no opinion about the
    /// contents and the door has none either. Absent = nothing sent.
    pub extra_body: Option<serde_json::Value>,
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
    /// `Authorization` header value for `url` (e.g. `Bearer …`), when the
    /// endpoint is a node reached directly rather than the door. Absent =
    /// no header.
    pub auth: Option<String>,
    /// Where this stage goes while its primary is unreachable (connection
    /// refused / reset / timeout / `503 backend_down`): the same model behind
    /// a different address — a Salad node called directly when the Iris door
    /// is down. Used ONLY during a primary hold; the primary is probed again
    /// when the hold expires. Rob, 2026-09-05: the door stays primary because
    /// it balances the two nodes; the direct node is what Pan runs on when
    /// the door is down.
    pub fallback: Option<Fallback>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Fallback {
    pub url: String,
    pub auth: Option<String>,
}

/// One address a stage's calls go to: a URL and, optionally, the
/// `Authorization` header it wants.
#[derive(Debug, Clone)]
pub struct Target {
    pub url: String,
    pub auth: Option<String>,
    /// `"primary"` or `"fallback"` — for the log line only.
    pub via: &'static str,
}

impl ModelEndpoint {
    pub fn primary(&self) -> Target {
        Target { url: self.url.clone(), auth: self.auth.clone(), via: "primary" }
    }
    pub fn fallback_target(&self) -> Option<Target> {
        self.fallback.as_ref().map(|f| Target { url: f.url.clone(), auth: f.auth.clone(), via: "fallback" })
    }
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
    backfill_since: Option<String>,
    /// How many days of model-call log files to keep under
    /// `<config dir>/logs/calls/`. Absent = 30.
    log_keep_days: Option<u32>,
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
    /// The backfill floor: images created BEFORE this (RFC 3339, local offset,
    /// same shape as git-lex:dateCreated) are never handed to a stage. Newest
    /// first still applies above it. Absent = no floor, walk everything.
    /// (Rob, 2026-09-05: a reasonable floor is mine to pick; picked midnight
    /// of the day the remote stages first came on.)
    pub backfill_since: Option<String>,
    /// Retention of the model-call log, in days (goodlux, 2026-09-07: a
    /// setting, default one month). Files older than this under
    /// `<config dir>/logs/calls/` are removed at start and once a day.
    pub log_keep_days: u32,
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

/// How many leading characters of the store id name its folder on the media
/// volume. The id itself (graph, wire, `pan:Store`) is always the full hash;
/// only the folder is short, because a person reads it in `ls`. 6, Rob's pick
/// (2026-09-04).
pub const MEDIA_FOLDER_CHARS: usize = 6;

/// Pan's folder inside a soul's directory on the media volume:
/// `<volume>/<6-char id>/pan/`. The soul comes first — that directory is
/// everything the soul keeps on the drive, the way its repo root is everything
/// it keeps in git — and Pan is one room in it, a sibling for any other tool.
/// No dot: a data drive has no documents to hide it from, and whoever opens the
/// drive should see it (Rob, 2026-09-05).
pub const MEDIA_DIR_ON_VOLUME: &str = "pan";

/// The folder name on the media volume for one store id.
pub fn media_folder_name(store_id: &str) -> String {
    store_id.chars().take(MEDIA_FOLDER_CHARS).collect()
}

impl DaemonConfig {
    /// The media root for one store under this config, or None for the pocket
    /// default: `<media_volume>/<first 6 chars of the id>/pan`. The full path
    /// is declared in the store's graph as `pan:mediaRoot`; nothing reads it by
    /// convention.
    pub fn media_root_for(&self, store_id: &str) -> Option<PathBuf> {
        self.media_volume.as_ref().map(|v| v.join(media_folder_name(store_id)).join(MEDIA_DIR_ON_VOLUME))
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
        if yml.log_keep_days == Some(0) {
            return Err(anyhow!("{}: log_keep_days must be at least 1 (it would delete today's log)", path.display()));
        }
        let mut stores: Vec<PathBuf> = yml.stores.iter().map(|p| expand_home(p)).collect();
        if stores.is_empty() {
            stores.push(default_store_dir());
        }
        let mut models = yml.models;
        for (stage, m) in models.iter_mut() {
            if m.url.is_empty() || m.model.is_empty() {
                return Err(anyhow!("{}: every model needs both url and model", path.display()));
            }
            if m.concurrency == 0 {
                return Err(anyhow!("{}: model concurrency must be at least 1", path.display()));
            }
            if let Some(name) = m.prompt.take() {
                let file = path.parent().unwrap_or(Path::new(".")).join("prompts").join(name.trim());
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("{}: stage {stage} names prompt file {} which cannot be read", path.display(), file.display()))?;
                if text.trim().is_empty() {
                    return Err(anyhow!("{}: prompt file {} is empty", path.display(), file.display()));
                }
                m.prompt = Some(text);
            }
        }
        Ok(DaemonConfig {
            path: path.to_path_buf(),
            stores,
            default: yml.default.map(|p| expand_home(&p)),
            media_volume: yml.media_volume.map(|p| expand_home(&p)),
            bind: DEFAULT_BIND.to_string(),
            port: yml.port.unwrap_or(DEFAULT_PORT),
            models,
            interval_secs: yml.interval_secs.unwrap_or(5),
            batch: yml.batch.unwrap_or(8),
            backfill_since: yml.backfill_since.filter(|s| !s.trim().is_empty()),
            log_keep_days: yml.log_keep_days.unwrap_or(super::calllog::DEFAULT_KEEP_DAYS),
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
    fn media_folder_is_the_short_prefix_of_the_full_id() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(&p, "media_volume: /Volumes/p02\n").unwrap();
        let cfg = DaemonConfig::load_from(&p).unwrap();
        let root = cfg.media_root_for("700c5bd4a969723107c1b92b83c0f1ec1497d9d4").unwrap();
        assert_eq!(root, PathBuf::from("/Volumes/p02/700c5b/pan"));
        // The all-zeros bare store id shortens the same way.
        assert_eq!(media_folder_name("0000000000000000000000000000000000000000"), "000000");
    }

    #[test]
    fn log_keep_days_defaults_to_a_month_and_refuses_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        assert_eq!(DaemonConfig::load_from(&p).unwrap().log_keep_days, 30, "missing file");
        std::fs::write(&p, "port: 7401\n").unwrap();
        assert_eq!(DaemonConfig::load_from(&p).unwrap().log_keep_days, 30, "missing key");
        std::fs::write(&p, "log_keep_days: 7\n").unwrap();
        assert_eq!(DaemonConfig::load_from(&p).unwrap().log_keep_days, 7);
        std::fs::write(&p, "log_keep_days: 0\n").unwrap();
        assert!(DaemonConfig::load_from(&p).is_err(), "zero would delete today's file");
    }

    #[test]
    fn unknown_key_is_loud() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(&p, "stroes:\n  - /x\n").unwrap();
        assert!(DaemonConfig::load_from(&p).is_err());
    }

    #[test]
    fn fallback_and_auth_parse_and_become_targets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "models:\n  pose:\n    url: http://door/percept/pose\n    model: rtmw\n    fallback:\n      url: https://node.example/pose\n      auth: Bearer abc\n",
        )
        .unwrap();
        let c = DaemonConfig::load_from(&p).unwrap();
        let ep = &c.models["pose"];
        let prim = ep.primary();
        assert_eq!((prim.url.as_str(), prim.auth.as_deref(), prim.via), ("http://door/percept/pose", None, "primary"));
        let fb = ep.fallback_target().unwrap();
        assert_eq!((fb.url.as_str(), fb.auth.as_deref(), fb.via), ("https://node.example/pose", Some("Bearer abc"), "fallback"));
        // Without a fallback there is no fallback target — the stage waits.
        std::fs::write(&p, "models:\n  pose:\n    url: http://door/percept/pose\n    model: rtmw\n").unwrap();
        assert!(DaemonConfig::load_from(&p).unwrap().models["pose"].fallback_target().is_none());
    }
}
