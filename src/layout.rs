//! Pan on-disk layout — the SINGLE authority for where every root lives.
//!
//! Follows the stack-wide `_ignore/` pocket law (Rob, 2026-08-05): in a tool's
//! dotdir, `_ignore/` is machine-local and gitignored; everything else is
//! committed. git-lex already manages the `.pan/_ignore/` gitignore entry.
//!
//! ```text
//! <root>/                          soul repo: <repo>/.pan   bare store: the dir itself
//!   pan.yml                        committable config (optional)
//!   _ignore/                       machine-local pocket
//!     oxigraph/                    the graph — always here, never relocated
//!     hnsw/<model>/                vector index per embedding model — always here
//!     media/                       DEFAULT media root; may live elsewhere (below)
//!
//! <media root>/                    default <root>/_ignore/media; when pand is
//!   │                              configured with a media volume it is
//!   │                              <volume>/<store id>/media instead — big media
//!   │                              off the system drive while the graph stays put
//!   ├── image/YYYY/MM/DD/<id>.png
//!   ├── thumbnail/YYYY/MM/DD/<id>.jpg
//!   ├── vectors/<model>/<id>.npy
//!   ├── caption/YYYY/MM/DD/<id>.<model>.xml
//!   ├── pose/YYYY/MM/DD/<id>.xml (+ <id>.<model>.png overlay)
//!   └── sam3/YYYY/MM/DD/<id>.xml
//! ```
//!
//! Every `pan:mediaPath` / `pan:path` in the graph is relative to the media
//! root, and the media root itself is a fact on the store's `pan:Store` node
//! (`pan:mediaRoot`) — a reader never derives it from convention.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PanLayout {
    /// The store home (`<repo>/.pan` or the bare directory).
    pub root: PathBuf,
    /// `<root>/_ignore` — the machine-local pocket.
    pub pocket: PathBuf,
    /// `<pocket>/oxigraph`. Never relocated.
    pub oxigraph_root: PathBuf,
    /// `<pocket>/hnsw`. Never relocated.
    pub hnsw_root: PathBuf,
    /// Where media and every derived file live. `<pocket>/media` by default;
    /// `<volume>/<store id>/media` when a media volume is configured.
    pub media_root: PathBuf,
}

impl PanLayout {
    pub const POCKET: &'static str = "_ignore";
    pub const OXIGRAPH_SUBDIR: &'static str = "oxigraph";
    pub const HNSW_SUBDIR: &'static str = "hnsw";
    pub const MEDIA_SUBDIR: &'static str = "media";
    pub const IMAGE_SUBDIR: &'static str = "image";
    pub const THUMBNAIL_SUBDIR: &'static str = "thumbnail";
    pub const VECTORS_SUBDIR: &'static str = "vectors";

    /// Resolve every root. `media_root_override` is the fully-resolved media
    /// root pand computed from its config (volume + store id); `None` = the
    /// pocket default.
    pub fn resolve(root: &Path, media_root_override: Option<&Path>) -> Self {
        let pocket = root.join(Self::POCKET);
        let media_root = match media_root_override {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => root.join(p),
            None => pocket.join(Self::MEDIA_SUBDIR),
        };
        PanLayout {
            root: root.to_path_buf(),
            oxigraph_root: pocket.join(Self::OXIGRAPH_SUBDIR),
            hnsw_root: pocket.join(Self::HNSW_SUBDIR),
            pocket,
            media_root,
        }
    }

    /// Media-root-relative path of the media bytes: `image/YYYY/MM/DD/<id>.<ext>`.
    pub fn media_rel_path(shard: &str, id: &str, ext: &str) -> String {
        format!("{}/{shard}/{id}.{ext}", Self::IMAGE_SUBDIR)
    }

    /// Media-root-relative path of the thumbnail.
    pub fn thumbnail_rel_path(shard: &str, id: &str) -> String {
        format!("{}/{shard}/{id}.jpg", Self::THUMBNAIL_SUBDIR)
    }

    /// Media-root-relative path of a vector sidecar: `vectors/<index>/<id>.npy`.
    pub fn vector_rel_path(index_name: &str, id: &str) -> String {
        format!("{}/{index_name}/{id}.npy", Self::VECTORS_SUBDIR)
    }

    /// Absolute path of a vector sidecar.
    pub fn vector_sidecar_path(&self, index_name: &str, id: &str) -> PathBuf {
        self.media_root.join(Self::vector_rel_path(index_name, id))
    }

    /// Media-root-relative path of an enricher's data file:
    /// `<kind>/YYYY/MM/DD/<id>[.<variant>].xml`.
    pub fn enrichment_rel_path(kind: &str, shard: &str, id: &str, variant: Option<&str>) -> String {
        match variant {
            Some(v) => format!("{kind}/{shard}/{id}.{v}.xml"),
            None => format!("{kind}/{shard}/{id}.xml"),
        }
    }

    /// Absolute path for a media-root-relative path.
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.media_root.join(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_into_the_pocket() {
        let l = PanLayout::resolve(Path::new("/soul/.pan"), None);
        assert_eq!(l.pocket, PathBuf::from("/soul/.pan/_ignore"));
        assert_eq!(l.oxigraph_root, PathBuf::from("/soul/.pan/_ignore/oxigraph"));
        assert_eq!(l.hnsw_root, PathBuf::from("/soul/.pan/_ignore/hnsw"));
        assert_eq!(l.media_root, PathBuf::from("/soul/.pan/_ignore/media"));
    }

    #[test]
    fn media_root_override_relocates_media_but_not_the_graph() {
        let l = PanLayout::resolve(Path::new("/soul/.pan"), Some(Path::new("/Volumes/p02/_pan/abc/media")));
        assert_eq!(l.oxigraph_root, PathBuf::from("/soul/.pan/_ignore/oxigraph"));
        assert_eq!(l.media_root, PathBuf::from("/Volumes/p02/_pan/abc/media"));
        assert_eq!(l.abs("image/2026/09/04/x.png"), PathBuf::from("/Volumes/p02/_pan/abc/media/image/2026/09/04/x.png"));
    }

    #[test]
    fn relative_paths_are_declared_shapes() {
        assert_eq!(PanLayout::media_rel_path("2026/09/04", "k7m2p9x4", "png"), "image/2026/09/04/k7m2p9x4.png");
        assert_eq!(PanLayout::thumbnail_rel_path("2026/09/04", "k7m2p9x4"), "thumbnail/2026/09/04/k7m2p9x4.jpg");
        assert_eq!(PanLayout::vector_rel_path("m", "k7m2p9x4"), "vectors/m/k7m2p9x4.npy");
    }
}
