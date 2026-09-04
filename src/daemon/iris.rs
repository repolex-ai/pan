//! The model client — pand's ONE funnel to Iris (the eye), or anything that
//! speaks its shape. Contract measured from iris/src/iris/server.py on
//! 2026-09-03; every route is multipart with an `image` file field.
//!
//! Outcomes are three-valued, because the eye is: a real result, a TERMINAL
//! refusal (422 — these bytes will never caption; stop asking), or a
//! TRANSIENT failure (5xx / unreachable / timeout — ask again later). A 200
//! with an empty body is how `/see_pose` and `/segment` report internal
//! failure, so "200" is never read as "worked" — the fields are.

use anyhow::{anyhow, Context, Result};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use std::time::Duration;

pub const CALL_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Debug)]
pub enum CallError {
    /// Retry later: the eye is down, busy, or timed out.
    Transient(String),
    /// Never retry these bytes with this stage: the eye said no for cause.
    Terminal(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Transient(m) => write!(f, "transient: {m}"),
            CallError::Terminal(m) => write!(f, "terminal: {m}"),
        }
    }
}

impl std::error::Error for CallError {}

/// `/see_embed`: caption + vector from one image load.
#[derive(Debug, Clone, Deserialize)]
pub struct SeeEmbed {
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub vector: Vec<f32>,
    #[serde(default)]
    pub dim: usize,
    #[serde(rename = "sceneObjects", default)]
    pub scene_objects: Vec<String>,
    /// Everything else the eye said (scene* tags, model-keyed caption copy).
    /// Kept, not dropped — nothing is written from it until the vocabulary
    /// for it is declared (open with Rob, 2026-09-03).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `/see_pose`: one skeleton per detected person, 133 COCO-WholeBody
/// keypoints each as `[x, y, confidence]`, plus the drawn skeleton as PNG.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SeePose {
    #[serde(default)]
    pub keypoints: Vec<Vec<[f32; 3]>>,
    #[serde(rename = "skeleton_png_b64", default)]
    pub skeleton_png_b64: Option<String>,
}

/// One `/segment` region.
#[derive(Debug, Clone, Deserialize)]
pub struct Region {
    pub prompt: String,
    #[serde(default)]
    pub score: f32,
    #[serde(default)]
    pub bbox: Vec<i64>,
    #[serde(default)]
    pub polygon: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct SegmentResponse {
    #[serde(default)]
    regions: Vec<Region>,
}

#[derive(Clone)]
pub struct Iris {
    client: reqwest::Client,
}

impl Iris {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(CALL_TIMEOUT)
                .build()
                .expect("reqwest client"),
        }
    }

    fn image_part(bytes: &[u8], media_type: &str) -> Result<Part> {
        let ext = match media_type {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "png",
        };
        Part::bytes(bytes.to_vec())
            .file_name(format!("image.{ext}"))
            .mime_str(media_type)
            .map_err(|e| anyhow!("multipart mime: {e}"))
    }

    async fn post(&self, url: &str, form: Form) -> std::result::Result<serde_json::Value, CallError> {
        let resp = self
            .client
            .post(url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| CallError::Transient(format!("{url}: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| CallError::Transient(format!("{url}: read body: {e}")))?;
        if status.as_u16() == 422 {
            return Err(CallError::Terminal(format!("{url}: {}", body.chars().take(300).collect::<String>())));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(CallError::Transient(format!("{url}: {status}: {}", body.chars().take(300).collect::<String>())));
        }
        if !status.is_success() {
            return Err(CallError::Terminal(format!("{url}: {status}: {}", body.chars().take(300).collect::<String>())));
        }
        serde_json::from_str(&body).map_err(|e| CallError::Transient(format!("{url}: response not JSON: {e}")))
    }

    pub async fn see_embed(&self, url: &str, bytes: &[u8], media_type: &str) -> std::result::Result<SeeEmbed, CallError> {
        let form = Form::new()
            .part("image", Self::image_part(bytes, media_type).map_err(|e| CallError::Terminal(e.to_string()))?)
            .text("resident", "true");
        let v = self.post(url, form).await?;
        let out: SeeEmbed = serde_json::from_value(v).map_err(|e| CallError::Transient(format!("see_embed shape: {e}")))?;
        if out.vector.is_empty() {
            return Err(CallError::Transient("see_embed returned no vector".into()));
        }
        if out.dim != 0 && out.dim != out.vector.len() {
            return Err(CallError::Transient(format!("see_embed dim {} != vector length {}", out.dim, out.vector.len())));
        }
        Ok(out)
    }

    /// `/see` (or `/see_embed` — the caption fields are the same): caption
    /// only, no vector required.
    pub async fn see(&self, url: &str, bytes: &[u8], media_type: &str) -> std::result::Result<SeeEmbed, CallError> {
        let form = Form::new()
            .part("image", Self::image_part(bytes, media_type).map_err(|e| CallError::Terminal(e.to_string()))?)
            .text("resident", "true");
        let v = self.post(url, form).await?;
        serde_json::from_value(v).map_err(|e| CallError::Transient(format!("see shape: {e}")))
    }

    pub async fn see_pose(&self, url: &str, bytes: &[u8], media_type: &str) -> std::result::Result<SeePose, CallError> {
        let form = Form::new()
            .part("image", Self::image_part(bytes, media_type).map_err(|e| CallError::Terminal(e.to_string()))?)
            .text("with_keypoints", "true");
        let v = self.post(url, form).await?;
        serde_json::from_value(v).map_err(|e| CallError::Transient(format!("see_pose shape: {e}")))
    }

    pub async fn segment(&self, url: &str, bytes: &[u8], media_type: &str, prompts: &[String]) -> std::result::Result<Vec<Region>, CallError> {
        if prompts.is_empty() {
            return Err(CallError::Terminal("segment needs at least one prompt".into()));
        }
        let form = Form::new()
            .part("image", Self::image_part(bytes, media_type).map_err(|e| CallError::Terminal(e.to_string()))?)
            .text("prompts", prompts.join(","));
        let v = self.post(url, form).await?;
        let out: SegmentResponse = serde_json::from_value(v).map_err(|e| CallError::Transient(format!("segment shape: {e}")))?;
        Ok(out.regions)
    }
}

impl Default for Iris {
    fn default() -> Self {
        Self::new()
    }
}

/// Keypoints → the `pan:keypoints` literal: `x,y,c;x,y,c;…` in the model's
/// own order, one Pose per person.
pub fn keypoints_literal(person: &[[f32; 3]]) -> String {
    person
        .iter()
        .map(|k| format!("{},{},{}", trim_f(k[0]), trim_f(k[1]), trim_f(k[2])))
        .collect::<Vec<_>>()
        .join(";")
}

fn trim_f(v: f32) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

pub fn bbox_literal(b: &[i64]) -> String {
    b.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")
}

impl SeePose {
    pub fn skeleton_png(&self) -> Result<Option<Vec<u8>>> {
        use base64::Engine;
        match &self.skeleton_png_b64 {
            Some(b) if !b.is_empty() => Ok(Some(
                base64::engine::general_purpose::STANDARD
                    .decode(b)
                    .context("decode skeleton png")?,
            )),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypoints_literal_is_compact() {
        let p = vec![[1.0, 2.5, 0.9], [3.25, 4.0, 0.0]];
        assert_eq!(keypoints_literal(&p), "1,2.5,0.9;3.25,4,0");
    }

    #[test]
    fn see_embed_keeps_unknown_fields() {
        let v: SeeEmbed = serde_json::from_str(
            r#"{"caption":"a cat","qwen35vl9bCaption":"a cat","sceneMood":"calm","sceneObjects":["cat"],"vector":[0.1,0.2],"dim":2}"#,
        )
        .unwrap();
        assert_eq!(v.caption.as_deref(), Some("a cat"));
        assert_eq!(v.scene_objects, vec!["cat"]);
        assert!(v.extra.contains_key("sceneMood"));
        assert!(v.extra.contains_key("qwen35vl9bCaption"));
    }

    #[test]
    fn empty_pose_body_decodes_to_nothing() {
        let p: SeePose = serde_json::from_str("{}").unwrap();
        assert!(p.keypoints.is_empty());
        assert!(p.skeleton_png().unwrap().is_none());
    }
}
