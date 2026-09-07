//! The stage ladder — the graph is the queue.
//!
//! Every pass, for every store, for every configured stage: ask the graph
//! which images have no record from this stage's model, take a bounded
//! batch, run the model, write the data file + graph + XMP for each one, and
//! move on. A failure leaves the image pending (with an in-memory hold so it
//! is not retried every pass); success is only ever the record in the graph.
//! "These 500 images have no caption, cycle captioning" is literally one
//! stage's query.
//!
//! Stages (config key → what it records):
//!   embed   `/see_embed`  → pan:Embedding (+ vector index) AND, when the
//!                           endpoint config names a `caption_model`, a
//!                           pan:Caption from the same image load
//!   caption `/see`        → pan:Caption only (a second captioning model)
//!   pose    `/see_pose`   → one pan:Pose per detected person + skeleton overlay
//!   sam3    `/percept/segment` → pan:Region per grounded prompt. The prompts
//!                           are the caption's OBJECTS line (the segmentable
//!                           nouns the caption model listed), so this stage
//!                           waits for a caption. The whole server answer is
//!                           kept as a .json beside the record.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::iris::{self, CallError};
use super::{Daemon, StoreHandle};
use crate::enrich::EnrichmentRecord;
use crate::{gen_pan_id, PendingItem};

pub const STAGE_EMBED: &str = "embed";
pub const STAGE_CAPTION: &str = "caption";
pub const STAGE_POSE: &str = "pose";
pub const STAGE_SAM3: &str = "sam3";

/// Which graph link a stage's completion is read from.
pub fn link_for(stage: &str) -> Option<&'static str> {
    match stage {
        STAGE_EMBED => Some("embedding"),
        STAGE_CAPTION => Some("captionItem"),
        STAGE_POSE => Some("pose"),
        STAGE_SAM3 => Some("region"),
        _ => None,
    }
}

/// Run the ladder forever. One pass touches every store and every stage;
/// then it sleeps `interval_secs`. Never exits on a failed item.
pub async fn run(d: Arc<Daemon>) {
    let every = Duration::from_secs(d.cfg.interval_secs);
    loop {
        let did = run_pass(d.clone()).await;
        if did == 0 {
            tokio::time::sleep(every).await;
        }
    }
}

/// One pass. Returns how many items were processed (success or failure), so
/// the caller can go straight into the next pass while there is work.
pub async fn run_pass(d: Arc<Daemon>) -> usize {
    let mut done = 0usize;
    for store in d.stores.clone() {
        for stage in [STAGE_EMBED, STAGE_CAPTION, STAGE_POSE, STAGE_SAM3] {
            if !d.cfg.models.get(stage).map(|m| m.enabled).unwrap_or(false) {
                continue;
            }
            match run_stage(d.clone(), store.clone(), stage).await {
                Ok(n) => done += n,
                Err(e) => tracing::error!(store = %store.entry.id, stage, "stage pass failed: {e:#}"),
            }
        }
        // Everything configured has a record → the object is ready as
        // configured; say when. With no stages configured, ingest IS ready.
        let required: Vec<(String, String)> = d
            .cfg
            .active_models()
            .filter_map(|(stage, ep)| link_for(stage).map(|l| (l.to_string(), ep.model.clone())))
            .collect();
        let s = store.clone();
        let batch = d.cfg.batch * 4;
        match tokio::task::spawn_blocking(move || -> Result<usize> {
            let mut n = 0;
            for id in s.pan.ready_candidates(&required, batch)? {
                if s.pan.mark_ready(&id)? {
                    n += 1;
                }
            }
            Ok(n)
        })
        .await
        {
            Ok(Ok(n)) if n > 0 => {
                tracing::info!(store = %store.entry.id, n, "marked ready");
                done += n;
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!(store = %store.entry.id, "ready pass failed: {e:#}"),
            Err(e) => tracing::error!(store = %store.entry.id, "ready pass join: {e}"),
        }
    }
    done
}

/// How long a whole stage waits after a call failed before reaching the model
/// (connection refused / reset / timeout). One try per hold; the door being
/// down is not a fact about the image.
const DOOR_DOWN_HOLD: Duration = Duration::from_secs(60);

/// How long a stage breathes after `503 busy` (every node's queue full) before
/// its next pass. Seconds, not minutes: the door asked for a retry "in seconds".
const BUSY_WAIT: Duration = Duration::from_secs(5);

async fn run_stage(d: Arc<Daemon>, store: Arc<StoreHandle>, stage: &'static str) -> Result<usize> {
    let ep = d.cfg.models.get(stage).cloned().ok_or_else(|| anyhow!("stage {stage} not configured"))?;
    // Which address this pass calls. A hold on the primary sends the stage to
    // its fallback (if it has one); a hold on both means wait. The primary is
    // probed again the moment its hold expires, so the door gets the traffic
    // back as soon as it is up.
    let held = |key: &str| -> bool {
        d.stage_hold.lock().unwrap().get(key).map(|u| *u > Instant::now()).unwrap_or(false)
    };
    let fallback_key = format!("{stage}/fallback");
    let target = if !held(stage) {
        ep.primary()
    } else if let Some(fb) = ep.fallback_target().filter(|_| !held(&fallback_key)) {
        fb
    } else {
        return Ok(0);
    };
    let hold_key: String = if target.via == "fallback" { fallback_key } else { stage.to_string() };
    let link = link_for(stage).ok_or_else(|| anyhow!("unknown stage {stage}"))?;
    let batch = d.cfg.batch;
    // Ask for more than the batch so items on hold do not starve the ones
    // behind them; then take the first `batch` that are not holding.
    let pending: Vec<PendingItem> = {
        let s = store.clone();
        let model = ep.model.clone();
        let since = d.cfg.backfill_since.clone();
        tokio::task::spawn_blocking(move || s.pan.pending_for(link, &model, batch * 4, since.as_deref())).await??
    };
    let work: Vec<PendingItem> = pending
        .into_iter()
        .filter(|p| d.holding(&store.entry.id, &p.id, stage).is_none())
        .take(batch)
        .collect();
    // Every item in the batch is spawned at once; the stage's Limiter decides
    // how many are actually in flight. Results are handled as they land.
    let n = work.len();
    let mut set = tokio::task::JoinSet::new();
    for item in work {
        let (d, store, ep, target) = (d.clone(), store.clone(), ep.clone(), target.clone());
        set.spawn(async move {
            let permit = d.funnels[stage].acquire().await;
            let result = run_one(&d, &store, stage, &ep, &target, &item).await;
            drop(permit);
            (item, result)
        });
    }
    let mut saw_busy = false;
    while let Some(joined) = set.join_next().await {
        let (item, result) = match joined {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(stage, "stage task join: {e}");
                continue;
            }
        };
        match result {
            Ok(()) => {
                d.clear_attempt(&store.entry.id, &item.id, stage);
                d.funnels[stage].on_success();
                tracing::info!(store = %store.entry.id, id = %item.id, stage, model = %ep.model, via = target.via, window = d.funnels[stage].window(), "recorded");
            }
            Err(e) => {
                if let Some(CallError::Busy(m)) = e.downcast_ref::<CallError>() {
                    // Every node's queue is full: the window was too wide.
                    // Narrow it; no attempt is recorded, the image stays
                    // pending and is asked again next pass.
                    saw_busy = true;
                    d.funnels[stage].on_busy();
                    tracing::info!(store = %store.entry.id, id = %item.id, stage, window = d.funnels[stage].window(), "door busy: {m}");
                    continue;
                }
                let (msg, terminal) = match e.downcast_ref::<CallError>() {
                    Some(CallError::Terminal(m)) => (m.clone(), true),
                    Some(CallError::Transient(m)) => (m.clone(), false),
                    Some(CallError::Busy(m)) => (m.clone(), false),
                    None => (format!("{e:#}"), false),
                };
                tracing::warn!(store = %store.entry.id, id = %item.id, stage, terminal, "stage failed: {msg}");
                // Failed before reaching the model: the DOOR is down, not the
                // image. Hold the address and drop the rest of this batch.
                // `backend_down` (m3rc, 2026-09-05) means NO node is up — same thing.
                let door_down = !terminal
                    && (msg.contains("error sending request")
                        || msg.contains("connection")
                        || msg.contains("timed out")
                        || msg.contains("backend_down"));
                d.record_attempt(&store.entry.id, &item.id, stage, msg, terminal);
                if door_down {
                    d.stage_hold.lock().unwrap().insert(hold_key.clone(), Instant::now() + DOOR_DOWN_HOLD);
                    let next = if target.via == "primary" && ep.fallback.is_some() { "switching to fallback" } else { "waiting" };
                    tracing::warn!(stage, url = %target.url, via = target.via, "endpoint unreachable — holding it for {}s, {next}", DOOR_DOWN_HOLD.as_secs());
                    set.abort_all();
                }
            }
        }
    }
    if saw_busy {
        tokio::time::sleep(BUSY_WAIT).await;
    }
    Ok(n)
}

async fn run_one(
    d: &Daemon,
    store: &Arc<StoreHandle>,
    stage: &str,
    ep: &super::config::ModelEndpoint,
    t: &super::config::Target,
    item: &PendingItem,
) -> Result<()> {
    // pand is the one thing allowed to read the media file.
    let abs = store.pan.layout.abs(&item.media_path);
    let bytes = tokio::fs::read(&abs).await.with_context(|| format!("read {}", abs.display()))?;
    let media_type = if item.media_type.is_empty() { "image/png" } else { &item.media_type };

    match stage {
        STAGE_EMBED => {
            d.counters.model_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let r = d.iris.see_embed(t, &bytes, media_type).await?;
            let s = store.clone();
            let id = item.id.clone();
            let model = ep.model.clone();
            let caption_model = ep.caption_model.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                // One index per embedding model: the model name IS the index
                // name, so a second embedder never lands in the first one's
                // space and search defaults to whatever pand embeds with.
                // Everything the server said besides the vector rides along:
                // its HF model id, precision, provider … (m3rc's Salad answers
                // label themselves). precision/provider land on the record.
                s.pan.write_embedding(&id, &model, &model, &r.vector, &r.extra)?;
                if let (Some(cm), Some(text)) = (caption_model, r.caption.as_deref()) {
                    if !text.trim().is_empty() {
                        write_caption(&s, &id, &cm, text)?;
                    }
                }
                Ok(())
            })
            .await??;
        }
        STAGE_CAPTION => {
            // `/percept/vlm` (m3rc, 2026-09-05): image + prompt → text. The
            // prompt is config and required — Pan supplies it, the door never
            // does (Rob, 2026-09-05); the model recorded is the one the SERVER
            // names in its answer, falling back to config only if it is silent.
            let Some(prompt) = ep.prompt.as_deref().filter(|p| !p.trim().is_empty()) else {
                return Err(CallError::Terminal(format!("caption stage {} has no `prompt` in config; nothing was sent", ep.url)).into());
            };
            // The caption provider gets PIXELS ONLY: a same-size, high-quality
            // JPEG re-encoded from the stored image, so neither Horae's copia
            // block nor Pan's own XMP reaches a third-party model (Rob,
            // 2026-09-05; see `wire.rs`). This is Pan's job, not the door's.
            let wire = {
                let b = bytes.clone();
                tokio::task::spawn_blocking(move || crate::wire::caption_copy(&b)).await??
            };
            d.counters.model_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let r = d.iris.vlm(t, &ep.model, &wire.bytes, wire.media_type, prompt, ep.extra_body.as_ref()).await?;
            if r.text.trim().is_empty() {
                return Err(CallError::Terminal("no caption text returned".into()).into());
            }
            let s = store.clone();
            let id = item.id.clone();
            let model = r.model.clone().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| ep.model.clone());
            let text = r.text.clone();
            tokio::task::spawn_blocking(move || write_caption(&s, &id, &model, &text)).await??;
        }
        STAGE_POSE => {
            d.counters.model_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let r = d.iris.see_pose(t, &bytes, media_type).await?;
            if r.keypoints.is_empty() {
                // The eye reports "no people" and "I failed" the same way (200
                // {}). Record a zero-count run so the image is not asked
                // forever; the count says what was found.
                let s = store.clone();
                let id = item.id.clone();
                let model = ep.model.clone();
                tokio::task::spawn_blocking(move || {
                    s.pan.write_enrichment(&id, "pose", "pose", "poseData", &model, &[], None).map(|_| ())
                })
                .await??;
                return Ok(());
            }
            let overlay = r.skeleton_png()?;
            let s = store.clone();
            let id = item.id.clone();
            let model = ep.model.clone();
            let media_type_owned = media_type.to_string();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut overlay_rel: Option<String> = None;
                if let Some(png) = overlay {
                    let created = s
                        .pan
                        .facts_for(&id)?
                        .iter()
                        .find(|(p, _)| p == &format!("{}dateCreated", crate::GIT_LEX_NS))
                        .and_then(|(_, v)| v.first().cloned())
                        .unwrap_or_default();
                    let shard = created.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
                    let rel = crate::layout::PanLayout::derived_rel_path(
                        crate::layout::PanLayout::media_kind(&media_type_owned),
                        "pose",
                        &format!("{shard}/{id}.{model}.png"),
                    );
                    let abs = s.pan.layout.abs(&rel);
                    if let Some(p) = abs.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    crate::write_atomic(&abs, &png)?;
                    overlay_rel = Some(rel);
                }
                let records: Vec<EnrichmentRecord> = r
                    .keypoints
                    .iter()
                    .map(|person| {
                        let mut rec = EnrichmentRecord::new(gen_pan_id(), "Pose", &model)
                            .field("keypoints", iris::keypoints_literal(person));
                        if let Some(o) = &overlay_rel {
                            rec = rec.field("overlayPath", o);
                        }
                        rec
                    })
                    .collect();
                s.pan.write_enrichment(&id, "pose", "pose", "poseData", &model, &records, None)?;
                Ok(())
            })
            .await??;
        }
        STAGE_SAM3 => {
            // Prompts come from the caption's OBJECTS line. pending_for only
            // hands over images that have a caption, so an empty list here
            // means the caption listed nothing segmentable: record a
            // zero-region run so the image is not asked forever.
            let caption = store.pan.caption_of(&item.id)?.unwrap_or_default();
            let prompts = objects_from_caption(&caption);
            let s = store.clone();
            let id = item.id.clone();
            let model = ep.model.clone();
            if prompts.is_empty() {
                tokio::task::spawn_blocking(move || {
                    s.pan.write_enrichment(&id, "sam3", "region", "regionData", &model, &[], None).map(|_| ())
                })
                .await??;
                return Ok(());
            }
            d.counters.model_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (regions, raw) = d.iris.segment(t, &bytes, media_type, &prompts).await?;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let records: Vec<EnrichmentRecord> = regions
                    .iter()
                    .map(|r| {
                        let mut rec = EnrichmentRecord::new(gen_pan_id(), "Region", &model)
                            .field("descriptor", &r.prompt)
                            .field("score", format!("{:.4}", r.score));
                        if r.bbox.len() == 4 {
                            rec = rec.field("bbox", format!("{},{},{},{}", r.bbox[0], r.bbox[1], r.bbox[2], r.bbox[3]));
                        }
                        if let Some(p) = &r.polygon {
                            if !p.is_empty() {
                                rec = rec.field("polygon", p);
                            }
                        }
                        rec
                    })
                    .collect();
                let rel = s.pan.write_enrichment(&id, "sam3", "region", "regionData", &model, &records, None)?;
                // Everything the server said, verbatim, beside the record.
                let side = s.pan.layout.abs(&rel).with_extension("json");
                crate::write_atomic(&side, serde_json::to_string_pretty(&raw)?.as_bytes())?;
                Ok(())
            })
            .await??;
        }
        other => return Err(anyhow!("stage {other} is not runnable")),
    }
    Ok(())
}

/// The nouns a caption's `OBJECTS:` line lists — the prompt's own contract
/// ("ONLY the distinct, physically-segmentable things"). Comma-separated,
/// trimmed, lower-cased, de-duplicated, order kept. No line → nothing.
pub fn objects_from_caption(text: &str) -> Vec<String> {
    let Some(line) = text.lines().map(str::trim).find(|l| l.to_ascii_uppercase().starts_with("OBJECTS:")) else {
        return Vec::new();
    };
    let rest = &line["OBJECTS:".len()..];
    let mut out: Vec<String> = Vec::new();
    for raw in rest.split(',') {
        let n = raw.trim().trim_matches(|c: char| c == '.' || c == ';').trim().to_lowercase();
        if !n.is_empty() && n.len() <= 40 && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// One model's caption: a Caption record in its own data file, and the
/// image's current caption text set to it (the newest caption is the one a
/// viewer sees).
fn write_caption(s: &StoreHandle, id: &str, model: &str, text: &str) -> Result<()> {
    let rec = EnrichmentRecord::new(gen_pan_id(), "Caption", model).field("text", text);
    s.pan.write_enrichment(id, "caption", "captionItem", "captionData", model, std::slice::from_ref(&rec), Some(model))?;
    s.pan.set_caption(id, text)
}

#[cfg(test)]
mod objects_tests {
    use super::objects_from_caption;

    #[test]
    fn objects_line_becomes_prompts() {
        let text = "A woman kneels on mossy stone.\n\nOBJECTS: woman, hair, eyes, vines, moss, boots, boots, stones, stone arch, plants\n\nSCENE:\nsceneCamera: low angle";
        assert_eq!(
            objects_from_caption(text),
            ["woman", "hair", "eyes", "vines", "moss", "boots", "stones", "stone arch", "plants"]
        );
        assert!(objects_from_caption("no objects line here").is_empty());
        assert!(objects_from_caption("OBJECTS: ").is_empty());
    }
}

