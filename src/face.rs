//! The face stage — every face in an image, handled like pose: one record
//! per face, all in one file, hung off one reference on the image
//! (pan.ttl 0.4.21, goodlux 2026-11-09).
//!
//! The node (katoptron, InsightFace buffalo_l on CPU, m3rc 2026-11-06)
//! answers `POST /percept/face` with one JSON object: `faces` (largest
//! first; `[]` when none), `width`/`height` of the image it saw, and the
//! model / precision / provider that ran. Each face: `bbox` `[x1,y1,x2,y2]`
//! in pixels, `score`, `kps` five `[x,y]` points (left eye, right eye, nose,
//! mouth left, mouth right), `pose` `[pitch,yaw,roll]` in degrees, and
//! `embedding`, 512 floats at unit length (ArcFace; cosine = dot product).
//! The node also guesses `age` and `sex` and returns 106-point and 68-point
//! landmark sets; those stay in the saved reply only, out of the graph.
//!
//! On disk, beside the record file the stage engine writes:
//!   image/enrichment/face/YYYY/MM/DD/<source file name>.face.<model>.json         the node's whole answer
//!   image/enrichment/face/YYYY/MM/DD/<source file name>.face.<model>.<face id>.npy one vector per face
//!   image/enrichment/face/YYYY/MM/DD/<source file name>.face.<model>.nq           the reference + Face records
//!
//! The vector also goes into a search index named after the model, keyed by
//! the Face record's id (not the image's: an image has many faces), so
//! "faces like this one" is a search like any other.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::enrich::EnrichmentRecord;
use crate::{gen_pan_id, write_atomic, Pan};

/// Config key and data-file directory of the stage.
pub const STAGE: &str = "face";

// ---- vocabulary ------------------------------------------------------------
// Declared in pan.ttl 0.4.21. Local names as the ontology spells them; the
// record is written through the same enrichment path as pose and depth.
/// The reference predicate on the image (like `poseData`).
pub const REF_LOCAL: &str = "faceData";
/// The record class (like `Pose`).
pub const CLASS: &str = "Face";
/// Record field: the five landmarks, `x,y;…`.
pub const F_LANDMARKS: &str = "landmarks";
/// Record field: `pitch,yaw,roll` in degrees.
pub const F_HEAD_POSE: &str = "headPose";

/// The node's answer, as it came. Unknown fields are kept, never dropped.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FaceAnswer {
    /// Absent (a `200 {}`) is the node failing; `[]` is an image with no face.
    pub faces: Option<Vec<FaceDet>>,
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

/// One face as the node reported it.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FaceDet {
    #[serde(default)]
    pub bbox: Vec<f64>,
    pub score: Option<f64>,
    #[serde(default)]
    pub kps: Vec<Vec<f64>>,
    #[serde(default)]
    pub pose: Option<Vec<f64>>,
    #[serde(default)]
    pub embedding: Vec<f32>,
    /// age, sex, landmark_2d_106, landmark_3d_68, and whatever else: kept in
    /// the reply file, not read.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl FaceAnswer {
    /// `200 {}` with no `faces` list is how a node reports an internal
    /// failure: nothing was looked at, which is not "no faces".
    pub fn is_empty(&self) -> bool {
        self.faces.is_none()
    }
}

impl FaceDet {
    /// The parts the graph records, checked for shape. A face the node
    /// returned half-formed is refused whole: a record with a box and no
    /// vector would be in the graph and never in the index.
    fn checked(&self, n: usize) -> Result<()> {
        if self.bbox.len() != 4 {
            return Err(anyhow!("face {n}: bbox has {} numbers, not 4", self.bbox.len()));
        }
        if self.score.is_none() {
            return Err(anyhow!("face {n}: no score"));
        }
        if self.kps.len() != 5 || self.kps.iter().any(|p| p.len() != 2) {
            return Err(anyhow!("face {n}: kps is not five [x,y] points"));
        }
        if let Some(p) = &self.pose {
            if p.len() != 3 {
                return Err(anyhow!("face {n}: pose has {} numbers, not 3", p.len()));
            }
        }
        if self.embedding.is_empty() {
            return Err(anyhow!("face {n}: no embedding"));
        }
        Ok(())
    }

    fn bbox_text(&self) -> String {
        self.bbox.iter().map(|v| dec(*v)).collect::<Vec<_>>().join(",")
    }

    fn landmarks_text(&self) -> String {
        self.kps
            .iter()
            .map(|p| format!("{},{}", dec(p[0]), dec(p[1])))
            .collect::<Vec<_>>()
            .join(";")
    }

    fn head_pose_text(&self) -> Option<String> {
        self.pose
            .as_ref()
            .map(|p| p.iter().map(|v| dec(*v)).collect::<Vec<_>>().join(","))
    }
}

/// Format a decimal the way the graph wants it: plain digits, no exponent.
fn dec(v: f64) -> String {
    format!("{v}")
}

impl Pan {
    /// Record one face run for an image: the node's whole answer and one
    /// `.npy` per face under `image/enrichment/face/`, one Face record per
    /// face hung off a fresh reference, each vector in the index named after
    /// `model` under the face's own id, graph and XMP refreshed. A run that
    /// found no face writes the reference with no records, so the image is
    /// done for this model. Returns the record file's media-root-relative
    /// path. An answer with no `faces` list is refused here so the image
    /// stays pending.
    pub fn write_face(
        &self,
        id: &str,
        model: &str,
        answer: &FaceAnswer,
        raw: &serde_json::Value,
    ) -> Result<String> {
        let Some(faces) = &answer.faces else {
            return Err(anyhow!("face answer carries no faces list"));
        };
        for (n, f) in faces.iter().enumerate() {
            f.checked(n)?;
        }
        // The node's own answer, whole, beside the record. Named on the
        // reference as pan:modelReplyPath (goodlux, 2026-09-19).
        let answer_rel = self.enrichment_file(id, STAGE, model, "json")?;
        let side = self.layout.abs(&answer_rel);
        if let Some(p) = side.parent() {
            std::fs::create_dir_all(p).context("create face dir")?;
        }
        write_atomic(&side, serde_json::to_string_pretty(raw)?.as_bytes())
            .with_context(|| format!("write {}", side.display()))?;

        let mut records = Vec::with_capacity(faces.len());
        let mut keyed: Vec<(String, &[f32])> = Vec::with_capacity(faces.len());
        for f in faces {
            let face_id = gen_pan_id();
            let npy_rel = self.enrichment_file(id, STAGE, model, &format!("{face_id}.npy"))?;
            let npy_abs = self.layout.abs(&npy_rel);
            crate::npy::write_f32_1d(&npy_abs, &f.embedding)
                .with_context(|| format!("write {}", npy_abs.display()))?;
            let mut rec = EnrichmentRecord::new(face_id.clone(), CLASS, model)
                .field("bbox", f.bbox_text())
                .field("score", format!("{:.4}", f.score.unwrap_or_default()))
                .field(F_LANDMARKS, f.landmarks_text())
                .field("vectorPath", &npy_rel)
                .field("dim", f.embedding.len().to_string());
            if let Some(hp) = f.head_pose_text() {
                rec = rec.field(F_HEAD_POSE, hp);
            }
            if let Some(p) = answer.precision.as_deref().filter(|s| !s.trim().is_empty()) {
                rec = rec.field("precision", p);
            }
            if let Some(p) = answer.provider.as_deref().filter(|s| !s.trim().is_empty()) {
                rec = rec.field("provider", p);
            }
            records.push(rec);
            keyed.push((face_id, f.embedding.as_slice()));
        }
        // One index per face model, the model name as the index name, the
        // same rule as image embeddings. Keyed by the face, not the image.
        for (face_id, vec) in &keyed {
            self.index_insert(face_id, model, vec, None)?;
        }
        // The index is saved by the daemon's flush loop, on stop and on
        // drop, not here: a save rewrites the whole index file, 437 MB on a
        // store of 200,000 faces, and doing that after every image wrote the
        // disk at about 500 MB/s and held the stage to one image a second
        // (2026-11-11).
        let rel = self.write_enrichment(
            id,
            STAGE,
            REF_LOCAL,
            model,
            &records,
            crate::RecordFile::default().with_model_reply(&answer_rel),
        );
        if rel.is_err() {
            // The graph did not take the records: the index must not keep
            // vectors the graph cannot name.
            for (face_id, _) in &keyed {
                let _ = self.remove_from_indexes(face_id);
            }
        }
        rel
    }

    /// The image a Face record belongs to, as `(image IRI, face IRI)`, or
    /// `None` when no image's face reference lists it.
    pub fn image_of_face(&self, face_id: &str) -> Result<Option<(String, String)>> {
        crate::validate_pan_id(face_id)?;
        let face_iri = format!("{}{CLASS}/{face_id}", crate::config::PAN_MEDIA_NS);
        let q = format!(
            "SELECT ?img WHERE {{ ?ref pan:item <{face_iri}> . ?img pan:{REF_LOCAL} ?ref . }}"
        );
        if let crate::QueryResults::Solutions(sols) = self.query(&q)? {
            for s in sols {
                let s = s?;
                if let Some(oxigraph::model::Term::NamedNode(n)) = s.get("img") {
                    return Ok(Some((n.as_str().to_string(), face_iri)));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/face/answer.json");

    #[test]
    fn the_node_answer_parses_and_the_graph_parts_are_formatted() {
        let a: FaceAnswer = serde_json::from_str(FIXTURE).unwrap();
        assert!(!a.is_empty());
        assert_eq!((a.width, a.height), (Some(1024), Some(1536)));
        assert_eq!(a.model.as_deref(), Some("insightface/buffalo_l"));
        assert_eq!(a.precision.as_deref(), Some("fp32-onnx-cpu"));
        assert_eq!(a.provider.as_deref(), Some("katoptron"));
        let faces = a.faces.as_ref().unwrap();
        assert_eq!(faces.len(), 1);
        let f = &faces[0];
        f.checked(0).unwrap();
        assert_eq!(f.bbox_text(), "323.5,159.3,711.7,620.8");
        assert_eq!(
            f.landmarks_text(),
            "476.5,335.5;647.5,364.1;579.3,446.2;442.2,472.7;624,497.3"
        );
        assert_eq!(f.head_pose_text().as_deref(), Some("-1.08,18.82,11.37"));
        assert_eq!(f.embedding.len(), 512);
        let norm: f32 = f.embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "unit length, got {norm}");
        // The guesses and the long landmark sets are carried, not read.
        assert_eq!(f.extra["age"], 29);
        assert_eq!(f.extra["sex"], "F");
        assert_eq!(f.extra["landmark_2d_106"].as_array().unwrap().len(), 106);
        assert_eq!(f.extra["landmark_3d_68"].as_array().unwrap().len(), 68);
    }

    #[test]
    fn an_empty_body_is_a_failure_and_an_empty_list_is_no_faces() {
        let a: FaceAnswer = serde_json::from_str("{}").unwrap();
        assert!(a.is_empty());
        let b: FaceAnswer = serde_json::from_str(r#"{"faces": []}"#).unwrap();
        assert!(!b.is_empty());
        assert!(b.faces.as_ref().unwrap().is_empty());
    }

    #[test]
    fn a_half_formed_face_is_refused() {
        let f: FaceDet = serde_json::from_str(
            r#"{"bbox":[1,2,3],"score":0.9,"kps":[[1,2],[3,4],[5,6],[7,8],[9,10]],"embedding":[0.1]}"#,
        )
        .unwrap();
        assert!(f.checked(0).unwrap_err().to_string().contains("bbox"));
        let g: FaceDet = serde_json::from_str(
            r#"{"bbox":[1,2,3,4],"score":0.9,"kps":[[1,2],[3,4],[5,6],[7,8],[9,10]],"embedding":[]}"#,
        )
        .unwrap();
        assert!(g.checked(0).unwrap_err().to_string().contains("embedding"));
    }

    #[test]
    fn decimals_never_carry_an_exponent() {
        assert_eq!(dec(323.5), "323.5");
        assert_eq!(dec(624.0), "624");
        assert_eq!(dec(-1.08), "-1.08");
        assert_eq!(dec(1e-7), "0.0000001");
    }
}
