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
//!       ├── blob/image/YYYY/MM/DD/<panId>.png
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
    /// Media root — blobs + raw vector sidecars. DEFAULT `<root>/storage`; the
    /// ONE overridable location (pan.yml `storage_root:`, absolute wins,
    /// relative joins root). Points at a big external volume when needed.
    pub storage_root: PathBuf,
    /// Blob tree under storage_root: `<storage_root>/blob/image`.
    pub blob_root: PathBuf,
    /// Raw vector sidecars (`.npy`) under storage_root: `<storage_root>/vectors`.
    /// (The ASSEMBLED search index is `hnsw_root`, local; raw per-object
    /// embeddings are media-derived and follow media to the volume.)
    pub vectors_root: PathBuf,
}

impl PanLayout {
    pub const OXIGRAPH_SUBDIR: &'static str = "oxigraph";
    pub const HNSW_SUBDIR: &'static str = "hnsw";
    pub const STORAGE_SUBDIR: &'static str = "storage";
    pub const BLOB_SUBPATH: &'static str = "blob/image";
    pub const VECTORS_SUBDIR: &'static str = "vectors";

    /// Resolve every root from the store home. `storage_root_override` comes
    /// from pan.yml (absolute wins; relative joins root; None → `<root>/storage`).
    pub fn resolve(root: &Path, storage_root_override: Option<&Path>) -> Self {
        let storage_root = match storage_root_override {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => root.join(p),
            None => root.join(Self::STORAGE_SUBDIR),
        };
        PanLayout {
            root: root.to_path_buf(),
            oxigraph_root: root.join(Self::OXIGRAPH_SUBDIR),
            hnsw_root: root.join(Self::HNSW_SUBDIR),
            blob_root: storage_root.join(Self::BLOB_SUBPATH),
            vectors_root: storage_root.join(Self::VECTORS_SUBDIR),
            storage_root,
        }
    }

    /// The raw `.npy` sidecar path for a panId in a named index:
    /// `<vectors_root>/<index>/<panId>.npy`.
    pub fn vector_sidecar_path(&self, index_name: &str, pan_id: &str) -> PathBuf {
        self.vectors_root.join(index_name).join(format!("{pan_id}.npy"))
    }

    /// Store-RELATIVE path of an enricher's data file:
    /// `<kind>/YYYY/MM/DD/<panId>[.<variant>].xml`.
    ///
    /// Date-sharded like blobs for the same reason: a store holding 80k images
    /// must never put 80k files in one directory. `variant` distinguishes
    /// several files of one kind for one image — one caption file per model —
    /// and is omitted when there is only ever one.
    pub fn enrichment_rel_path(kind: &str, shard: &str, pan_id: &str, variant: Option<&str>) -> String {
        match variant {
            Some(v) => format!("{kind}/{shard}/{pan_id}.{v}.xml"),
            None => format!("{kind}/{shard}/{pan_id}.xml"),
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
        assert_eq!(l.oxigraph_root, PathBuf::from("/x/store/oxigraph"));
        assert_eq!(l.hnsw_root, PathBuf::from("/x/store/hnsw"));
        assert_eq!(l.storage_root, PathBuf::from("/x/store/storage"));
        assert_eq!(l.blob_root, PathBuf::from("/x/store/storage/blob/image"));
        assert_eq!(l.vectors_root, PathBuf::from("/x/store/storage/vectors"));
    }

    #[test]
    fn storage_root_override_relocates_media_but_not_metadata() {
        // The core property: storage_root points at a volume; oxigraph + hnsw
        // STAY local under the store home no matter what.
        let l = PanLayout::resolve(Path::new("/x/store"), Some(Path::new("/Volumes/big/pan")));
        assert_eq!(l.oxigraph_root, PathBuf::from("/x/store/oxigraph"), "oxigraph stays local");
        assert_eq!(l.hnsw_root, PathBuf::from("/x/store/hnsw"), "hnsw stays local");
        assert_eq!(l.storage_root, PathBuf::from("/Volumes/big/pan"));
        assert_eq!(l.blob_root, PathBuf::from("/Volumes/big/pan/blob/image"));
    }

    #[test]
    fn relative_storage_root_joins_home() {
        let l = PanLayout::resolve(Path::new("/x/store"), Some(Path::new("media")));
        assert_eq!(l.storage_root, PathBuf::from("/x/store/media"));
    }

    #[test]
    fn vector_sidecar_is_panid_named() {
        let l = PanLayout::resolve(Path::new("/x"), None);
        assert_eq!(
            l.vector_sidecar_path("my-index", "abcd12xy"),
            PathBuf::from("/x/storage/vectors/my-index/abcd12xy.npy")
        );
    }
}
