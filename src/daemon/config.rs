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
    /// THE MODEL'S NAME — one name, written by a person, used everywhere Pan
    /// says which model did something: `pan:model` in the graph and in the
    /// file, the file name, the log line (goodlux, 2026-09-18). Lowercase
    /// letters, digits and single dashes; no periods, no spaces, no slashes.
    pub model: String,
    /// The instruction sent with the image to a captioning endpoint. In the
    /// config file this is the NAME of a plain-text file (goodlux,
    /// 2026-09-08: the prompt text lives somewhere a person edits, not inside
    /// YAML); after `load` it holds the file's text. Required for the caption
    /// stage. The prompt is the schema: the model answers with the property
    /// names it names.
    ///
    /// One flat folder holds every prompt, `~/.config/pan/prompts/`. A name
    /// ending `.default.md` is one Pan ships and rewrites when it ships a new
    /// version; every other name is yours and Pan never writes it. The config
    /// names the file and pand reads that one and no other (goodlux,
    /// 2026-09-19).
    pub prompt: Option<String>,
    /// Which prompt file was read, its file name exactly as the config gave
    /// it. Not
    /// config: `load` fills it in, and the caption stage records it on the
    /// object and on the Caption record, so an image says which prompt
    /// described it. A prompt that changes gets a new file name; Pan does not
    /// read the old one back.
    #[serde(skip)]
    pub prompt_path: Option<String>,
    /// Nouns this stage always asks for, whatever the caption model said
    /// (goodlux, 2026-09-19). The segmentation stage grounds the nouns the
    /// caption listed; a caption that never says "person" left a photograph of
    /// people with no person region. These are added to every call, so person
    /// and face are found because Pan asked, not because a caption happened to
    /// mention them. Comma-separated in the config: `always: person, face`.
    #[serde(default, deserialize_with = "comma_or_list")]
    pub always: Vec<String>,
    /// Provider-side request fields for a captioning endpoint, sent VERBATIM
    /// as the `extra_body` form field; Iris merges them into the
    /// provider's request body untouched (m3rc, 2026-09-05). Qwen's thinking
    /// switch lives here — `chat_template_kwargs: {enable_thinking: false}` —
    /// and so do `max_tokens` / `temperature`. Pan has no opinion about the
    /// contents and Iris has none either. Absent = nothing sent.
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
    /// endpoint is a node reached directly rather than Iris. Absent =
    /// no header.
    pub auth: Option<String>,
    /// Where this stage goes while its primary is unreachable (connection
    /// refused / reset / timeout / `503 backend_down`): the same model behind
    /// a different address — a Salad node called directly when Iris
    /// is down. Used ONLY during a primary hold; the primary is probed again
    /// when the hold expires. Rob, 2026-09-05: Iris stays primary because
    /// it balances the two nodes; the direct node is what Pan runs on when
    /// Iris is down.
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
        Target {
            url: self.url.clone(),
            auth: self.auth.clone(),
            via: "primary",
        }
    }
    pub fn fallback_target(&self) -> Option<Target> {
        self.fallback.as_ref().map(|f| Target {
            url: f.url.clone(),
            auth: f.auth.clone(),
            via: "fallback",
        })
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
    /// same shape as pan:createdDate) are never handed to a stage. Newest
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

/// Pan's folder inside a soul's directory on the media volume:
/// `<volume>/<6-char id>/pan/`. The soul comes first — that directory is
/// everything the soul keeps on the drive, the way its repo root is everything
/// it keeps in git — and Pan is one room in it, a sibling for any other tool.
/// No dot: a data drive has no documents to hide it from, and whoever opens the
/// drive should see it (Rob, 2026-09-05).
pub const MEDIA_DIR_ON_VOLUME: &str = "pan";

/// `always: person, face` and `always: [person, face]` both mean the same two
/// nouns. One line of YAML either way; nobody should have to remember which.
fn comma_or_list<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    let raw = Option::<OneOrMany>::deserialize(d)?;
    let items = match raw {
        None => Vec::new(),
        Some(OneOrMany::One(s)) => s.split(',').map(str::to_string).collect(),
        Some(OneOrMany::Many(v)) => v,
    };
    Ok(items
        .into_iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect())
}

/// The prompts this binary ships. One flat folder holds every prompt; a
/// shipped one is marked by its name, `<what it does>.default.md`, and Pan
/// rewrites it only when the text it ships differs from what is on disk — so a
/// new version of pand carrying a new prompt replaces it and an ordinary start
/// touches nothing (goodlux, 2026-09-19). Pan writes no other file in that
/// folder: your own prompt is any name without `.default`, and it is yours.
pub const SHIPPED_PROMPTS: [(&str, &str); 1] = [(
    "full-caption.default.md",
    include_str!("../../prompts/full-caption.default.md"),
)];

/// Put the shipped prompts in the prompts folder, writing only the ones whose
/// text has changed. Touches nothing else in there.
pub fn install_default_prompts(prompts_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(prompts_dir)
        .with_context(|| format!("create {}", prompts_dir.display()))?;
    for (name, text) in SHIPPED_PROMPTS {
        let file = prompts_dir.join(name);
        let same = std::fs::read_to_string(&file).is_ok_and(|on_disk| on_disk == text);
        if !same {
            crate::write_atomic(&file, text.as_bytes())
                .with_context(|| format!("write {}", file.display()))?;
        }
    }
    Ok(())
}

/// The file a `prompt:` line names, in the prompts directory.
///
/// The config says which prompt a stage uses by its file name:
/// `full-caption.default.md` for the one Pan ships, or any name of your own.
/// There is no searching and no preference order — the line names the file,
/// the file is read, and a line that names nothing readable stops pand
/// (goodlux, 2026-09-19).
pub fn resolve_prompt(prompts_dir: &Path, name: &str) -> (String, PathBuf) {
    (name.to_string(), prompts_dir.join(name))
}

/// A model's name, which has to survive being a file name: lowercase letters,
/// digits and single dashes between them. No periods, no spaces, no slashes,
/// no underscores, and never empty (goodlux, 2026-09-18).
pub fn check_model_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("a model name cannot be empty".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("a model name cannot start or end with a dash".to_string());
    }
    if name.contains("--") {
        return Err("a model name has one dash at a time, not two".to_string());
    }
    for c in name.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if !ok {
            return Err(format!(
                "{c:?} is not allowed: lowercase letters, digits and dashes only"
            ));
        }
    }
    Ok(())
}

/// The folder name on the media volume for one store id — the id itself.
/// The id is six characters everywhere (goodlux, 2026-09-18; see
/// `registry::STORE_ID_LEN`), so the folder a person reads in `ls` and the id
/// in the routes and in `<pan/Store/…>` are the same six characters. This
/// function used to cut a long id down to six for the folder alone, which is
/// how a forty-character identifier lived in the graph while the disk looked
/// right.
pub fn media_folder_name(store_id: &str) -> String {
    store_id.to_string()
}

impl DaemonConfig {
    /// The media root for one store under this config, or None for the pocket
    /// default: `<media_volume>/<id>/pan`. The full path
    /// is declared in the store's graph as `pan:mediaRoot`; nothing reads it by
    /// convention.
    pub fn media_root_for(&self, store_id: &str) -> Option<PathBuf> {
        self.media_volume.as_ref().map(|v| {
            v.join(media_folder_name(store_id))
                .join(MEDIA_DIR_ON_VOLUME)
        })
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&config_dir().join("config.yml"))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let yml: ConfigYml = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            serde_yaml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?
        } else {
            ConfigYml::default()
        };
        if yml.log_keep_days == Some(0) {
            return Err(anyhow!(
                "{}: log_keep_days must be at least 1 (it would delete today's log)",
                path.display()
            ));
        }
        let mut stores: Vec<PathBuf> = yml.stores.iter().map(|p| expand_home(p)).collect();
        if stores.is_empty() {
            stores.push(default_store_dir());
        }
        let mut models = yml.models;
        // The shipped prompts land before anything reads one, so a fresh
        // machine works with no setup. Unchanged ones are left alone.
        let prompts_dir = path.parent().unwrap_or(Path::new(".")).join("prompts");
        install_default_prompts(&prompts_dir)?;
        for (stage, m) in models.iter_mut() {
            if m.url.is_empty() {
                return Err(anyhow!("{}: every model needs a url", path.display()));
            }
            // A captioning stage without a prompt is a config that cannot
            // work, and pand says so at start rather than failing one image
            // at a time (goodlux, 2026-09-19). No default is substituted.
            if stage == "caption" && m.prompt.as_deref().is_none_or(|p| p.trim().is_empty()) {
                return Err(anyhow!(
                    "{}: the caption stage needs a `prompt:` naming a file in {}, for example `prompt: full-caption.default.md`",
                    path.display(),
                    prompts_dir.display(),
                ));
            }
            check_model_name(&m.model).map_err(|e| {
                anyhow!(
                    "{}: stage {stage} has model: {:?} — {e}",
                    path.display(),
                    m.model
                )
            })?;
            if m.concurrency == 0 {
                return Err(anyhow!(
                    "{}: model concurrency must be at least 1",
                    path.display()
                ));
            }
            if let Some(name) = m.prompt.take() {
                let prompts = path.parent().unwrap_or(Path::new(".")).join("prompts");
                let (rel, file) = resolve_prompt(&prompts, name.trim());
                let text = std::fs::read_to_string(&file).with_context(|| {
                    format!(
                        "{}: stage {stage} names prompt {}, which is not readable at {}",
                        path.display(),
                        name.trim(),
                        file.display(),
                    )
                })?;
                m.prompt_path = Some(rel);
                if text.trim().is_empty() {
                    return Err(anyhow!(
                        "{}: prompt file {} is empty",
                        path.display(),
                        file.display()
                    ));
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
            log_keep_days: yml
                .log_keep_days
                .unwrap_or(super::calllog::DEFAULT_KEEP_DAYS),
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
            "stores:\n  - /souls/a\n  - ~/.pan\ndefault: /souls/a\nport: 7402\nmodels:\n  embed:\n    url: http://127.0.0.1:1215/percept/embed\n    model: qwen-vl-2b\n    concurrency: 2\n",
        )
        .unwrap();
        let cfg = DaemonConfig::load_from(&p).unwrap();
        assert_eq!(cfg.stores.len(), 2);
        assert!(
            !cfg.stores[1].to_string_lossy().starts_with('~'),
            "home expanded"
        );
        assert_eq!(cfg.default, Some(PathBuf::from("/souls/a")));
        assert_eq!(cfg.port, 7402);
        assert_eq!(cfg.models["embed"].concurrency, 2);
        assert!(cfg.models["embed"].enabled, "enabled defaults to true");
    }

    #[test]
    fn disabled_stage_is_declared_but_not_active() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "models:\n  pose:\n    url: http://x/percept/pose\n    model: rtmw\n    enabled: false\n",
        )
        .unwrap();
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
        // The id arrives already six characters (registry::store_id cuts it
        // once, at the edge), and the folder is that id unchanged.
        let root = cfg.media_root_for("700c5b").unwrap();
        assert_eq!(root, PathBuf::from("/Volumes/p02/700c5b/pan"));
        assert_eq!(media_folder_name("000000"), "000000");
    }

    #[test]
    fn log_keep_days_defaults_to_a_month_and_refuses_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        assert_eq!(
            DaemonConfig::load_from(&p).unwrap().log_keep_days,
            30,
            "missing file"
        );
        std::fs::write(&p, "port: 7401\n").unwrap();
        assert_eq!(
            DaemonConfig::load_from(&p).unwrap().log_keep_days,
            30,
            "missing key"
        );
        std::fs::write(&p, "log_keep_days: 7\n").unwrap();
        assert_eq!(DaemonConfig::load_from(&p).unwrap().log_keep_days, 7);
        std::fs::write(&p, "log_keep_days: 0\n").unwrap();
        assert!(
            DaemonConfig::load_from(&p).is_err(),
            "zero would delete today's file"
        );
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
        assert_eq!(
            (prim.url.as_str(), prim.auth.as_deref(), prim.via),
            ("http://door/percept/pose", None, "primary")
        );
        let fb = ep.fallback_target().unwrap();
        assert_eq!(
            (fb.url.as_str(), fb.auth.as_deref(), fb.via),
            ("https://node.example/pose", Some("Bearer abc"), "fallback")
        );
        // Without a fallback there is no fallback target — the stage waits.
        std::fs::write(
            &p,
            "models:\n  pose:\n    url: http://door/percept/pose\n    model: rtmw\n",
        )
        .unwrap();
        assert!(DaemonConfig::load_from(&p).unwrap().models["pose"]
            .fallback_target()
            .is_none());
    }

    /// The name is what lands in a file name, so it may hold only what a file
    /// name may hold. The wire string `qwen/qwen3.8-27b` is exactly what must
    /// never reach one (goodlux, 2026-09-18).
    #[test]
    fn a_model_name_is_lowercase_digits_and_dashes() {
        for good in ["qwen3-8-27b", "sam3", "rtmw-x-l", "depth-anything-v2-base"] {
            assert!(check_model_name(good).is_ok(), "{good} should be allowed");
        }
        for bad in [
            "qwen3.8-27b",
            "qwen/qwen3.8-27b",
            "depth anything",
            "Depth-Anything",
            "depth_anything",
            "-sam3",
            "sam3-",
            "sam--3",
            "",
        ] {
            assert!(check_model_name(bad).is_err(), "{bad:?} should be refused");
        }
    }

    /// The old shape put the endpoint's string in `model:` and pand wrote it
    /// down as the model's name. That string is now refused outright.
    #[test]
    fn the_endpoints_own_string_is_not_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "models:\n  caption:\n    url: http://x/percept/vlm\n    model: qwen/qwen3.8-27b\n",
        )
        .unwrap();
        let e = DaemonConfig::load_from(&p).unwrap_err().to_string();
        assert!(e.contains("caption"), "{e}");
    }

    /// The prompt is named in full and nothing is searched for.
    #[test]
    fn a_caption_stage_without_a_prompt_refuses_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "models:\n  caption:\n    url: http://x/percept/vlm\n    model: qwen3-8-27b\n",
        )
        .unwrap();
        let e = DaemonConfig::load_from(&p).unwrap_err().to_string();
        assert!(e.contains("prompt"), "{e}");
    }

    #[test]
    fn a_prompt_that_is_not_there_names_the_file_it_looked_for() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "models:\n  caption:\n    url: http://x/percept/vlm\n    model: qwen3-8-27b\n    prompt: nope.md\n",
        )
        .unwrap();
        let e = format!("{:#}", DaemonConfig::load_from(&p).unwrap_err());
        assert!(e.contains("nope.md"), "{e}");
    }

    /// The shipped prompt lands, and the config's own name is what gets
    /// recorded on the caption.
    #[test]
    fn the_shipped_prompt_is_installed_and_named_as_config_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "models:\n  caption:\n    url: http://x/percept/vlm\n    model: qwen3-8-27b\n    prompt: full-caption.default.md\n",
        )
        .unwrap();
        let cfg = DaemonConfig::load_from(&p).unwrap();
        assert!(dir.path().join("prompts/full-caption.default.md").is_file());
        assert_eq!(
            cfg.models["caption"].prompt_path.as_deref(),
            Some("full-caption.default.md")
        );
        assert!(cfg.models["caption"]
            .prompt
            .as_deref()
            .unwrap()
            .contains("shortCaption"));
    }

    /// The nouns segmentation always asks for, written either way.
    #[test]
    fn always_reads_as_a_line_or_a_list() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "models:\n  sam3:\n    url: http://x/percept/segment\n    model: sam3\n    always: Person, face\n  pose:\n    url: http://x/percept/pose\n    model: rtmw-x-l\n    always: [hand]\n",
        )
        .unwrap();
        let cfg = DaemonConfig::load_from(&p).unwrap();
        assert_eq!(cfg.models["sam3"].always, vec!["person", "face"]);
        assert_eq!(cfg.models["pose"].always, vec!["hand"]);
    }

    /// One name per stage, and it is what the request says.
    #[test]
    fn the_name_is_what_the_request_says() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yml");
        std::fs::write(
            &p,
            "models:\n  caption:\n    url: http://x/percept/vlm\n    model: qwen3-8-27b\n    prompt: full-caption.default.md\n",
        )
        .unwrap();
        let cfg = DaemonConfig::load_from(&p).unwrap();
        assert_eq!(cfg.models["caption"].model, "qwen3-8-27b");
    }
}
