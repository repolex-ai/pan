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
//! Stages are addressed only by the url in the config. The five Pan uses
//! today are Iris's, 2026-09-18: POST /percept/vlm (OpenAI chat completions),
//! and POST /percept/embed, /percept/pose, /percept/depth and
//! /percept/segment (an image upload, JSON back).
//!
//!   embed   → pan:Embedding and the vector index
//!   caption → pan:Caption, and the facts its JSON answer declares
//!   pose    → one pan:Pose per detected person + skeleton overlay
//!   sam3    → pan:Region per grounded prompt. The prompts are the nouns the
//!             caption model listed, so this stage waits for a caption. The
//!             whole server answer is kept as a .json beside the record.
//!   depth   → one pan:Depth per image: the map PNG and the node's sidecar
//!             beside the record.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::calllog::{CallLine, Meter};
use super::iris::{self, CallError};
use super::{Daemon, StoreHandle};
use crate::enrich::EnrichmentRecord;
use crate::{gen_pan_id, PendingItem};

pub const STAGE_EMBED: &str = "embed";
pub const STAGE_CAPTION: &str = "caption";
pub const STAGE_POSE: &str = "pose";
pub const STAGE_SAM3: &str = "sam3";
pub const STAGE_DEPTH: &str = crate::depth::STAGE;

/// Which graph link a stage's completion is read from: the data REFERENCE
/// (`pan:regionData`, …), not a record (records hang off the reference via `pan:item`). A run that
/// found nothing writes a reference with count 0 and no records, and it must
/// still count as done — keyed on records, an empty pose or segment run was
/// handed back every pass forever (seen 2026-09-07: hundreds of sam3 lines
/// per five minutes for one image).
pub fn link_for(stage: &str) -> Option<&'static str> {
    match stage {
        STAGE_EMBED => Some("vectorData"),
        STAGE_CAPTION => Some("captionData"),
        STAGE_POSE => Some("poseData"),
        STAGE_SAM3 => Some("regionData"),
        STAGE_DEPTH => Some(crate::depth::REF_LOCAL),
        _ => None,
    }
}

/// Run the ladder forever: ONE independent loop per enabled stage, plus one
/// for the ready mark. A stage that is slow (captions with thinking on), held
/// (its door down), or breathing (busy) delays nobody else — every other
/// stage keeps walking its own pending list (Rob, 2026-09-07). They share the
/// one writer: model calls are async and hold no lock; only the short write
/// after each answer touches the graph, and oxigraph serializes those.
pub async fn run(d: Arc<Daemon>) {
    let every = Duration::from_secs(d.cfg.interval_secs);
    let mut loops = tokio::task::JoinSet::new();
    for stage in [
        STAGE_EMBED,
        STAGE_CAPTION,
        STAGE_POSE,
        STAGE_SAM3,
        STAGE_DEPTH,
    ] {
        if !d.cfg.models.get(stage).map(|m| m.enabled).unwrap_or(false) {
            continue;
        }
        let d = d.clone();
        loops.spawn(async move {
            loop {
                let mut did = 0usize;
                for store in d.stores.clone() {
                    match run_stage(d.clone(), store.clone(), stage).await {
                        Ok(n) => did += n,
                        Err(e) => tracing::error!(store = %store.entry.id, stage, "stage pass failed: {e:#}"),
                    }
                }
                if did == 0 {
                    tokio::time::sleep(every).await;
                }
            }
        });
    }
    {
        let d = d.clone();
        loops.spawn(async move {
            loop {
                let did = mark_ready_pass(d.clone()).await;
                if did == 0 {
                    tokio::time::sleep(every).await;
                }
            }
        });
    }
    // These loops never return; if one ever does (a panic inside it), say so
    // loudly rather than run with a stage silently missing.
    while let Some(r) = loops.join_next().await {
        tracing::error!("a stage loop ended: {r:?} — pand should be restarted");
    }
}

/// One pass of the ready mark over every store: an object whose every
/// configured stage has a record is ready as configured; say when. With no
/// stages configured, ingest IS ready. Returns how many were marked.
pub async fn mark_ready_pass(d: Arc<Daemon>) -> usize {
    let mut done = 0usize;
    let required: Vec<(String, String)> = d
        .cfg
        .active_models()
        .filter_map(|(stage, ep)| link_for(stage).map(|l| (l.to_string(), ep.model.clone())))
        .collect();
    for store in d.stores.clone() {
        let s = store.clone();
        let req = required.clone();
        let batch = d.cfg.batch * 4;
        match tokio::task::spawn_blocking(move || -> Result<usize> {
            let mut n = 0;
            for id in s.pan.ready_candidates(&req, batch)? {
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

/// One pass of every stage over every store, in sequence. Kept for tests and
/// one-shot tools; the daemon runs [`run`], where stages are independent.
pub async fn run_pass(d: Arc<Daemon>) -> usize {
    let mut done = 0usize;
    for store in d.stores.clone() {
        for stage in [
            STAGE_EMBED,
            STAGE_CAPTION,
            STAGE_POSE,
            STAGE_SAM3,
            STAGE_DEPTH,
        ] {
            if !d.cfg.models.get(stage).map(|m| m.enabled).unwrap_or(false) {
                continue;
            }
            match run_stage(d.clone(), store.clone(), stage).await {
                Ok(n) => done += n,
                Err(e) => {
                    tracing::error!(store = %store.entry.id, stage, "stage pass failed: {e:#}")
                }
            }
        }
    }
    done + mark_ready_pass(d).await
}

/// How long a whole stage waits after a call failed before reaching the model
/// (connection refused / reset / timeout, or Iris on :1215 answering
/// `backend_down`). One try per hold; the server being down is not a fact
/// about the image. 5 s matches Iris's own roster refresh (`percept.refresh_s`,
/// 20 s -> 5 s, goodlux 2026-09-07): a failed call marks a Salad node down at
/// Iris until its next refresh, so that interval is the whole gap Pan sees.
const SERVER_DOWN_HOLD: Duration = Duration::from_secs(5);

/// How long a whole stage waits after the provider answers `402`: the account
/// is out of credit, and credit does not come back in seconds. One try per
/// hold, so the log says so every ten minutes instead of every two seconds
/// (2026-09-08: 667 calls against an empty Phala account in 80 minutes).
const QUOTA_HOLD: Duration = Duration::from_secs(600);

/// How long a stage breathes after `503 busy` (every node's queue full) before
/// its next pass. Seconds, not minutes: Iris asks for a retry in seconds.
const BUSY_WAIT: Duration = Duration::from_secs(5);

async fn run_stage(d: Arc<Daemon>, store: Arc<StoreHandle>, stage: &'static str) -> Result<usize> {
    let ep = d
        .cfg
        .models
        .get(stage)
        .cloned()
        .ok_or_else(|| anyhow!("stage {stage} not configured"))?;
    // Which address this pass calls. A hold on the primary sends the stage to
    // its fallback (if it has one); a hold on both means wait. The primary is
    // probed again the moment its hold expires, so Iris gets the traffic
    // back as soon as it is up.
    let held = |key: &str| -> bool {
        crate::locked(&d.stage_hold)
            .get(key)
            .map(|u| *u > Instant::now())
            .unwrap_or(false)
    };
    let fallback_key = format!("{stage}/fallback");
    let target = if !held(stage) {
        ep.primary()
    } else if let Some(fb) = ep.fallback_target().filter(|_| !held(&fallback_key)) {
        fb
    } else {
        return Ok(0);
    };
    let hold_key: String = if target.via == "fallback" {
        fallback_key
    } else {
        stage.to_string()
    };
    let link = link_for(stage).ok_or_else(|| anyhow!("unknown stage {stage}"))?;
    let batch = d.cfg.batch;
    // Ask for more than the batch so items on hold do not starve the ones
    // behind them; then take the first `batch` that are not holding.
    let pending: Vec<PendingItem> = {
        let s = store.clone();
        let model = ep.model.clone();
        let since = d.cfg.backfill_since.clone();
        tokio::task::spawn_blocking(move || {
            s.pan.pending_for(link, &model, batch * 4, since.as_deref())
        })
        .await??
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
            // The client measures the call into `meter`; the outcome is only
            // known here, after the write — so the log line is written where
            // both meet, below. No model call = nothing measured = no line.
            let meter = Meter::new();
            let result = run_one(&d, &store, stage, &ep, &target, &item, &meter).await;
            drop(permit);
            (item, result, meter.take())
        });
    }
    let mut saw_busy = false;
    while let Some(joined) = set.join_next().await {
        let (item, result, meta) = match joined {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(stage, "stage task join: {e}");
                continue;
            }
        };
        // One line in the model-call log per call that reached the client.
        let log_call = |outcome: &str, error: Option<&str>| {
            if let Some(m) = &meta {
                d.calls.record(&CallLine {
                    time: CallLine::now(),
                    store: &store.entry.id,
                    id: &item.id,
                    stage,
                    model: &ep.model,
                    url: &m.url,
                    via: target.via,
                    request_bytes: m.request_bytes,
                    status: m.status,
                    latency_ms: m.latency_ms,
                    response_bytes: m.response_bytes,
                    outcome,
                    error,
                    finish_reason: m.finish_reason.as_deref(),
                    prompt_tokens: m.prompt_tokens,
                    completion_tokens: m.completion_tokens,
                });
            }
        };
        match result {
            Ok(()) => {
                d.clear_attempt(&store.entry.id, &item.id, stage);
                d.funnels[stage].on_success();
                log_call("recorded", None);
                tracing::info!(store = %store.entry.id, id = %item.id, stage, model = %ep.model, via = target.via, window = d.funnels[stage].window(), "recorded");
            }
            Err(e) => {
                if let Some(CallError::Busy(m)) = e.downcast_ref::<CallError>() {
                    // Every node's queue is full: the window was too wide.
                    // Narrow it; no attempt is recorded, the image stays
                    // pending and is asked again next pass.
                    saw_busy = true;
                    d.funnels[stage].on_busy();
                    log_call("busy", Some(m));
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
                // Failed before reaching the model: the SERVER is down, not the
                // image. Hold the address and drop the rest of this batch.
                // `backend_down` (m3rc, 2026-09-05) means NO node is up — same thing.
                let server_down = !terminal
                    && (msg.contains("error sending request")
                        || msg.contains("connection")
                        || msg.contains("timed out")
                        || msg.contains("backend_down"));
                let quota = !terminal && msg.contains("402 quota exceeded");
                let outcome = if terminal {
                    "terminal"
                } else if quota {
                    "quota"
                } else if server_down {
                    "backend_down"
                } else {
                    "transient"
                };
                log_call(outcome, Some(&msg));
                d.record_attempt(&store.entry.id, &item.id, stage, msg, terminal);
                if quota {
                    crate::locked(&d.stage_hold)
                        .insert(hold_key.clone(), Instant::now() + QUOTA_HOLD);
                    tracing::warn!(stage, url = %target.url, "provider account out of credit — holding the stage for {}s; add credits at the provider", QUOTA_HOLD.as_secs());
                    set.abort_all();
                } else if server_down {
                    crate::locked(&d.stage_hold)
                        .insert(hold_key.clone(), Instant::now() + SERVER_DOWN_HOLD);
                    let next = if target.via == "primary" && ep.fallback.is_some() {
                        "switching to fallback"
                    } else {
                        "waiting"
                    };
                    tracing::warn!(stage, url = %target.url, via = target.via, "endpoint unreachable — holding it for {}s, {next}", SERVER_DOWN_HOLD.as_secs());
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
    meter: &Meter,
) -> Result<()> {
    // pand is the one thing allowed to read the media file.
    let abs = store.pan.layout.abs(&item.media_path);
    let bytes = tokio::fs::read(&abs)
        .await
        .with_context(|| format!("read {}", abs.display()))?;
    let media_type = if item.media_type.is_empty() {
        "image/png"
    } else {
        &item.media_type
    };

    match stage {
        STAGE_EMBED => {
            // The embedding is multimodal: the image AND the complete XMP
            // packet in the file, embedded together as one vector (goodlux,
            // 2026-09-08). pending_for holds an image back until the caption
            // stage has written its fields, so the packet carries them.
            let packet = crate::xmp::read_xmp_packet_from_bytes(&bytes)?.unwrap_or_default();
            d.counters
                .model_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let r = d.iris.embed(t, &bytes, media_type, &packet, meter).await?;
            let s = store.clone();
            let id = item.id.clone();
            let model = ep.model.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                // One index per embedding model: the model name IS the index
                // name, so a second embedder never lands in the first one's
                // space and search defaults to whatever pand embeds with.
                // Everything the server said besides the vector rides along:
                // its HF model id, precision, provider … (m3rc's Salad answers
                // label themselves). precision/provider land on the record.
                s.pan
                    .write_embedding(&id, &model, &model, &r.vector, &r.extra)
                    .map(|_| ())
            })
            .await??;
        }
        STAGE_CAPTION => {
            // `/percept/vlm` (m3rc, 2026-09-05): image + prompt → text. The
            // prompt is config and required — Pan supplies it, Iris never
            // does (Rob, 2026-09-05); the model recorded is the one the SERVER
            // names in its answer, falling back to config only if it is silent.
            let Some(prompt) = ep.prompt.as_deref().filter(|p| !p.trim().is_empty()) else {
                return Err(CallError::Terminal(format!(
                    "caption stage {} has no `prompt` in config; nothing was sent",
                    ep.url
                ))
                .into());
            };
            // The caption provider gets PIXELS ONLY: a same-size, high-quality
            // JPEG re-encoded from the stored image, so neither Horae's copia
            // block nor Pan's own XMP reaches a third-party model (Rob,
            // 2026-09-05; see `wire.rs`). This is Pan's job, not Iris's.
            let wire = {
                let b = bytes.clone();
                tokio::task::spawn_blocking(move || crate::wire::caption_copy(&b)).await??
            };
            d.counters
                .model_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let r = d
                .iris
                .vlm(
                    t,
                    &ep.model,
                    &wire.bytes,
                    wire.media_type,
                    prompt,
                    ep.extra_body.as_ref(),
                    meter,
                )
                .await?;
            if r.text.trim().is_empty() {
                return Err(CallError::Terminal("no caption text returned".into()).into());
            }
            // The answer is one JSON object keyed by property name (pan.ttl
            // 0.3.4). A key the ontology does not declare fails this image
            // for good: the prompt is the schema, and a wrong prompt is a
            // config error, not something to retry.
            let mut perception = crate::Perception::parse(&r.text).map_err(|e| {
                CallError::Terminal(caption_failure_message(
                    &e,
                    r.finish_reason.as_deref(),
                    r.prompt_tokens,
                    r.completion_tokens,
                    &r.text,
                ))
            })?;
            // Which prompt asked for this answer, recorded on the object and
            // on the Caption record (goodlux, 2026-09-19).
            perception.prompt_path = ep.prompt_path.clone().unwrap_or_default();
            let s = store.clone();
            let id = item.id.clone();
            // The name is what gets recorded. The server answers with its own
            // string (`qwen/qwen3.8-27b`); that is the request's business, not
            // a second name for the model (goodlux, 2026-09-18). A mismatch is
            // worth knowing about, so it is logged, not written down.
            let model = ep.model.clone();
            let text = r.text.clone();
            tokio::task::spawn_blocking(move || {
                write_perception(&s, &id, &model, &text, &perception)
            })
            .await??;
        }
        STAGE_POSE => {
            d.counters
                .model_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let r = d.iris.pose(t, &bytes, media_type, meter).await?;
            if r.keypoints.is_empty() {
                // The eye reports "no people" and "I failed" the same way (200
                // {}). Record a zero-count run so the image is not asked
                // forever; the count says what was found.
                let s = store.clone();
                let id = item.id.clone();
                let model = ep.model.clone();
                tokio::task::spawn_blocking(move || {
                    s.pan
                        .write_enrichment(&id, "pose", "poseData", &model, &[], Default::default())
                        .map(|_| ())
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
                        .find(|(p, _)| p == &format!("{}createdDate", crate::PAN_NS))
                        .and_then(|(_, v)| v.first().cloned())
                        .unwrap_or_default();
                    let shard = created.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
                    let rel = crate::layout::PanLayout::overlay_rel_path(
                        crate::layout::PanLayout::media_kind(&media_type_owned),
                        "pose",
                        &shard,
                        &id,
                        &model,
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
                s.pan.write_enrichment(
                    &id,
                    "pose",
                    "poseData",
                    &model,
                    &records,
                    Default::default(),
                )?;
                Ok(())
            })
            .await??;
        }
        STAGE_SAM3 => {
            // What this stage grounds: the nouns the caption model listed,
            // plus the nouns the config says to ask for every time (goodlux,
            // 2026-09-19). A caption that never says "person" used to leave a
            // photograph of people with no person region; now person and face
            // are found because Pan asked for them.
            let mut prompts = store.pan.scene_objects_of(&item.id)?;
            for noun in &ep.always {
                if !prompts.iter().any(|p| p == noun) {
                    prompts.push(noun.clone());
                }
            }
            let s = store.clone();
            let id = item.id.clone();
            let model = ep.model.clone();
            if prompts.is_empty() {
                tokio::task::spawn_blocking(move || {
                    s.pan
                        .write_enrichment(
                            &id,
                            "sam3",
                            "regionData",
                            &model,
                            &[],
                            Default::default(),
                        )
                        .map(|_| ())
                })
                .await??;
                return Ok(());
            }
            d.counters
                .model_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (regions, raw) = d
                .iris
                .segment(t, &bytes, media_type, &prompts, meter)
                .await?;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let records: Vec<EnrichmentRecord> = regions
                    .iter()
                    .map(|r| {
                        let mut rec = EnrichmentRecord::new(gen_pan_id(), "Region", &model)
                            .field("descriptor", &r.prompt)
                            .field("score", format!("{:.4}", r.score));
                        if r.bbox.len() == 4 {
                            rec = rec.field(
                                "bbox",
                                format!("{},{},{},{}", r.bbox[0], r.bbox[1], r.bbox[2], r.bbox[3]),
                            );
                        }
                        if let Some(p) = &r.polygon {
                            if !p.is_empty() {
                                rec = rec.field("polygon", p);
                            }
                        }
                        rec
                    })
                    .collect();
                // Everything the server said, verbatim, beside the record,
                // and named on the reference as pan:modelReplyPath (goodlux,
                // 2026-09-19) so the graph knows the file exists.
                let record_rel = s.pan.enrichment_rel(&id, "sam3", None)?;
                let answer_rel = format!("{record_rel}.json");
                let side = s.pan.layout.abs(&answer_rel);
                if let Some(parent) = side.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::write_atomic(&side, serde_json::to_string_pretty(&raw)?.as_bytes())?;
                s.pan.write_enrichment(
                    &id,
                    "sam3",
                    "regionData",
                    &model,
                    &records,
                    crate::RecordFile::default().with_model_reply(&answer_rel),
                )?;
                Ok(())
            })
            .await??;
        }
        STAGE_DEPTH => {
            // One map per image, always: there is no "found nothing" for
            // depth, so an empty answer is the node failing, and the image
            // stays pending (transient) rather than being retired.
            d.counters
                .model_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let answer = d.iris.depth(t, &bytes, media_type, meter).await?;
            if answer.is_empty() {
                return Err(
                    CallError::Transient("depth: node answered without a map".into()).into(),
                );
            }
            let s = store.clone();
            let id = item.id.clone();
            let model = ep.model.clone();
            tokio::task::spawn_blocking(move || {
                s.pan.write_depth(&id, &model, &answer).map(|_| ())
            })
            .await??;
        }
        other => return Err(anyhow!("stage {other} is not runnable")),
    }
    Ok(())
}

/// The model's whole answer goes into the Caption record verbatim (save
/// everything); the parsed fields go onto the object.
fn write_perception(
    s: &StoreHandle,
    id: &str,
    model: &str,
    raw: &str,
    p: &crate::Perception,
) -> Result<()> {
    // The prompt that asked for this answer rides on the record as well as on
    // the object (goodlux, 2026-09-19): a second captioning model, or the same
    // one with a different prompt, is a second record naming its own prompt.
    let mut rec = EnrichmentRecord::new(gen_pan_id(), "Caption", model).field("text", raw);
    if !p.prompt_path.trim().is_empty() {
        rec = rec.field("modelPromptPath", &p.prompt_path);
    }
    s.pan.write_enrichment(
        id,
        "caption",
        "captionData",
        model,
        std::slice::from_ref(&rec),
        crate::RecordFile::variant(model),
    )?;
    s.pan.set_perception(id, p)
}

/// The `stage failed: caption answer: …` text when the model's reply did not
/// parse: the parser's own words, then what the server said about the reply
/// (why it stopped, how many tokens in and out) and the first 200 characters
/// of the content. Asked for by m3rc (2026-09-18): `length` means the 2048
/// token window ran out; `stop` with prose means sampling wandered.
fn caption_failure_message(
    parse_error: &str,
    finish_reason: Option<&str>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    content: &str,
) -> String {
    let opt_s = |v: Option<&str>| v.unwrap_or("none").to_string();
    let opt_n = |v: Option<u64>| v.map_or_else(|| "none".to_string(), |n| n.to_string());
    let head: String = content.chars().take(200).collect();
    format!(
        "caption answer: {parse_error} (finish_reason={}, prompt_tokens={}, completion_tokens={}, content[..200]={head:?})",
        opt_s(finish_reason),
        opt_n(prompt_tokens),
        opt_n(completion_tokens),
    )
}

#[cfg(test)]
mod caption_failure_tests {
    use super::caption_failure_message;

    #[test]
    fn a_cut_off_answer_names_the_window_and_shows_the_head() {
        let prose = "The image shows a woman standing on a cliff at dusk, her coat ".repeat(6);
        let e = crate::Perception::parse(&prose).unwrap_err();
        assert_eq!(e, "answer has no JSON object");
        let msg = caption_failure_message(&e, Some("length"), Some(1811), Some(237), &prose);
        assert!(
            msg.starts_with("caption answer: answer has no JSON object ("),
            "{msg}"
        );
        assert!(msg.contains("finish_reason=length"), "{msg}");
        assert!(msg.contains("prompt_tokens=1811"), "{msg}");
        assert!(msg.contains("completion_tokens=237"), "{msg}");
        let head: String = prose.chars().take(200).collect();
        assert!(msg.contains(&format!("content[..200]={head:?}")), "{msg}");
        assert!(
            !msg.contains(&prose),
            "only the first 200 characters ride in the message"
        );
    }

    #[test]
    fn missing_usage_says_none_not_zero() {
        let msg = caption_failure_message("answer is not a JSON object", None, None, None, "[]");
        assert!(
            msg.contains("finish_reason=none, prompt_tokens=none, completion_tokens=none"),
            "{msg}"
        );
    }
}
