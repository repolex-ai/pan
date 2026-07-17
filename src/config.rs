//! Pan configuration — ONE optional file, `<root>/pan.yml`.
//!
//! Config you cannot get wrong: every field has a sensible single-user default,
//! and a missing file means "all defaults." The two Mode-1 settings from the
//! spec are the root dir (where this file lives) and the `storage_id`.
//!
//! Detectors are `{role → endpoint URL}`, every entry optional — Pan ships zero
//! models and works with none configured (graph-only query mode). Per-role
//! endpoints (not one multi-endpoint) so a user can mix providers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const PAN_NS: &str = "https://repolex.ai/ontology/pan/";

/// The a-box (instance) base. Subtexture-wide convention (Rob, Day-50):
/// `https://repolex.ai/<application>/<Class>/<instanceId>` — application slug,
/// then the CAPITALIZED class from the application's kit ontology, then the
/// id. Pan is not special: `https://repolex.ai/pan/Image/k7m2p9x4`, with
/// `Image` = `pan:Image` from ontology/pan.ttl. A SEPARATE constant from
/// `PAN_NS` (vocabulary) — different lifecycles. The IRI is stable identity;
/// a future resolver (Syrinx) can dereference it (Cool URIs).
pub const PAN_MEDIA_NS: &str = "https://repolex.ai/pan/";

pub const DEFAULT_STORAGE_ID: &str = "default";
pub const DEFAULT_INDEX_ID: &str = "default";

/// The parsed pan.yml. All fields optional on disk; `PanConfig::load` applies
/// defaults so the rest of the code never sees an Option it doesn't want.
///
/// `deny_unknown_fields`: a mistyped key (`stroage_id:`) is a LOUD error, not a
/// silently-ignored line that leaves you wondering why your config did nothing
/// ("config you cannot get wrong" — you find out at load, not never).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PanYml {
    pub storage_id: Option<String>,
    pub storage_root: Option<PathBuf>,
    pub index_id: Option<String>,
    /// Extra namespace prefixes for facts + queries, e.g. `copia: https://…/copia/`.
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
    pub default_prefix: Option<String>,
    /// Detector role → endpoint URL. Known roles: embed, caption, pose, sam3 —
    /// but the map is open: a new role is a new entry, not a code change.
    #[serde(default)]
    pub detectors: HashMap<String, String>,
}

/// Fully-resolved runtime config.
#[derive(Debug, Clone)]
pub struct PanConfig {
    pub root: PathBuf,
    pub storage_id: String,
    pub storage_root_override: Option<PathBuf>,
    pub index_id: String,
    /// Full prefix map: `pan:` always present, plus pan.yml extras.
    pub prefixes: HashMap<String, String>,
    pub default_prefix: String,
    pub detectors: HashMap<String, String>,
}

impl PanConfig {
    /// Load `<root>/pan.yml` if present, else all defaults. LOUD on a present
    /// but unparseable file — a config typo must not silently become defaults.
    pub fn load(root: &Path) -> Result<Self> {
        let yml_path = root.join("pan.yml");
        let yml: PanYml = if yml_path.exists() {
            let raw = std::fs::read_to_string(&yml_path)
                .with_context(|| format!("read {}", yml_path.display()))?;
            serde_yaml::from_str(&raw)
                .with_context(|| format!("parse {}", yml_path.display()))?
        } else {
            PanYml::default()
        };

        let mut prefixes = yml.prefixes;
        prefixes
            .entry("pan".to_string())
            .or_insert_with(|| PAN_NS.to_string());

        Ok(PanConfig {
            root: root.to_path_buf(),
            storage_id: yml.storage_id.unwrap_or_else(|| DEFAULT_STORAGE_ID.to_string()),
            storage_root_override: yml.storage_root,
            index_id: yml.index_id.unwrap_or_else(|| DEFAULT_INDEX_ID.to_string()),
            prefixes,
            default_prefix: yml.default_prefix.unwrap_or_else(|| "pan".to_string()),
            detectors: yml.detectors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = PanConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.storage_id, "default");
        assert_eq!(cfg.index_id, "default");
        assert_eq!(cfg.prefixes.get("pan").unwrap(), PAN_NS);
        assert!(cfg.detectors.is_empty());
        assert!(cfg.storage_root_override.is_none());
    }

    #[test]
    fn yml_fields_resolve() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pan.yml"),
            "storage_id: my-store\nstorage_root: /Volumes/big\nindex_id: clip-768\nprefixes:\n  copia: https://repolex.ai/ontology/kit/copia/\ndetectors:\n  embed: http://127.0.0.1:1215/embed\n",
        )
        .unwrap();
        let cfg = PanConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.storage_id, "my-store");
        assert_eq!(cfg.storage_root_override, Some(PathBuf::from("/Volumes/big")));
        assert_eq!(cfg.index_id, "clip-768");
        assert!(cfg.prefixes.contains_key("pan"), "pan: always registered");
        assert!(cfg.prefixes.contains_key("copia"));
        assert_eq!(cfg.detectors.get("embed").unwrap(), "http://127.0.0.1:1215/embed");
    }

    #[test]
    fn broken_yml_fails_loud() {
        // A config typo is an error, not silently-defaults.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pan.yml"), "storage_id: [unclosed").unwrap();
        assert!(PanConfig::load(dir.path()).is_err());
    }
}
