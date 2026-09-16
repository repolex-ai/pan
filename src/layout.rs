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
//!   │                              <volume>/<6-char store id>/pan instead — big
//!   │                              media off the system drive while the graph
//!   │                              stays put; the soul's folder first, Pan's
//!   │                              room inside it (Rob, 2026-09-05)
//!   └── <kind>/                    image | video | audio — the media type first
//!       ├── source/YYYY/MM/DD/<stem>.png     the stored bytes, the thing itself
//!       ├── thumbnail/YYYY/MM/DD/<stem>.jpg  everything below is DERIVED from source
//!       ├── vectors/<model>/<id>.npy (+ .json)
//!       ├── caption/YYYY/MM/DD/<id>.<model>.xml
//!       ├── pose/YYYY/MM/DD/<id>.xml (+ <id>.<model>.png overlay)
//!       └── sam3/YYYY/MM/DD/<id>.xml
//! ```
//!
//! Type first, `source` for the bytes, derived folders beside it (Rob,
//! 2026-09-05): five siblings where one was the thing and four were about it
//! told nobody whose they were once a second media type arrived. The search
//! index (`hnsw/`) stays at the top: one per model, across every type.
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
    pub const SOURCE_SUBDIR: &'static str = "source";
    pub const THUMBNAIL_SUBDIR: &'static str = "thumbnail";
    pub const VECTORS_SUBDIR: &'static str = "vectors";

    /// The top folder for a media type: `image`, `video`, `audio` — from the
    /// media type's major part; anything else lands under `other`.
    pub fn media_kind(media_type: &str) -> &'static str {
        match media_type.split('/').next().unwrap_or("") {
            "image" => "image",
            "video" => "video",
            "audio" => "audio",
            _ => "other",
        }
    }

    /// The media kind a stored path belongs to: its first segment.
    pub fn kind_of_path(rel: &str) -> &str {
        rel.split('/').next().unwrap_or("other")
    }

    /// Where a derived file of `kind` (thumbnail, caption, pose, …) for a
    /// media object of `media_kind` lives: `<media_kind>/<kind>/<tail>`.
    pub fn derived_rel_path(media_kind: &str, kind: &str, tail: &str) -> String {
        format!("{media_kind}/{kind}/{tail}")
    }

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

    /// The file stem, Pool's shape with Pan's identity — the date and time
    /// plus the pan id, never the cid (Rob, 2026-09-04): `YYYYMMDD-HHMMSS-<id>`,
    /// the time being `git-lex:createdDate` in system local time, as every Pan date.
    /// Readers never parse it back — `pan:mediaPath` in the graph is the path.
    pub fn file_stem(created_date: &str, id: &str) -> String {
        // created_date is RFC 3339 with offset: 2026-09-04T03:49:53-07:00
        let digits: String = created_date.chars().take(19).filter(|c| c.is_ascii_digit()).collect();
        let (d, t) = digits.split_at(digits.len().min(8));
        format!("{d}-{t}-{id}")
    }

    /// Media-root-relative path of the media bytes:
    /// `<kind>/source/YYYY/MM/DD/YYYYMMDD-HHMMSS-<id>.<ext>`.
    pub fn media_rel_path(media_kind: &str, shard: &str, stem: &str, ext: &str) -> String {
        Self::derived_rel_path(media_kind, Self::SOURCE_SUBDIR, &format!("{shard}/{stem}.{ext}"))
    }

    /// Media-root-relative path of the thumbnail, same stem as the media.
    pub fn thumbnail_rel_path(media_kind: &str, shard: &str, stem: &str) -> String {
        Self::derived_rel_path(media_kind, Self::THUMBNAIL_SUBDIR, &format!("{shard}/{stem}.jpg"))
    }

    /// Media-root-relative path of a vector sidecar: `<kind>/vectors/<index>/<id>.npy`.
    pub fn vector_rel_path(media_kind: &str, index_name: &str, id: &str) -> String {
        Self::derived_rel_path(media_kind, Self::VECTORS_SUBDIR, &format!("{index_name}/{id}.npy"))
    }

    /// Absolute path of a vector sidecar.
    pub fn vector_sidecar_path(&self, media_kind: &str, index_name: &str, id: &str) -> PathBuf {
        self.media_root.join(Self::vector_rel_path(media_kind, index_name, id))
    }

    /// Media-root-relative path of an enricher's data file:
    /// `<media kind>/<kind>/YYYY/MM/DD/<id>[.<variant>].xml`.
    pub fn enrichment_rel_path(media_kind: &str, kind: &str, shard: &str, id: &str, variant: Option<&str>) -> String {
        let file = match variant {
            Some(v) => format!("{shard}/{id}.{v}.xml"),
            None => format!("{shard}/{id}.xml"),
        };
        Self::derived_rel_path(media_kind, kind, &file)
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
        let stem = PanLayout::file_stem("2026-09-04T03:49:53-07:00", "k7m2p9x4");
        assert_eq!(stem, "20260904-034953-k7m2p9x4", "Pool's shape, Pan's id, local time");
        assert_eq!(PanLayout::media_rel_path("image", "2026/09/04", &stem, "png"), "image/source/2026/09/04/20260904-034953-k7m2p9x4.png");
        assert_eq!(PanLayout::thumbnail_rel_path("image", "2026/09/04", &stem), "image/thumbnail/2026/09/04/20260904-034953-k7m2p9x4.jpg");
        assert_eq!(PanLayout::vector_rel_path("image", "m", "k7m2p9x4"), "image/vectors/m/k7m2p9x4.npy");
        assert_eq!(PanLayout::media_rel_path("image", "2026/09/04", &stem, "png"), "image/source/2026/09/04/20260904-034953-k7m2p9x4.png");
        assert_eq!(PanLayout::enrichment_rel_path("image", "caption", "2026/09/04", "k7m2p9x4", Some("m")), "image/caption/2026/09/04/k7m2p9x4.m.xml");
        assert_eq!(PanLayout::media_kind("video/mp4"), "video");
    }
}
