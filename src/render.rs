//! The render request a generated image arrived with.
//!
//! A diffusion user interface writes the whole call that made an image into a
//! PNG text chunk keyed `parameters`: the prompt, an optional negative prompt,
//! then one line of `Key: value` pairs. Pan has copied that chunk into the
//! stored file byte for byte since the first version and never read it, so
//! "every image from seed 1030317025" was a question the graph could not
//! answer about data sitting in its own files.
//!
//! WHAT THIS MIRRORS. The shape is read from the generator's own source
//! (sd-webui-forge-neo, `modules/processing.py: create_infotext` and
//! `modules/infotext_utils.py: parse_generation_parameters`, read 2026-09-19),
//! not guessed from samples:
//!
//! - The settings are the LAST line, and it counts as settings only if at
//!   least three `Key: value` pairs parse out of it. A one-line prompt that
//!   happens to contain a colon is a prompt, not settings.
//! - A key is word characters, spaces, dashes and slashes. A value is either a
//!   JSON string in double quotes or everything up to the next comma, so a
//!   comma inside `Lora hashes: "a: 1, b: 2"` belongs to its value.
//! - Any value shaped `1536x1536` is two numbers, whatever its key: Size,
//!   Hires resize, Seed resize from.
//! - A setting whose value equals its key is written bare, with no colon, and
//!   is skipped by their parser and by this one.
//!
//! WHAT IS OPEN-ENDED. The key set is not fixed and cannot be: every script
//! and extension may add its own through `p.extra_generation_params`, and the
//! built-in ones alone contribute around fifty. So Pan declares the settings
//! worth querying as properties of their own and keeps EVERY other key as a
//! named setting beside them — nothing is dropped, nothing needs a new word in
//! the ontology when an extension invents one, and the whole chunk is stored
//! verbatim as well.

use crate::enrich::EnrichmentRecord;
use crate::gen_pan_id;

/// The node class and the property that links an image to it.
pub const CLASS: &str = "RenderRequest";
pub const REF_LOCAL: &str = "renderRequest";
/// One key/value setting Pan does not declare a property for.
pub const SETTING_CLASS: &str = "RenderSetting";
pub const SETTING_LOCAL: &str = "renderSetting";
/// The PNG text chunk a diffusion user interface writes.
pub const PNG_KEYWORD: &str = "parameters";
/// At least this many pairs must parse for a line to be the settings line —
/// the generator's own test.
const MIN_PAIRS: usize = 3;

/// Settings Pan declares a property for, keyed as the generator writes them.
/// Everything else keeps its own name as a [`SETTING_CLASS`] node, so an
/// extension's key is stored and queryable without new vocabulary.
const DECLARED: [(&str, &str); 21] = [
    ("Steps", "renderSteps"),
    ("Sampler", "renderSampler"),
    ("Schedule type", "renderScheduleType"),
    ("CFG scale", "renderCfgScale"),
    ("Distilled CFG Scale", "renderDistilledCfgScale"),
    ("Seed", "renderSeed"),
    ("Variation seed", "renderVariationSeed"),
    ("Variation seed strength", "renderVariationSeedStrength"),
    ("Model", "renderModel"),
    ("Model hash", "renderModelHash"),
    ("VAE Encoder", "renderVaeEncoder"),
    ("VAE Decoder", "renderVaeDecoder"),
    ("Lora hashes", "renderLoraHashes"),
    ("Denoising strength", "renderDenoisingStrength"),
    ("Clip skip", "renderClipSkip"),
    ("RNG", "renderRng"),
    ("Diffusion in Low Bits", "renderDiffusionPrecision"),
    ("Hires upscaler", "renderHiresUpscaler"),
    ("Hires steps", "renderHiresSteps"),
    ("Hires checkpoint", "renderHiresCheckpoint"),
    ("Version", "renderVersion"),
];

/// Keys whose value is a size and whose two numbers Pan declares. Any other
/// size-shaped value keeps its text as a named setting.
const SIZE_KEYS: [(&str, &str, &str); 1] = [("Size", "width", "height")];

/// One parsed render request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderRequest {
    /// The positive prompt: every line before `Negative prompt:`.
    pub prompt: String,
    /// The negative prompt, when the chunk has one.
    pub negative_prompt: Option<String>,
    /// Declared settings as (property, value), in the order they appeared.
    pub declared: Vec<(String, String)>,
    /// Every other key, name and value as written.
    pub settings: Vec<(String, String)>,
    /// The whole chunk, unchanged.
    pub verbatim: String,
}

impl RenderRequest {
    /// Parse a `parameters` chunk. None when the text has no settings line —
    /// prose alone is not a render request.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let (head, settings_line) = split_settings(text)?;
        let (prompt, negative_prompt) = split_prompts(head);

        let mut out = RenderRequest {
            prompt,
            negative_prompt,
            verbatim: text.to_string(),
            ..Default::default()
        };
        for (key, value) in pairs(settings_line) {
            if let Some((_, w, h)) = SIZE_KEYS.iter().find(|(k, _, _)| *k == key) {
                if let Some((a, b)) = split_size(&value) {
                    out.declared.push((w.to_string(), a));
                    out.declared.push((h.to_string(), b));
                    continue;
                }
            }
            // `Module 1`, `Module 2`, `Hires Module 1`: one property, one
            // value each, in the order the generator wrote them.
            if key.starts_with("Module ") {
                out.declared.push(("renderModule".to_string(), value));
                continue;
            }
            match DECLARED.iter().find(|(k, _)| *k == key) {
                Some((_, prop)) => out.declared.push((prop.to_string(), value)),
                None => out.settings.push((key, value)),
            }
        }
        Some(out)
    }

    /// The request as nodes: the request itself, then one node per setting Pan
    /// does not declare a property for.
    pub fn records(&self) -> (EnrichmentRecord, Vec<EnrichmentRecord>) {
        let mut rec = EnrichmentRecord::new(gen_pan_id(), CLASS, "")
            .field("renderPrompt", &self.prompt)
            .field("renderParameters", &self.verbatim);
        if let Some(n) = &self.negative_prompt {
            rec = rec.field("renderNegativePrompt", n);
        }
        for (prop, value) in &self.declared {
            rec = rec.field(prop, value);
        }
        let settings: Vec<EnrichmentRecord> = self
            .settings
            .iter()
            .map(|(name, value)| {
                EnrichmentRecord::new(gen_pan_id(), SETTING_CLASS, "")
                    .field("settingName", name)
                    .field("settingValue", value)
            })
            .collect();
        (rec, settings)
    }
}

/// Head and settings line. The last line is the settings only if at least
/// three pairs parse from it — the generator's own rule, and the reason a
/// one-line prompt with a colon in it is not mistaken for settings.
fn split_settings(text: &str) -> Option<(&str, &str)> {
    let last = text.lines().next_back()?;
    if pairs(last).len() < MIN_PAIRS {
        return None;
    }
    let head = &text[..text.len() - last.len()];
    Some((head.trim_end(), last))
}

/// Everything before the settings line: the positive prompt, and a negative
/// one when a line names it. Both may run over several lines.
fn split_prompts(head: &str) -> (String, Option<String>) {
    let mut positive: Vec<&str> = Vec::new();
    let mut negative: Vec<String> = Vec::new();
    let mut in_negative = false;
    for line in head.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("Negative prompt:") {
            in_negative = true;
            negative.push(rest.trim().to_string());
            continue;
        }
        if in_negative {
            negative.push(line.to_string());
        } else {
            positive.push(line);
        }
    }
    let neg = (!negative.is_empty()).then(|| negative.join("\n").trim().to_string());
    (positive.join("\n").trim().to_string(), neg)
}

/// `Key: value, Key: "a, b"` — a key is word characters, spaces, dashes and
/// slashes; a value is a JSON string in quotes or text up to the next comma.
/// A bare token with no colon is skipped, as the generator's parser skips it.
fn pairs(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            field.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                field.push(c);
                escaped = true;
            }
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
    let Some((k, v)) = field.split_once(':') else {
        return;
    };
    let k = k.trim();
    if k.is_empty() || !k.chars().all(|c| c.is_alphanumeric() || " -/_".contains(c)) {
        return;
    }
    let v = unquote(v.trim());
    if !v.is_empty() {
        out.push((k.to_string(), v));
    }
}

/// A value the generator wrapped in quotes is a JSON string; anything else is
/// its own text.
fn unquote(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        if let Ok(s) = serde_json::from_str::<String>(v) {
            return s;
        }
    }
    v.to_string()
}

/// `1536x1536` → the two numbers.
fn split_size(v: &str) -> Option<(String, String)> {
    let (a, b) = v.split_once('x')?;
    let (a, b) = (a.trim(), b.trim());
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    (digits(a) && digits(b)).then(|| (a.to_string(), b.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "SUBJECT: An image of SYLKIE, close-up, at the glass lectern.\nSteps: 12, Sampler: Euler, Schedule type: Simple, CFG scale: 1.0, Seed: 1030317025, Size: 1536x1536, Model: anatomyKrea2_turboV2FP8, Model hash: 4157f5328b, Module 1: qwen_image_vae, Module 2: qwen3vl_4b_fp8_scaled, RNG: CPU, Version: neo-2.28";

    fn declared(r: &RenderRequest, prop: &str) -> Option<String> {
        r.declared
            .iter()
            .find(|(k, _)| k == prop)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn a_real_chunk_parses_into_its_parts() {
        let r = RenderRequest::parse(REAL).unwrap();
        assert!(r.prompt.starts_with("SUBJECT: An image of SYLKIE"));
        assert_eq!(r.negative_prompt, None);
        assert_eq!(declared(&r, "width").as_deref(), Some("1536"));
        assert_eq!(declared(&r, "height").as_deref(), Some("1536"));
        assert_eq!(declared(&r, "renderSteps").as_deref(), Some("12"));
        assert_eq!(declared(&r, "renderSampler").as_deref(), Some("Euler"));
        assert_eq!(declared(&r, "renderSeed").as_deref(), Some("1030317025"));
        assert_eq!(
            declared(&r, "renderModel").as_deref(),
            Some("anatomyKrea2_turboV2FP8")
        );
        let modules: Vec<&str> = r
            .declared
            .iter()
            .filter(|(k, _)| k == "renderModule")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(modules, vec!["qwen_image_vae", "qwen3vl_4b_fp8_scaled"]);
        assert_eq!(r.verbatim, REAL);
    }

    /// A key no property is declared for keeps its own name rather than being
    /// dropped — extensions add keys Pan has never heard of.
    #[test]
    fn an_undeclared_key_is_kept_by_name() {
        let t = "x\nSteps: 20, Seed: 7, Token merging ratio: 0.5, Schedule rho: 7.0";
        let r = RenderRequest::parse(t).unwrap();
        assert_eq!(
            r.settings,
            vec![
                ("Token merging ratio".to_string(), "0.5".to_string()),
                ("Schedule rho".to_string(), "7.0".to_string()),
            ]
        );
    }

    /// The generator quotes any value holding a comma, a colon or a newline.
    #[test]
    fn a_quoted_value_is_unquoted_and_keeps_its_commas() {
        let t = "x\nSteps: 20, Lora hashes: \"detail: abc123, film: def456\", Seed: 7";
        let r = RenderRequest::parse(t).unwrap();
        assert_eq!(
            declared(&r, "renderLoraHashes").as_deref(),
            Some("detail: abc123, film: def456")
        );
        assert_eq!(declared(&r, "renderSeed").as_deref(), Some("7"));
    }

    #[test]
    fn a_negative_prompt_over_several_lines_is_its_own_field() {
        let t = "a cat\non a mat\nNegative prompt: blurry\nextra fingers\nSteps: 20, Seed: 7, Sampler: Euler";
        let r = RenderRequest::parse(t).unwrap();
        assert_eq!(r.prompt, "a cat\non a mat");
        assert_eq!(r.negative_prompt.as_deref(), Some("blurry\nextra fingers"));
    }

    /// Fewer than three pairs on the last line means there is no settings
    /// line, which is how the generator itself decides.
    #[test]
    fn prose_alone_is_not_a_render_request() {
        assert!(RenderRequest::parse("just a caption someone wrote").is_none());
        assert!(RenderRequest::parse("").is_none());
        assert!(RenderRequest::parse("SUBJECT: a wolf, LIGHT: dusk").is_none());
    }

    /// A prompt whose own text looks like settings does not win: the settings
    /// are the last line.
    #[test]
    fn the_settings_are_the_last_line() {
        let t = "recipe: Steps: 1 knead, Sampler: hand, CFG scale: none\nSteps: 30, Seed: 9, Sampler: Euler";
        let r = RenderRequest::parse(t).unwrap();
        assert!(r.prompt.contains("knead"));
        assert_eq!(declared(&r, "renderSteps").as_deref(), Some("30"));
    }

    /// Hires and variation settings are declared where they are worth
    /// querying and kept by name where they are not.
    #[test]
    fn hires_and_variation_settings_land() {
        let t = "x\nSteps: 20, Seed: 7, Denoising strength: 0.35, Hires upscaler: 4x-UltraSharp, Hires steps: 10, Hires resize: 2048x2048, Variation seed: 12, Variation seed strength: 0.2";
        let r = RenderRequest::parse(t).unwrap();
        assert_eq!(
            declared(&r, "renderHiresUpscaler").as_deref(),
            Some("4x-UltraSharp")
        );
        assert_eq!(
            declared(&r, "renderDenoisingStrength").as_deref(),
            Some("0.35")
        );
        assert_eq!(declared(&r, "renderVariationSeed").as_deref(), Some("12"));
        // Hires resize has no declared property; its text is kept as written.
        assert!(r
            .settings
            .iter()
            .any(|(k, v)| k == "Hires resize" && v == "2048x2048"));
    }

    /// Every setting reaches a node: declared ones as fields of the request,
    /// the rest as nodes of their own.
    #[test]
    fn records_carry_the_declared_fields_and_every_other_key() {
        let t = "x\nSteps: 20, Seed: 7, Token merging ratio: 0.5";
        let r = RenderRequest::parse(t).unwrap();
        let (req, settings) = r.records();
        assert!(req
            .fields
            .iter()
            .any(|(k, v)| k == "renderSteps" && v == "20"));
        assert!(req.fields.iter().any(|(k, _)| k == "renderParameters"));
        assert_eq!(settings.len(), 1);
        assert!(settings[0]
            .fields
            .iter()
            .any(|(k, v)| k == "settingName" && v == "Token merging ratio"));
    }
}
