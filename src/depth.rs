//! The depth stage — one monocular depth map per image, handled like every
//! other enrichment (goodlux, 2026-09-16; issue #24).
//!
//! The node (Salad percept-v1.7, Depth Anything V2 Base, m3rc 2026-09-16)
//! answers `POST /percept/depth` with one JSON object: the map as a base64
//! 8-bit grayscale PNG the size of the image, the raw `min`/`max` the map was
//! normalized from, `width`/`height`, and the model / precision / provider
//! that ran. In the map BRIGHT IS NEAR: the model predicts relative inverse
//! depth, so after normalization 255 is the nearest point and 0 the farthest
//! (checked on a real image, 2026-09-17).
//!
//! On disk, beside the record file the stage engine writes:
//!   image/data/depth/YYYY/MM/DD/<id>.<model>.png   the map
//!   image/data/depth/YYYY/MM/DD/<id>.<model>.json  everything else the node said
//!   image/data/depth/YYYY/MM/DD/<id>.xml           the reference + Depth record
//!
//! The record keeps min and max on purpose: the map is normalized per image,
//! so without its range two images' maps cannot be compared.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::enrich::EnrichmentRecord;
use crate::layout::PanLayout;
use crate::{gen_pan_id, write_atomic, Pan};

/// Config key and data-file directory of the stage.
pub const STAGE: &str = "depth";

// ---- vocabulary ------------------------------------------------------------
// Declared in pan.ttl 0.4.5 (ruled by goodlux 2026-09-17; the map polarity
// comment there was corrected the same day). These are the local names as
// the ontology spells them; the record is written through the same
// enrichment path as pose/caption/regions, so it carries pan:id like every
// other node.
/// The reference predicate on the image (like `poseData`).
pub const REF_LOCAL: &str = "depthData";
/// The record class (like `Pose`).
pub const CLASS: &str = "Depth";
/// Record field: path of the map PNG, relative to the media root.
pub const F_MAP_PATH: &str = "depthMapPath";
/// Record field: the raw minimum the map was normalized from.
pub const F_MIN: &str = "depthMin";
/// Record field: the raw maximum the map was normalized from.
pub const F_MAX: &str = "depthMax";

/// The node's answer, as it came. Unknown fields are kept, never dropped.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DepthAnswer {
    #[serde(default)]
    pub depth_png_b64: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub precision: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl DepthAnswer {
    /// `200 {}` is how the node reports an internal failure (same as pose and
    /// segment): no map means nothing was estimated, not "no depth".
    pub fn is_empty(&self) -> bool {
        self.depth_png_b64.trim().is_empty()
    }

    /// The map, decoded and checked to be a PNG.
    pub fn map_png(&self) -> Result<Vec<u8>> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(self.depth_png_b64.trim())
            .context("decode depth map base64")?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(anyhow!("depth map is not a PNG ({} bytes)", bytes.len()));
        }
        Ok(bytes)
    }

    /// Everything the node said except the map bytes, which live in their
    /// own file: the sidecar JSON written beside the map.
    pub fn sidecar(&self, map_rel: &str) -> serde_json::Value {
        let mut m = self.extra.clone();
        let mut put = |k: &str, v: serde_json::Value| {
            if !v.is_null() {
                m.insert(k.to_string(), v);
            }
        };
        put("min", self.min.into());
        put("max", self.max.into());
        put("width", self.width.into());
        put("height", self.height.into());
        put("model", self.model.clone().into());
        put("precision", self.precision.clone().into());
        put("provider", self.provider.clone().into());
        put("depth_png", serde_json::Value::String(map_rel.to_string()));
        serde_json::Value::Object(m)
    }
}

/// Format a decimal the way the graph wants it: plain digits. Rust's `f64`
/// Display never writes an exponent, so `-1.2637` stays `-1.2637`.
fn dec(v: f64) -> String {
    format!("{v}")
}

impl Pan {
    /// Record one depth run for an image: the map PNG and the node's sidecar
    /// under `image/data/depth/`, one Depth record hung off a fresh reference,
    /// graph and XMP refreshed. Returns the record file's media-root-relative
    /// path. An answer with no map is refused here so the image stays pending.
    pub fn write_depth(&self, id: &str, model: &str, answer: &DepthAnswer) -> Result<String> {
        if answer.is_empty() {
            return Err(anyhow!("depth answer carries no map"));
        }
        let (Some(min), Some(max)) = (answer.min, answer.max) else {
            return Err(anyhow!(
                "depth answer carries no min/max; the map cannot be read back without its range"
            ));
        };
        let png = answer.map_png()?;
        let created = self.created_date_of(id)?;
        let shard = created.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
        let media_kind = self.media_kind_of(id)?;
        let map_rel = PanLayout::overlay_rel_path(&media_kind, STAGE, &shard, id, model);
        let map_abs = self.layout.abs(&map_rel);
        if let Some(p) = map_abs.parent() {
            std::fs::create_dir_all(p).context("create depth dir")?;
        }
        write_atomic(&map_abs, &png).with_context(|| format!("write {}", map_abs.display()))?;
        // The node's own answer, whole, beside the map. Named on the
        // reference as pan:modelReplyPath (goodlux, 2026-09-19).
        let answer_rel = format!(
            "{}.json",
            map_rel.strip_suffix(".png").unwrap_or(map_rel.as_str())
        );
        let side = self.layout.abs(&answer_rel);
        write_atomic(
            &side,
            serde_json::to_string_pretty(&answer.sidecar(&map_rel))?.as_bytes(),
        )
        .with_context(|| format!("write {}", side.display()))?;

        let mut rec = EnrichmentRecord::new(gen_pan_id(), CLASS, model)
            .field(F_MAP_PATH, &map_rel)
            .field(F_MIN, dec(min))
            .field(F_MAX, dec(max));
        if let Some(w) = answer.width {
            rec = rec.field("width", w.to_string());
        }
        if let Some(h) = answer.height {
            rec = rec.field("height", h.to_string());
        }
        if let Some(p) = answer.precision.as_deref().filter(|s| !s.trim().is_empty()) {
            rec = rec.field("precision", p);
        }
        if let Some(p) = answer.provider.as_deref().filter(|s| !s.trim().is_empty()) {
            rec = rec.field("provider", p);
        }
        self.write_enrichment(
            id,
            STAGE,
            REF_LOCAL,
            model,
            std::slice::from_ref(&rec),
            crate::RecordFile::default().with_model_reply(&answer_rel),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/depth/answer.json");

    #[test]
    fn the_node_answer_parses_and_the_map_is_a_png() {
        let a: DepthAnswer = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(a.min, Some(-1.2637));
        assert_eq!(a.max, Some(9.3984));
        assert_eq!((a.width, a.height), (Some(4), Some(4)));
        assert_eq!(
            a.model.as_deref(),
            Some("depth-anything/Depth-Anything-V2-Base-hf")
        );
        assert_eq!(a.precision.as_deref(), Some("fp16-cuda"));
        assert_eq!(a.provider.as_deref(), Some("salad"));
        let png = a.map_png().unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        // IHDR: 4x4, bit depth 8, colour type 0 (grayscale)
        assert_eq!(&png[16..24], &[0, 0, 0, 4, 0, 0, 0, 4]);
        assert_eq!((png[24], png[25]), (8, 0));
    }

    #[test]
    fn an_empty_body_is_a_failure_not_a_finding() {
        let a: DepthAnswer = serde_json::from_str("{}").unwrap();
        assert!(a.is_empty());
        assert!(a.map_png().is_err());
    }

    #[test]
    fn sidecar_keeps_everything_but_the_bytes() {
        let a: DepthAnswer = serde_json::from_str(FIXTURE).unwrap();
        let s = a.sidecar("image/data/depth/2026/09/17/x.m.png");
        assert!(s.get("depth_png_b64").is_none());
        assert_eq!(s["depth_png"], "image/data/depth/2026/09/17/x.m.png");
        assert_eq!(s["provider"], "salad");
        assert_eq!(s["min"], -1.2637);
    }

    #[test]
    fn decimals_never_carry_an_exponent() {
        assert_eq!(dec(-1.2637), "-1.2637");
        assert_eq!(dec(9.3984), "9.3984");
        assert_eq!(dec(1e-7), "0.0000001");
        assert_eq!(dec(0.0), "0");
    }
}
