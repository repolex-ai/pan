//! Pan on-disk layout — the SINGLE authority for where every root lives.
//!
//! Follows the stack-wide `_ignore/` pocket law (goodlux, 2026-08-05): in a
//! tool's dotdir, `_ignore/` is machine-local and gitignored; everything else
//! is committed. git-lex already manages the `.pan/_ignore/` gitignore entry.
//!
//! ```text
//! <root>/                          soul repo: <repo>/.pan   bare store: the dir itself
//!   pan.yml                        committable config (optional)
//!   photosets/<id>.xml             one file per curated set; committed, the graph is rebuilt from them
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
//!   │                              room inside it (goodlux, 2026-09-05)
//!   └── <kind>/                    image | video | audio — the media type first
//!       ├── img/                   PIXELS: the pictures themselves and their renditions
//!       │   ├── original/YYYY/MM/DD/<stem>.<ext>       what arrived, when it was not PNG; kept, never read again
//!       │   ├── source/YYYY/MM/DD/<stem>.png           THE image: always PNG, XMP inside, what every stage reads
//!       │   ├── jpg/YYYY/MM/DD/<stem>_<longEdge>.jpg   derived JPEG renditions; the thumbnail is _512
//!       │   └── upscale/YYYY/MM/DD/<stem>_<longEdge>.png upscaled renditions, PNG like the source; reserved, nothing writes here yet
//!       └── data/                  MODEL OUTPUT: records about the picture
//!           ├── caption/YYYY/MM/DD/<id>.<model>.xml
//!           ├── pose/YYYY/MM/DD/<id>.xml (+ <id>.<model>.png overlay)
//!           ├── sam3/YYYY/MM/DD/<id>.xml
//!           └── vectors/<model>/<id>.npy (+ .json)
//! ```
//!
//! Pixels under `img/`, records under `data/` (goodlux, 2026-09-16): one glob
//! finds every picture, another every record, and neither has to know the
//! other's folder names. A derived size is named by its long edge in the file
//! name (`_512`, `_2048`), never by a role word in a folder — roles drift, a
//! number does not. An upscaled rendition sits under `img/upscale/` by the
//! same rule, `<stem>_<longEdge>.png` — an upscale stays PNG like the source
//! (goodlux, 2026-09-16). `_<edge>_sq` is reserved for a square crop, when a
//! grid needs one.
//!
//! `<model>` in a FILE NAME is the model id with every `/` turned into `-`
//! (`qwen/qwen3.8-27b` → `qwen-qwen3.8-27b`); the graph's `pan:model` keeps
//! the real id. A slash in a file name is a folder, and a model id must not
//! make one.
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
    /// `<volume>/<store id>/pan` when a media volume is configured.
    pub media_root: PathBuf,
}

impl PanLayout {
    pub const POCKET: &'static str = "_ignore";
    pub const OXIGRAPH_SUBDIR: &'static str = "oxigraph";
    pub const HNSW_SUBDIR: &'static str = "hnsw";
    pub const MEDIA_SUBDIR: &'static str = "media";
    /// `<kind>/img/` — the pictures and their renditions.
    pub const IMG_SUBDIR: &'static str = "img";
    /// `<kind>/data/` — model output about the pictures.
    pub const DATA_SUBDIR: &'static str = "data";
    pub const ORIGINAL_SUBDIR: &'static str = "original";
    pub const SOURCE_SUBDIR: &'static str = "source";
    pub const JPG_SUBDIR: &'static str = "jpg";
    /// `<kind>/img/upscale/` — upscaled renditions, PNG like the source.
    pub const UPSCALE_SUBDIR: &'static str = "upscale";
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

    /// A model id as it may appear in a file name: every `/` becomes `-`.
    /// The graph keeps the real id; only the path is flattened.
    pub fn file_safe_model(model: &str) -> String {
        model.replace('/', "-")
    }

    /// `<media_kind>/img/<sub>/<tail>` — a picture or a rendition of one.
    pub fn img_rel_path(media_kind: &str, sub: &str, tail: &str) -> String {
        format!("{media_kind}/{}/{sub}/{tail}", Self::IMG_SUBDIR)
    }

    /// `<media_kind>/data/<kind>/<tail>` — a record a stage wrote about a
    /// picture (caption, pose, sam3, vectors, …).
    pub fn data_rel_path(media_kind: &str, kind: &str, tail: &str) -> String {
        format!("{media_kind}/{}/{kind}/{tail}", Self::DATA_SUBDIR)
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

    /// The file stem: the date and time plus the pan id, never a content hash
    /// (goodlux, 2026-09-04): `YYYYMMDD-HHMMSS-<id>`, the time being
    /// `git-lex:createdDate` in system local time, as every Pan date.
    /// Readers never parse it back — `pan:mediaPath` in the graph is the path.
    pub fn file_stem(created_date: &str, id: &str) -> String {
        // created_date is RFC 3339 with offset: 2026-09-04T03:49:53-07:00
        let digits: String = created_date.chars().take(19).filter(|c| c.is_ascii_digit()).collect();
        let (d, t) = digits.split_at(digits.len().min(8));
        format!("{d}-{t}-{id}")
    }

    /// Media-root-relative path of the media bytes Pan works from:
    /// `<kind>/img/source/YYYY/MM/DD/YYYYMMDD-HHMMSS-<id>.<ext>`. For images
    /// `ext` is always `png`; other kinds keep their own extension.
    pub fn media_rel_path(media_kind: &str, shard: &str, stem: &str, ext: &str) -> String {
        Self::img_rel_path(media_kind, Self::SOURCE_SUBDIR, &format!("{shard}/{stem}.{ext}"))
    }

    /// Media-root-relative path of the bytes as they arrived, when they were
    /// not already the source format: `<kind>/img/original/YYYY/MM/DD/<stem>.<ext>`.
    /// Kept for the record; nothing reads it again.
    pub fn original_rel_path(media_kind: &str, shard: &str, stem: &str, ext: &str) -> String {
        Self::img_rel_path(media_kind, Self::ORIGINAL_SUBDIR, &format!("{shard}/{stem}.{ext}"))
    }

    /// Media-root-relative path of a derived JPEG rendition, named by its long
    /// edge: `<kind>/img/jpg/YYYY/MM/DD/<stem>_<longEdge>.jpg`.
    pub fn jpg_rel_path(media_kind: &str, shard: &str, stem: &str, long_edge: u32) -> String {
        Self::img_rel_path(media_kind, Self::JPG_SUBDIR, &format!("{shard}/{stem}_{long_edge}.jpg"))
    }

    /// Media-root-relative path of an upscaled rendition, named by its long
    /// edge and kept as PNG like the source:
    /// `<kind>/img/upscale/YYYY/MM/DD/<stem>_<longEdge>.png`. The place is
    /// reserved (goodlux, 2026-09-16); no stage writes here yet.
    pub fn upscale_rel_path(media_kind: &str, shard: &str, stem: &str, long_edge: u32) -> String {
        Self::img_rel_path(media_kind, Self::UPSCALE_SUBDIR, &format!("{shard}/{stem}_{long_edge}.png"))
    }

    /// The thumbnail is the `long_edge` JPEG rendition; no folder of its own.
    pub fn thumbnail_rel_path(media_kind: &str, shard: &str, stem: &str, long_edge: u32) -> String {
        Self::jpg_rel_path(media_kind, shard, stem, long_edge)
    }

    /// Media-root-relative path of a vector sidecar:
    /// `<kind>/data/vectors/<index>/<id>.npy` (index name flattened for the path).
    pub fn vector_rel_path(media_kind: &str, index_name: &str, id: &str) -> String {
        Self::data_rel_path(media_kind, Self::VECTORS_SUBDIR, &format!("{}/{id}.npy", Self::file_safe_model(index_name)))
    }

    /// Absolute path of a vector sidecar.
    pub fn vector_sidecar_path(&self, media_kind: &str, index_name: &str, id: &str) -> PathBuf {
        self.media_root.join(Self::vector_rel_path(media_kind, index_name, id))
    }

    /// Media-root-relative path of an enricher's data file:
    /// `<kind>/data/<stage>/YYYY/MM/DD/<id>[.<variant>].xml`. The variant is a
    /// model id and is flattened for the path.
    pub fn enrichment_rel_path(media_kind: &str, kind: &str, shard: &str, id: &str, variant: Option<&str>) -> String {
        let file = match variant {
            Some(v) => format!("{shard}/{id}.{}.xml", Self::file_safe_model(v)),
            None => format!("{shard}/{id}.xml"),
        };
        Self::data_rel_path(media_kind, kind, &file)
    }

    /// Media-root-relative path of a stage's overlay picture, beside its
    /// record: `<kind>/data/<stage>/YYYY/MM/DD/<id>.<model>.png`.
    pub fn overlay_rel_path(media_kind: &str, kind: &str, shard: &str, id: &str, model: &str) -> String {
        Self::data_rel_path(media_kind, kind, &format!("{shard}/{id}.{}.png", Self::file_safe_model(model)))
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
        let l = PanLayout::resolve(Path::new("/soul/.pan"), Some(Path::new("/Volumes/p02/_pan/abc/pan")));
        assert_eq!(l.oxigraph_root, PathBuf::from("/soul/.pan/_ignore/oxigraph"));
        assert_eq!(l.media_root, PathBuf::from("/Volumes/p02/_pan/abc/pan"));
        assert_eq!(l.abs("image/img/source/2026/09/04/x.png"), PathBuf::from("/Volumes/p02/_pan/abc/pan/image/img/source/2026/09/04/x.png"));
    }

    #[test]
    fn pixels_under_img_records_under_data() {
        let stem = PanLayout::file_stem("2026-09-04T03:49:53-07:00", "k7m2p9x4");
        assert_eq!(stem, "20260904-034953-k7m2p9x4", "date, time, pan id, local time");
        assert_eq!(PanLayout::media_rel_path("image", "2026/09/04", &stem, "png"), "image/img/source/2026/09/04/20260904-034953-k7m2p9x4.png");
        assert_eq!(PanLayout::original_rel_path("image", "2026/09/04", &stem, "jpg"), "image/img/original/2026/09/04/20260904-034953-k7m2p9x4.jpg");
        assert_eq!(PanLayout::thumbnail_rel_path("image", "2026/09/04", &stem, 512), "image/img/jpg/2026/09/04/20260904-034953-k7m2p9x4_512.jpg");
        assert_eq!(PanLayout::jpg_rel_path("image", "2026/09/04", &stem, 2048), "image/img/jpg/2026/09/04/20260904-034953-k7m2p9x4_2048.jpg");
        assert_eq!(PanLayout::upscale_rel_path("image", "2026/09/04", &stem, 4096), "image/img/upscale/2026/09/04/20260904-034953-k7m2p9x4_4096.png", "an upscale is PNG, beside jpg/, named by its long edge");
        assert_eq!(PanLayout::vector_rel_path("image", "m", "k7m2p9x4"), "image/data/vectors/m/k7m2p9x4.npy");
        assert_eq!(PanLayout::enrichment_rel_path("image", "caption", "2026/09/04", "k7m2p9x4", Some("m")), "image/data/caption/2026/09/04/k7m2p9x4.m.xml");
        assert_eq!(PanLayout::enrichment_rel_path("image", "sam3", "2026/09/04", "k7m2p9x4", None), "image/data/sam3/2026/09/04/k7m2p9x4.xml");
        assert_eq!(PanLayout::overlay_rel_path("image", "pose", "2026/09/04", "k7m2p9x4", "rtmw-x-l"), "image/data/pose/2026/09/04/k7m2p9x4.rtmw-x-l.png");
        assert_eq!(PanLayout::media_kind("video/mp4"), "video");
    }

    #[test]
    fn a_slash_in_a_model_id_never_makes_a_folder() {
        assert_eq!(PanLayout::file_safe_model("qwen/qwen3.8-27b"), "qwen-qwen3.8-27b");
        assert_eq!(
            PanLayout::enrichment_rel_path("image", "caption", "2026/09/08", "ygjjmvkw", Some("qwen/qwen3.8-27b")),
            "image/data/caption/2026/09/08/ygjjmvkw.qwen-qwen3.8-27b.xml"
        );
        assert_eq!(PanLayout::vector_rel_path("image", "org/model", "x"), "image/data/vectors/org-model/x.npy");
        assert_eq!(PanLayout::overlay_rel_path("image", "pose", "2026/09/08", "x", "a/b"), "image/data/pose/2026/09/08/x.a-b.png");
    }
}
