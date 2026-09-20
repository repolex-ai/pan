//! The render request a generated image arrived with.
//!
//! An image made by a diffusion user interface carries the whole call that
//! made it in a PNG text chunk keyed `parameters`: the prompt, then a line of
//! `Key: value` pairs naming the sampler, the seed, the checkpoint and the
//! rest. Pan copies that chunk into the stored file byte for byte and always
//! has; until now nothing read it, so "every image seeded 1030317025" was a
//! question the graph could not answer.
//!
//! This parses it into a `pan:RenderRequest` node hanging off the image
//! (goodlux, 2026-09-19). The whole chunk is kept verbatim in one field as
//! well, so a key Pan does not know is not a key Pan has lost.

use crate::enrich::EnrichmentRecord;
use crate::gen_pan_id;

/// The node class and the property that links an image to it.
pub const CLASS: &str = "RenderRequest";
pub const REF_LOCAL: &str = "renderRequest";
/// The PNG text chunk a diffusion user interface writes.
pub const PNG_KEYWORD: &str = "parameters";

/// `Key: value` pairs Pan declares, as they appear in the chunk, paired with
/// the property that holds them. A key not in this list still reaches the
/// graph inside `renderParameters`, and earns its own property when someone
/// needs to query it.
const KEYS: [(&str, &str); 12] = [
    ("Steps", "renderSteps"),
    ("Sampler", "renderSampler"),
    ("Schedule type", "renderScheduleType"),
    ("CFG scale", "renderCfgScale"),
    ("Seed", "renderSeed"),
    ("Model", "renderModel"),
    ("Model hash", "renderModelHash"),
    ("RNG", "renderRng"),
    ("Version", "renderVersion"),
    ("Denoising strength", "renderDenoisingStrength"),
    ("Hires upscaler", "renderHiresUpscaler"),
    ("Clip skip", "renderClipSkip"),
];

/// One parsed render request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderRequest {
    /// The positive prompt: everything before `Negative prompt:` or, when
    /// there is none, before the settings line.
    pub prompt: String,
    /// The negative prompt, when the chunk has one.
    pub negative_prompt: Option<String>,
    /// Declared settings, as (property, value), in the order KEYS names them.
    pub settings: Vec<(String, String)>,
    /// `Size: 1536x1536` split, when present.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `Module 1`, `Module 2`, … one value each, in the order they appear.
    pub modules: Vec<String>,
    /// The whole chunk, unchanged.
    pub verbatim: String,
}

impl RenderRequest {
    /// Parse a `parameters` chunk. None when the text carries no settings line
    /// at all — a chunk that is only prose is not a render request.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let settings_line = last_settings_line(text)?;
        let head = text[..text.len() - settings_line.len()].trim_end();
        let (prompt, negative_prompt) = split_prompts(head);

        let mut out = RenderRequest {
            prompt,
            negative_prompt,
            verbatim: text.to_string(),
            ..Default::default()
        };
        for (key, value) in pairs(settings_line) {
            if key == "Size" {
                if let Some((w, h)) = value.split_once('x') {
                    out.width = w.trim().parse().ok();
                    out.height = h.trim().parse().ok();
                }
                continue;
            }
            if key.starts_with("Module ") {
                out.modules.push(value);
                continue;
            }
            if let Some((_, prop)) = KEYS.iter().find(|(k, _)| *k == key) {
                out.settings.push((prop.to_string(), value));
            }
        }
        Some(out)
    }

    /// The record as it is written: one node with the fields it has.
    pub fn record(&self) -> EnrichmentRecord {
        let mut rec = EnrichmentRecord::new(gen_pan_id(), CLASS, "")
            .field("renderPrompt", &self.prompt)
            .field("renderParameters", &self.verbatim);
        if let Some(n) = &self.negative_prompt {
            rec = rec.field("renderNegativePrompt", n);
        }
        for (prop, value) in &self.settings {
            rec = rec.field(prop, value);
        }
        if let Some(w) = self.width {
            rec = rec.field("width", w.to_string());
        }
        if let Some(h) = self.height {
            rec = rec.field("height", h.to_string());
        }
        for m in &self.modules {
            rec = rec.field("renderModule", m);
        }
        rec
    }
}

/// The settings line is the LAST line that starts with `Steps:` — a prompt may
/// contain anything, including a line that looks like settings.
fn last_settings_line(text: &str) -> Option<&str> {
    text.lines()
        .rev()
        .find(|l| l.trim_start().starts_with("Steps:"))
}

/// Everything before the settings line: the positive prompt, and a negative
/// one when the chunk names it.
fn split_prompts(head: &str) -> (String, Option<String>) {
    match head.split_once("Negative prompt:") {
        Some((pos, neg)) => (pos.trim().to_string(), Some(neg.trim().to_string())),
        None => (head.trim().to_string(), None),
    }
}

/// `Key: value, Key: value` — commas inside double quotes are part of the
/// value, since `Lora hashes: "a: 1, b: 2"` is one setting, not three.
fn pairs(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                field.push(c);
            }
            ',' if !quoted => {
                push_pair(&mut out, &field);
                field.clear();
            }
            _ => field.push(c),
        }
    }
    push_pair(&mut out, &field);
    out
}

fn push_pair(out: &mut Vec<(String, String)>, field: &str) {
    if let Some((k, v)) = field.split_once(':') {
        let k = k.trim().to_string();
        let v = v.trim().trim_matches('"').trim().to_string();
        if !k.is_empty() && !v.is_empty() {
            out.push((k, v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "SUBJECT: An image of SYLKIE, close-up, at the glass lectern.\nSteps: 12, Sampler: Euler, Schedule type: Simple, CFG scale: 1.0, Seed: 1030317025, Size: 1536x1536, Model: anatomyKrea2_turboV2FP8, Model hash: 4157f5328b, Module 1: qwen_image_vae, Module 2: qwen3vl_4b_fp8_scaled, RNG: CPU, Version: neo-2.28";

    #[test]
    fn a_real_chunk_parses_into_its_parts() {
        let r = RenderRequest::parse(REAL).unwrap();
        assert!(r.prompt.starts_with("SUBJECT: An image of SYLKIE"));
        assert_eq!(r.negative_prompt, None);
        assert_eq!(r.width, Some(1536));
        assert_eq!(r.height, Some(1536));
        assert_eq!(r.modules, vec!["qwen_image_vae", "qwen3vl_4b_fp8_scaled"]);
        let get = |p: &str| {
            r.settings
                .iter()
                .find(|(k, _)| k == p)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("renderSteps").as_deref(), Some("12"));
        assert_eq!(get("renderSampler").as_deref(), Some("Euler"));
        assert_eq!(get("renderScheduleType").as_deref(), Some("Simple"));
        assert_eq!(get("renderCfgScale").as_deref(), Some("1.0"));
        assert_eq!(get("renderSeed").as_deref(), Some("1030317025"));
        assert_eq!(
            get("renderModel").as_deref(),
            Some("anatomyKrea2_turboV2FP8")
        );
        assert_eq!(get("renderModelHash").as_deref(), Some("4157f5328b"));
        assert_eq!(get("renderRng").as_deref(), Some("CPU"));
        assert_eq!(get("renderVersion").as_deref(), Some("neo-2.28"));
        assert_eq!(r.verbatim, REAL);
    }

    #[test]
    fn a_negative_prompt_is_its_own_field() {
        let t = "a cat\nNegative prompt: blurry, extra fingers\nSteps: 20, Seed: 7";
        let r = RenderRequest::parse(t).unwrap();
        assert_eq!(r.prompt, "a cat");
        assert_eq!(r.negative_prompt.as_deref(), Some("blurry, extra fingers"));
    }

    /// A comma inside quotes belongs to its value.
    #[test]
    fn a_quoted_value_keeps_its_commas() {
        let t = "x\nSteps: 20, Lora hashes: \"a: 1, b: 2\", Seed: 7";
        let r = RenderRequest::parse(t).unwrap();
        let seed = r.settings.iter().find(|(k, _)| k == "renderSeed");
        assert_eq!(seed.map(|(_, v)| v.as_str()), Some("7"));
    }

    /// A prompt is not a render request, and a prompt that mentions steps is
    /// still not one.
    #[test]
    fn prose_alone_is_not_a_render_request() {
        assert!(RenderRequest::parse("just a caption someone wrote").is_none());
        assert!(RenderRequest::parse("").is_none());
    }

    /// The settings line is the last one, so a prompt containing its own
    /// Steps: line does not win.
    #[test]
    fn the_last_settings_line_is_the_settings() {
        let t = "recipe: Steps: 1 knead\nSteps: 30, Seed: 9";
        let r = RenderRequest::parse(t).unwrap();
        assert!(r.prompt.contains("knead"));
        let steps = r.settings.iter().find(|(k, _)| k == "renderSteps");
        assert_eq!(steps.map(|(_, v)| v.as_str()), Some("30"));
    }
}
