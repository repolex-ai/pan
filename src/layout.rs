//! Pan on-disk layout — the SINGLE authority for where every root lives.
//!
//! Carried from Pool's hard-won `.pool` discipline (pool/src/layout.rs): **hot
//! small metadata (the graph + the search index) is pinned local and never
//! moves; only heavy media relocates.** There is exactly ONE override knob
//! (`storage_root` in pan.yml), in ONE place, for ONE thing (media). Every
//! root resolves here and nowhere else — no consumer derives a path itself.
//!
//! ```text
//! <root>/                       the store home (standalone: the configured dir;
//!   │                           git-lex mode: <repo>/.pan)
//!   ├── pan.yml                 optional config
//!   ├── oxigraph/               RDF graph store — ALWAYS here, NEVER relocated
//!   ├── hnsw/                   vector index (index.usearch + keymap.json) — ALWAYS here
//!   └── storage/                media root — DEFAULT here; the ONLY overridable
//!       ├── media/image/YYYY/MM/DD/<stem>.png
//!       └── vectors/<index>/... raw per-object .npy sidecars
//! ```

use serde::Serialize;
use std::path::{Path, PathBuf};

/// The fully-resolved set of on-disk roots for one Pan store.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PanLayout {
    /// The store home. Standalone: the configured root dir. Never overridable.
    pub root: PathBuf,
    /// RDF graph store. ALWAYS `<root>/oxigraph`. Small, hot, local.
    pub oxigraph_root: PathBuf,
    /// Assembled vector index (usearch). ALWAYS `<root>/hnsw`. Never relocated.
    pub hnsw_root: PathBuf,
    /// Media root — media files + raw vector sidecars. DEFAULT `<root>/storage`; the
    /// ONE overridable location (pan.yml `storage_root:`, absolute wins,
    /// relative joins root). Points at a big external volume when needed.
    pub storage_root: PathBuf,
    /// Media tree under storage_root: `<storage_root>/media/image`.
    pub media_root: PathBuf,
    /// Backward-compatible alias for `media_root`.
    pub blob_root: PathBuf,
    /// Raw vector sidecars (`.npy`) under storage_root: `<storage_root>/vectors`.
    pub vectors_root: PathBuf,
}

impl PanLayout {
    pub const OXIGRAPH_SUBDIR: &'static str = "oxigraph";
    pub const HNSW_SUBDIR: &'static str = "hnsw";
    pub const STORAGE_SUBDIR: &'static str = "storage";
    pub const MEDIA_SUBPATH: &'static str = "media/image";
    pub const BLOB_SUBPATH: &'static str = "media/image"; // updated from blob/image
    pub const VECTORS_SUBDIR: &'static str = "vectors";

    /// Resolve every root from the store home. `storage_root_override` comes
    /// from pan.yml (absolute wins; relative joins root; None → `<root>/storage`).
    pub fn resolve(root: &Path, storage_root_override: Option<&Path>) -> Self {
        let storage_root = match storage_root_override {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => root.join(p),
            None => root.join(Self::STORAGE_SUBDIR),
        };
        let media_root = storage_root.join(Self::MEDIA_SUBPATH);
        PanLayout {
            root: root.to_path_buf(),
            oxigraph_root: root.join(Self::OXIGRAPH_SUBDIR),
            hnsw_root: root.join(Self::HNSW_SUBDIR),
            media_root: media_root.clone(),
            blob_root: media_root,
            vectors_root: storage_root.join(Self::VECTORS_SUBDIR),
            storage_root,
        }
    }

    /// The raw `.npy` sidecar path for a stem in a named index:
    /// `<vectors_root>/<index>/<shard>/<stem>.npy` or `<vectors_root>/<index>/<stem>.npy`.
    pub fn vector_sidecar_path(&self, index_name: &str, stem: &str) -> PathBuf {
        self.vectors_root.join(index_name).join(format!("{stem}.npy"))
    }

    /// Store-RELATIVE path of an enricher's data file:
    /// `<kind>/YYYY/MM/DD/<stem>[.<variant>].xml`.
    pub fn enrichment_rel_path(kind: &str, shard: &str, stem: &str, variant: Option<&str>) -> String {
        match variant {
            Some(v) => format!("{kind}/{shard}/{stem}.{v}.xml"),
            None => format!("{kind}/{shard}/{stem}.xml"),
        }
    }

    /// Absolute path for a store-relative path under the storage root.
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.storage_root.join(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults() {
        let l = PanLayout::resolve(Path::new("/x/store"), None);
        assert_eq!(l.root, PathBuf::from("/x/store"));
        assert_eq!(l.oxigraph_root, PathBuf::from("/x/store/oxigraph"));
        assert_eq!(l.hnsw_root, PathBuf::from("/x/store/hnsw"));
        assert_eq!(l.storage_root, PathBuf::from("/x/store/storage"));
        assert_eq!(l.media_root, PathBuf::from("/x/store/storage/media/image"));
        assert_eq!(l.vectors_root, PathBuf::from("/x/store/storage/vectors"));
    }

    #[test]
    fn storage_root_override_relocates_media_but_not_metadata() {
        let l = PanLayout::resolve(Path::new("/x/store"), Some(Path::new("/Volumes/big/pan")));
        assert_eq!(l.root, PathBuf::from("/x/store"));
        assert_eq!(l.oxigraph_root, PathBuf::from("/x/store/oxigraph"));
        assert_eq!(l.hnsw_root, PathBuf::from("/x/store/hnsw"));
        assert_eq!(l.storage_root, PathBuf::from("/Volumes/big/pan"));
        assert_eq!(l.media_root, PathBuf::from("/Volumes/big/pan/media/image"));
        assert_eq!(l.vectors_root, PathBuf::from("/Volumes/big/pan/vectors"));
    }

    #[test]
    fn relative_storage_root_joins_home() {
        let l = PanLayout::resolve(Path::new("/x/store"), Some(Path::new("media-data")));
        assert_eq!(l.storage_root, PathBuf::from("/x/store/media-data"));
        assert_eq!(l.media_root, PathBuf::from("/x/store/media-data/media/image"));
    }
}
