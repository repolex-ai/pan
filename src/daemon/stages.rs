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
//!   sam3    `/segment`    → pan:Region per grounded prompt — HELD: needs a
//!                           ruling on where scene prompts live (see TODO doc)

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
        for stage in [STAGE_EMBED, STAGE_CAPTION, STAGE_POSE] {
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
    if let Some(until) = d.stage_hold.lock().unwrap().get(stage).copied() {
        if until > Instant::now() {
            return Ok(0);
        }
    }
    let ep = d.cfg.models.get(stage).cloned().ok_or_else(|| anyhow!("stage {stage} not configured"))?;
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
    let mut n = 0usize;
    for item in work {
        n += 1;
        let permit = d.funnels[stage].clone().acquire_owned().await?;
        let result = run_one(&d, &store, stage, &ep, &item).await;
        drop(permit);
        match result {
            Ok(()) => {
                d.clear_attempt(&store.entry.id, &item.id, stage);
                tracing::info!(store = %store.entry.id, id = %item.id, stage, model = %ep.model, "recorded");
            }
            Err(e) => {
                if let Some(CallError::Busy(m)) = e.downcast_ref::<CallError>() {
                    // Every node's queue is full. No attempt is recorded — the
                    // image stays pending and is simply asked again after a
                    // short breath. Stop walking this batch; the next pass
                    // starts from the top of the ladder again.
                    tracing::info!(store = %store.entry.id, id = %item.id, stage, "door busy, backing off {}s: {m}", BUSY_WAIT.as_secs());
                    tokio::time::sleep(BUSY_WAIT).await;
                    break;
                }
                let (msg, terminal) = match e.downcast_ref::<CallError>() {
                    Some(CallError::Terminal(m)) => (m.clone(), true),
                    Some(CallError::Transient(m)) => (m.clone(), false),
                    Some(CallError::Busy(m)) => (m.clone(), false),
                    None => (format!("{e:#}"), false),
                };
                tracing::warn!(store = %store.entry.id, id = %item.id, stage, terminal, "stage failed: {msg}");
                // Failed before reaching the model: the DOOR is down, not the
                // image. Hold the whole stage and stop walking this batch.
                // `backend_down` (m3rc, 2026-09-05) means NO node is up — same thing.
                let door_down = !terminal
                    && (msg.contains("error sending request")
                        || msg.contains("connection")
                        || msg.contains("timed out")
                        || msg.contains("backend_down"));
                d.record_attempt(&store.entry.id, &item.id, stage, msg, terminal);
                if door_down {
                    d.stage_hold.lock().unwrap().insert(stage.to_string(), Instant::now() + DOOR_DOWN_HOLD);
                    tracing::warn!(stage, url = %ep.url, "endpoint unreachable — holding this stage for {}s", DOOR_DOWN_HOLD.as_secs());
                    break;
                }
            }
        }
    }
    Ok(n)
}

async fn run_one(
    d: &Daemon,
    store: &Arc<StoreHandle>,
    stage: &str,
    ep: &super::config::ModelEndpoint,
    item: &PendingItem,
) -> Result<()> {
    // pand is the one thing allowed to read the media file.
    let abs = store.pan.layout.abs(&item.media_path);
    let bytes = tokio::fs::read(&abs).await.with_context(|| format!("read {}", abs.display()))?;
    let media_type = if item.media_type.is_empty() { "image/png" } else { &item.media_type };

    match stage {
        STAGE_EMBED => {
            d.counters.model_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let r = d.iris.see_embed(&ep.url, &bytes, media_type).await?;
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
            // prompt is config and required (Rob: "you need to provide prompt
            // for iris as well"); the model recorded is the one the SERVER
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
            let r = d.iris.vlm(&ep.url, &wire.bytes, wire.media_type, prompt, ep.extra_body.as_ref()).await?;
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
            let r = d.iris.see_pose(&ep.url, &bytes, media_type).await?;
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
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut overlay_rel: Option<String> = None;
                if let Some(png) = overlay {
                    let created = s
                        .pan
                        .facts_for(&id)?
                        .iter()
                        .find(|(p, _)| p == &format!("{}createdDate", crate::PAN_NS))
                        .and_then(|(_, v)| v.first().cloned())
                        .unwrap_or_default();
                    let shard = created.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
                    let rel = format!("pose/{shard}/{id}.{model}.png");
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
        other => return Err(anyhow!("stage {other} is not runnable")),
    }
    Ok(())
}

/// One model's caption: a Caption record in its own data file, and the
/// image's current caption text set to it (the newest caption is the one a
/// viewer sees).
fn write_caption(s: &StoreHandle, id: &str, model: &str, text: &str) -> Result<()> {
    let rec = EnrichmentRecord::new(gen_pan_id(), "Caption", model).field("text", text);
    s.pan.write_enrichment(id, "caption", "captionItem", "captionData", model, std::slice::from_ref(&rec), Some(model))?;
    s.pan.set_caption(id, text)
}
