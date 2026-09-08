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

pub use super::config::Target;

pub const CALL_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Debug)]
pub enum CallError {
    /// Retry later: the eye is down or timed out.
    Transient(String),
    /// Retry in SECONDS, not minutes: every node's queue is full right now
    /// (m3rc's door, 2026-09-05: `503 {"reason":"busy"}` — max_queue 2 per
    /// node per model). Nothing is wrong with the image or the door.
    Busy(String),
    /// Never retry these bytes with this stage: the eye said no for cause.
    Terminal(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Transient(m) => write!(f, "transient: {m}"),
            CallError::Busy(m) => write!(f, "busy: {m}"),
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

/// `/percept/vlm` (m3rc, 2026-09-05): image + prompt → `text`, and the
/// `model` / `provider` that answered. Unknown fields are kept, never dropped.
#[derive(Debug, Clone, Deserialize)]
pub struct Vlm {
    pub text: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
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

    fn request(&self, t: &Target) -> reqwest::RequestBuilder {
        let mut req = self.client.post(&t.url);
        if let Some(a) = &t.auth {
            req = req.header(reqwest::header::AUTHORIZATION, a);
        }
        req
    }

    async fn post(&self, t: &Target, form: Form) -> std::result::Result<serde_json::Value, CallError> {
        let url = &t.url;
        let resp = self
            .request(t)
            .multipart(form)
            .send()
            .await
            .map_err(|e| CallError::Transient(format!("{url}: {e}")))?;
        self.finish(url, resp).await
    }

    /// `POST` a JSON body. Used where the door forwards Pan's bytes to a
    /// provider untouched and hands back the provider's own status + body.
    async fn post_json(&self, t: &Target, body: &serde_json::Value) -> std::result::Result<serde_json::Value, CallError> {
        let url = &t.url;
        let resp = self
            .request(t)
            .json(body)
            .send()
            .await
            .map_err(|e| CallError::Transient(format!("{url}: {e}")))?;
        self.finish(url, resp).await
    }

    async fn finish(&self, url: &str, resp: reqwest::Response) -> std::result::Result<serde_json::Value, CallError> {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| CallError::Transient(format!("{url}: read body: {e}")))?;
        if status.as_u16() == 422 {
            return Err(CallError::Terminal(format!("{url}: {}", body.chars().take(300).collect::<String>())));
        }
        if status.as_u16() == 503 {
            // m3rc's door says WHY in the body: `busy` = every node's queue is
            // full (retry in seconds); `backend_down` = no node is up at all
            // (a fact about the door, not the image — the stage holds).
            let reason = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(str::to_owned))
                .unwrap_or_default();
            let short = body.chars().take(300).collect::<String>();
            return Err(match reason.as_str() {
                "busy" => CallError::Busy(format!("{url}: 503 busy: {short}")),
                "backend_down" => CallError::Transient(format!("{url}: 503 backend_down (no node up): {short}")),
                _ => CallError::Transient(format!("{url}: {status}: {short}")),
            });
        }
        if status.as_u16() == 402 {
            // The provider's account is out of credit. A fact about the
            // account, not the image: nothing about this image will change,
            // and nothing about the next one either. The stage holds.
            return Err(CallError::Transient(format!("{url}: 402 quota exceeded (add credits): {}", body.chars().take(300).collect::<String>())));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(CallError::Transient(format!("{url}: {status}: {}", body.chars().take(300).collect::<String>())));
        }
        if !status.is_success() {
            return Err(CallError::Terminal(format!("{url}: {status}: {}", body.chars().take(300).collect::<String>())));
        }
        serde_json::from_str(&body).map_err(|e| CallError::Transient(format!("{url}: response not JSON: {e}")))
    }

    /// `/percept/embed` on Iris (:1215): the image as a file part and the
    /// text to embed WITH it as the `text` string part; ONE joint vector of
    /// pixels and text comes back (m3rc, 2026-09-08). Pan sends the file's
    /// complete XMP packet as the text (goodlux, 2026-09-08). The model
    /// input is capped at 8192 tokens, image and text together; a very long
    /// packet is truncated at its end by the node.
    pub async fn embed(&self, t: &Target, bytes: &[u8], media_type: &str, text: &str) -> std::result::Result<SeeEmbed, CallError> {
        let form = Form::new()
            .part("image", Self::image_part(bytes, media_type).map_err(|e| CallError::Terminal(e.to_string()))?)
            .text("text", text.to_string());
        let v = self.post(t, form).await?;
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
    pub async fn see(&self, t: &Target, bytes: &[u8], media_type: &str) -> std::result::Result<SeeEmbed, CallError> {
        let form = Form::new()
            .part("image", Self::image_part(bytes, media_type).map_err(|e| CallError::Terminal(e.to_string()))?)
            .text("resident", "true");
        let v = self.post(t, form).await?;
        serde_json::from_value(v).map_err(|e| CallError::Transient(format!("see shape: {e}")))
    }

    /// `POST /percept/vlm` (m3rc's door, 2026-09-05, third and final shape —
    /// Rob: the door must not massage anything): the body IS the OpenAI
    /// chat-completions request the provider should see. Pan builds it, the
    /// door adds the Authorization header, forwards the bytes, and returns
    /// the provider's response body and status as-is. Pan reads
    /// `choices[0].message.content` itself. `extra_body` (config) is merged
    /// into the top level verbatim — that is where `provider`,
    /// `chat_template_kwargs.enable_thinking`, `max_tokens` live.
    pub async fn vlm(
        &self,
        t: &Target,
        model: &str,
        bytes: &[u8],
        media_type: &str,
        prompt: &str,
        extra_body: Option<&serde_json::Value>,
    ) -> std::result::Result<Vlm, CallError> {
        let body = build_chat_request(model, media_type, bytes, prompt, extra_body).map_err(CallError::Terminal)?;
        let v = self.post_json(t, &body).await?;
        let text = text_from_chat_response(&v)
            .ok_or_else(|| CallError::Transient(format!("vlm: no choices[0].message.content in: {}", v.to_string().chars().take(300).collect::<String>())))?;
        let model = v.get("model").and_then(|m| m.as_str()).map(str::to_owned);
        let extra = match v {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        Ok(Vlm { text, model, provider: None, extra })
    }

    pub async fn see_pose(&self, t: &Target, bytes: &[u8], media_type: &str) -> std::result::Result<SeePose, CallError> {
        let form = Form::new()
            .part("image", Self::image_part(bytes, media_type).map_err(|e| CallError::Terminal(e.to_string()))?)
            .text("with_keypoints", "true");
        let v = self.post(t, form).await?;
        serde_json::from_value(v).map_err(|e| CallError::Transient(format!("see_pose shape: {e}")))
    }

    /// `/percept/segment` (m3rc's door → SAM3 on Salad): `prompts` is one
    /// comma-separated string of nouns. Returns the parsed regions AND the
    /// whole response as it came, so the caller can keep everything the
    /// server said (area, verts, provenance) beside the record.
    pub async fn segment(&self, t: &Target, bytes: &[u8], media_type: &str, prompts: &[String]) -> std::result::Result<(Vec<Region>, serde_json::Value), CallError> {
        if prompts.is_empty() {
            return Err(CallError::Terminal("segment needs at least one prompt".into()));
        }
        let form = Form::new()
            .part("image", Self::image_part(bytes, media_type).map_err(|e| CallError::Terminal(e.to_string()))?)
            .text("prompts", prompts.join(","));
        let v = self.post(t, form).await?;
        let out: SegmentResponse = serde_json::from_value(v.clone()).map_err(|e| CallError::Transient(format!("segment shape: {e}")))?;
        Ok((out.regions, v))
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

/// The OpenAI chat-completions request a caption provider sees, built by Pan
/// and forwarded by the door byte for byte. One user message: the image as a
/// data URL, then the prompt. `extra_body` keys land at the top level as
/// given; they may not override `model` or `messages`.
pub fn build_chat_request(
    model: &str,
    media_type: &str,
    bytes: &[u8],
    prompt: &str,
    extra_body: Option<&serde_json::Value>,
) -> std::result::Result<serde_json::Value, String> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut body = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{b64}")}},
                {"type": "text", "text": prompt}
            ]
        }]
    });
    if let Some(eb) = extra_body {
        let serde_json::Value::Object(m) = eb else {
            return Err(format!("caption extra_body must be a JSON object, got: {eb}"));
        };
        let out = body.as_object_mut().expect("object");
        for (k, v) in m {
            if k == "model" || k == "messages" {
                return Err(format!("caption extra_body may not set `{k}`; that comes from the stage"));
            }
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(body)
}

/// `choices[0].message.content` from a chat-completions response. Content is
/// a string, or (some servers) a list of parts whose `text` fields are joined.
pub fn text_from_chat_response(v: &serde_json::Value) -> Option<String> {
    let content = v.get("choices")?.get(0)?.get("message")?.get("content")?;
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            let s: Vec<&str> = parts.iter().filter_map(|p| p.get("text").and_then(|t| t.as_str())).collect();
            if s.is_empty() { None } else { Some(s.join("")) }
        }
        _ => None,
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;

    #[test]
    fn request_is_the_openai_shape_with_extra_body_at_top_level() {
        let eb = serde_json::json!({"provider": {"aci_verified": true}, "chat_template_kwargs": {"enable_thinking": false}, "max_tokens": 512});
        let b = build_chat_request("qwen/qwen3.8-27b", "image/jpeg", b"\xFF\xD8\xFF", "Describe.", Some(&eb)).unwrap();
        assert_eq!(b["model"], "qwen/qwen3.8-27b");
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["messages"][0]["content"][0]["type"], "image_url");
        assert!(b["messages"][0]["content"][0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,/9j/"));
        assert_eq!(b["messages"][0]["content"][1]["text"], "Describe.");
        assert_eq!(b["provider"]["aci_verified"], true);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(b["max_tokens"], 512);
        assert!(b.get("prompt").is_none() && b.get("extra_body").is_none(), "no door-era fields");
    }

    #[test]
    fn extra_body_cannot_hijack_model_or_messages() {
        let eb = serde_json::json!({"model": "other"});
        assert!(build_chat_request("m", "image/jpeg", b"x", "p", Some(&eb)).is_err());
        let eb = serde_json::json!(["not", "an", "object"]);
        assert!(build_chat_request("m", "image/jpeg", b"x", "p", Some(&eb)).is_err());
    }

    #[test]
    fn text_comes_from_choices_zero() {
        let v = serde_json::json!({"model": "qwen/qwen3.8-27b", "choices": [{"message": {"role": "assistant", "content": "A woman reads."}}]});
        assert_eq!(text_from_chat_response(&v).as_deref(), Some("A woman reads."));
        let v = serde_json::json!({"choices": [{"message": {"content": [{"type": "text", "text": "A "}, {"type": "text", "text": "man."}]}}]});
        assert_eq!(text_from_chat_response(&v).as_deref(), Some("A man."));
        assert!(text_from_chat_response(&serde_json::json!({"error": "nope"})).is_none());
    }
}
