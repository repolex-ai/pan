//! Migration: a Pool store → a Pan store.
//!
//! Pool is the system Pan replaces. Its media carries four ERAS of metadata in
//! one tree (the shapes drifted as models and code changed), so this importer
//! is also the honest test of whether Pan's vocabulary has a home for
//! everything real data contains.
//!
//! What it does per image, in order:
//!   1. read the source bytes and its XMP packet;
//!   2. assign a fresh pan id (identity is assigned, never derived — two
//!      imports of the same bytes would be two objects, hence the re-run guard
//!      on `pan:importedFrom`);
//!   3. translate what Pan has vocabulary for — captions (one record per
//!      producing model), regions, poses, pixel dimensions — into pan records;
//!   4. write each enricher's records to its own DATA FILE beside the blob,
//!      and one REFERENCE per file into the image's packet;
//!   5. carry the application's own block (copia:, and the older mflux:/dc:/
//!      photoshop: blocks) through untouched, into both the image and the graph;
//!   6. copy the bytes into Pan's store with the new packet written in, and
//!      bring across the vector and pose-overlay files.
//!
//! WHAT IT DELIBERATELY DOES NOT DO: invent data. A field with no home in
//! pan: stays in its application block rather than being force-fitted; a
//! missing enrichment produces no reference at all, so "absent" and "empty"
//! never render alike.

use anyhow::{anyhow, Context, Result};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::PAN_NS;
use crate::enrich::{build_data_file, record_quads, ref_quads, EnrichmentRecord, EnrichmentRef};
use crate::layout::PanLayout;
use crate::{gen_pan_id, media_class, media_subject_iri, npy, xmp, Pan};

const COPIA_NS: &str = "https://repolex.ai/ontology/copia/";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Caption predicates Pool used, and the model each one names.
///
/// This is the migration's headline exhibit: the field name CHANGED when the
/// model changed (`qwenvl8bCaption` → `qwen35vl9bCaption`), so a query for
/// captions had to know the model roster. In Pan the model is data on a
/// Caption record and the vocabulary never moves again.
const CAPTION_FIELDS: &[(&str, &str)] = &[
    ("qwenvl8bCaption", "qwen-vl-8b"),
    ("qwen35vl9bCaption", "qwen3.5-vl-9b"),
    ("qwen3vl8bCaption", "qwen3-vl-8b"),
];

#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// The Pool store root (the directory holding `blob/`, `poses/`, `vectors/`).
    pub source_root: PathBuf,
    /// Stop after this many images overall.
    pub limit: Option<usize>,
    /// Take at most this many images from each YYYY/MM — the era sample, so a
    /// short run still exercises every metadata shape in the source.
    pub per_month: Option<usize>,
    /// Report what would happen; write nothing.
    pub dry_run: bool,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ImportStats {
    pub scanned: usize,
    pub imported: usize,
    pub skipped_already: usize,
    pub failed: usize,
    pub regions: usize,
    pub poses: usize,
    pub captions: usize,
    pub vectors: usize,
    pub overlays: usize,
    pub by_month: BTreeMap<String, usize>,
    /// Source fields with no pan: home, counted by name — the review list.
    /// Passthrough is not a failure, but an unexpected name showing up here in
    /// bulk means the vocabulary is missing something.
    pub passthrough_fields: BTreeMap<String, usize>,
    pub warnings: Vec<String>,
}

/// One source image and the sidecar files that belong to it.
struct SourceImage {
    path: PathBuf,
    /// `YYYY/MM/DD` shard the source filed it under.
    shard: String,
    /// The source filename without extension — Pool's association key.
    stem: String,
}

/// Walk the Pool blob tree. Only dated shards are taken: `blob/image/observer`
/// holds conversation summaries rather than stored media, and `.tmp` is
/// in-flight work.
fn scan_source(root: &Path, opts: &ImportOptions) -> Result<Vec<SourceImage>> {
    let blob_root = root.join("blob/image");
    let mut per_month: BTreeMap<String, Vec<SourceImage>> = BTreeMap::new();

    for year in read_dirs(&blob_root)? {
        let yname = file_name(&year);
        if !yname.chars().all(|c| c.is_ascii_digit()) {
            continue; // observer/, .tmp/ — not dated media
        }
        for month in read_dirs(&year)? {
            let mname = file_name(&month);
            for day in read_dirs(&month)? {
                let dname = file_name(&day);
                let shard = format!("{yname}/{mname}/{dname}");
                for entry in std::fs::read_dir(&day).with_context(|| format!("read {}", day.display()))? {
                    let p = entry?.path();
                    if p.extension().and_then(|e| e.to_str()) != Some("png") {
                        continue;
                    }
                    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
                    if stem.is_empty() {
                        continue;
                    }
                    per_month
                        .entry(format!("{yname}/{mname}"))
                        .or_default()
                        .push(SourceImage { path: p, shard: shard.clone(), stem });
                }
            }
        }
    }

    let mut out = Vec::new();
    for (_, mut images) in per_month {
        images.sort_by(|a, b| a.stem.cmp(&b.stem));
        // Spread the sample across the month rather than taking the first N,
        // so a month whose shape changed mid-way is represented on both sides.
        if let Some(n) = opts.per_month {
            if images.len() > n && n > 0 {
                let step = images.len() / n;
                images = images.into_iter().step_by(step.max(1)).take(n).collect();
            }
        }
        out.extend(images);
    }
    out.sort_by(|a, b| a.stem.cmp(&b.stem));
    if let Some(limit) = opts.limit {
        out.truncate(limit);
    }
    Ok(out)
}

fn read_dirs(p: &Path) -> Result<Vec<PathBuf>> {
    if !p.is_dir() {
        return Ok(vec![]);
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .with_context(|| format!("read {}", p.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|e| e.is_dir())
        .collect();
    v.sort();
    Ok(v)
}

fn file_name(p: &Path) -> String {
    p.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_string()
}

/// `20260817-202652-43cf3ec9` → `2026-08-17T20:26:52+00:00`.
///
/// Used only when the packet carries no render timestamp. A migrated store
/// keeps the SOURCE's chronology: importing must never restamp 80,000 images
/// with today's date and destroy the timeline.
fn created_at_from_stem(stem: &str) -> Option<String> {
    let (date, rest) = stem.split_once('-')?;
    let time = rest.split('-').next()?;
    if date.len() != 8 || time.len() != 6 || !date.chars().chain(time.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "{}-{}-{}T{}:{}:{}+00:00",
        &date[0..4], &date[4..6], &date[6..8], &time[0..2], &time[2..4], &time[4..6]
    ))
}

/// Normalize Pool's render timestamp to RFC3339 seconds, or fall back.
fn normalize_timestamp(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    // Pool wrote e.g. 2026-08-17T20:26:50.100416+00:00 — trim sub-seconds so
    // every migrated object carries the one agreed shape.
    let cut = match (t.find('.'), t.find('+')) {
        (Some(dot), Some(plus)) if dot < plus => format!("{}{}", &t[..dot], &t[plus..]),
        _ => t.to_string(),
    };
    Some(cut)
}

/// Find the source vector file for a stem, across both layouts Pool used
/// (dated shards, and an older flat directory).
fn find_vector(source_root: &Path, index: &str, shard: &str, stem: &str) -> Option<PathBuf> {
    let base = source_root.join("vectors").join(index);
    let dated = base.join(shard).join(format!("{stem}.npy"));
    if dated.exists() {
        return Some(dated);
    }
    let flat = base.join(format!("{stem}.npy"));
    if flat.exists() {
        return Some(flat);
    }
    None
}

fn vector_index_names(source_root: &Path) -> Vec<String> {
    read_dirs(&source_root.join("vectors"))
        .unwrap_or_default()
        .iter()
        .map(|p| file_name(p))
        .collect()
}

/// Run the migration.
pub fn import_pool(pan: &Pan, opts: &ImportOptions) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let images = scan_source(&opts.source_root, opts)?;
    stats.scanned = images.len();
    let indexes = vector_index_names(&opts.source_root);

    for img in &images {
        let month = img.shard[..7].to_string();
        match import_one(pan, opts, img, &indexes, &mut stats) {
            Ok(true) => {
                stats.imported += 1;
                *stats.by_month.entry(month).or_default() += 1;
            }
            Ok(false) => stats.skipped_already += 1,
            Err(e) => {
                stats.failed += 1;
                stats.warnings.push(format!("{}: {e:#}", img.path.display()));
            }
        }
    }
    pan.flush()?;
    Ok(stats)
}

fn import_one(
    pan: &Pan,
    opts: &ImportOptions,
    img: &SourceImage,
    indexes: &[String],
    stats: &mut ImportStats,
) -> Result<bool> {
    let source_key = img.path.to_string_lossy().to_string();
    if pan.imported_from(&source_key)? {
        return Ok(false);
    }

    let bytes = std::fs::read(&img.path).with_context(|| format!("read {}", img.path.display()))?;
    if !xmp::is_png(&bytes) {
        return Err(anyhow!("not a PNG"));
    }

    // ── Read the source's own description. ──────────────────────────────────
    // A packet Pan cannot parse must not silently become an image with no
    // metadata: the bytes are still worth having, but the gap is reported.
    let packet = xmp::read_xmp_packet_from_bytes(&bytes)?;
    let blocks = match &packet {
        Some(p) => match xmp::parse_packet(p) {
            Ok(b) => b,
            Err(e) => {
                stats.warnings.push(format!("{}: unparseable XMP ({e:#}) — bytes imported bare", img.stem));
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    // Root facts: predicate IRI → values.
    let mut root: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for b in blocks.iter().filter(|b| b.subject.is_none()) {
        for (pred, values) in &b.facts {
            root.entry(pred.clone())
                .or_default()
                .extend(values.iter().map(|v| v.value().to_string()));
        }
    }
    let copia = |local: &str| -> Option<String> {
        root.get(&format!("{COPIA_NS}{local}")).and_then(|v| v.first().cloned())
    };

    // ── Identity + timing. ──────────────────────────────────────────────────
    let pan_id = pan.new_id()?;
    let media_type = "image/png".to_string();
    let subject = media_subject_iri(&media_type, &pan_id)?;
    let created_at = copia("renderTimestamp")
        .and_then(|t| normalize_timestamp(&t))
        .or_else(|| created_at_from_stem(&img.stem))
        .unwrap_or_else(|| format!("{}T00:00:00+00:00", img.shard.replace('/', "-")));
    let shard = created_at.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
    let blob_rel = format!("{}/{shard}/{pan_id}.png", PanLayout::BLOB_SUBPATH);

    let mut quads: Vec<Quad> = vec![
        Quad::new(
            subject.clone(),
            NamedNode::new(RDF_TYPE).expect("rdf:type"),
            NamedNode::new(format!("{PAN_NS}{}", media_class(&media_type)))?,
            GraphName::DefaultGraph,
        ),
        pan_lit(&subject, "id", &pan_id)?,
        pan_lit(&subject, "blobPath", &blob_rel)?,
        pan_lit(&subject, "mediaType", &media_type)?,
        pan_lit(&subject, "createdAt", &created_at)?,
        pan_lit(&subject, "importedFrom", &source_key)?,
    ];

    // Pixel dimensions: generic facts about the image, so they get a pan home
    // (they were application-namespaced in the source — the clearest case of a
    // fact filed under the wrong owner).
    let width = copia("width").and_then(|v| v.parse::<u32>().ok());
    let height = copia("height").and_then(|v| v.parse::<u32>().ok());
    if let Some(w) = width {
        quads.push(pan_lit(&subject, "width", &w.to_string())?);
    }
    if let Some(h) = height {
        quads.push(pan_lit(&subject, "height", &h.to_string())?);
    }

    // ── Captions: one record per producing model. ───────────────────────────
    let current_caption = copia("caption");
    if let Some(c) = &current_caption {
        quads.push(pan_lit(&subject, "caption", c)?);
    }
    let mut enrichment: Vec<(String, Vec<EnrichmentRef>)> = Vec::new();
    let mut caption_refs: Vec<EnrichmentRef> = Vec::new();
    for (field, model) in CAPTION_FIELDS {
        let Some(text) = copia(field) else { continue };
        if text.trim().is_empty() {
            continue;
        }
        let rec = EnrichmentRecord::new(gen_pan_id(), "Caption", *model).field("text", &text);
        let rel = PanLayout::enrichment_rel_path("caption", &shard, &pan_id, Some(model));
        if !opts.dry_run {
            write_data_file(pan, &rel, &subject.as_str().to_string(), "captionItem", std::slice::from_ref(&rec))?;
        }
        quads.extend(record_quads(subject.as_str(), "captionItem", std::slice::from_ref(&rec))?);
        caption_refs.push(EnrichmentRef {
            id: gen_pan_id(),
            model: (*model).to_string(),
            path: rel,
            count: 1,
        });
        stats.captions += 1;
    }

    // ── Regions and poses: from the source's sub-subject descriptions. ──────
    let mut regions: Vec<EnrichmentRecord> = Vec::new();
    let mut poses: Vec<EnrichmentRecord> = Vec::new();
    for b in blocks.iter().filter(|b| b.subject.is_some()) {
        let about = b.subject.clone().unwrap_or_default();
        let get = |local: &str| -> String {
            b.facts
                .iter()
                .find(|(p, _)| p == &format!("{COPIA_NS}{local}"))
                .and_then(|(_, v)| v.first().map(|t| t.value().to_string()))
                .unwrap_or_default()
        };
        if about.starts_with("Sam3Region:") {
            regions.push(
                EnrichmentRecord::new(gen_pan_id(), "Region", "sam3")
                    .field("descriptor", get("regionDescriptor"))
                    .field("polygon", get("regionPolygon"))
                    .field("bbox", get("regionBbox"))
                    .field("score", get("regionScore")),
            );
        } else if about.starts_with("PoseDetection:") {
            let detector = get("poseDetector");
            let mut rec = EnrichmentRecord::new(gen_pan_id(), "Pose", &detector)
                .field("keypoints", get("poseKeypoints"));
            // The overlay path lives on the POSE description, not the root, and
            // the source wrote it absolute in some eras and relative in others.
            rec.fields.push(("__sidecar".into(), get("poseSidecar")));
            poses.push(rec);
        }
    }

    if !regions.is_empty() {
        let rel = PanLayout::enrichment_rel_path("sam3", &shard, &pan_id, None);
        if !opts.dry_run {
            write_data_file(pan, &rel, &subject.as_str().to_string(), "region", &regions)?;
        }
        quads.extend(record_quads(subject.as_str(), "region", &regions)?);
        enrichment.push((
            "regionData".to_string(),
            vec![EnrichmentRef { id: gen_pan_id(), model: "sam3".into(), path: rel, count: regions.len() }],
        ));
        stats.regions += regions.len();
    }

    if !poses.is_empty() {
        // The source rendered a skeleton overlay image; bring it across so the
        // migrated store is self-contained, and point the records at ITS copy.
        let model = poses[0].model.clone();
        let overlay_rel = format!("poses/{shard}/{pan_id}.png");
        let mut poses = poses;

        // Lift the carried sidecar path off the records, resolve it, and bring
        // the overlay image into Pan's store so the migrated store is
        // self-contained. Every record then points at PAN's copy.
        let sidecar = poses
            .iter()
            .find_map(|p| p.fields.iter().find(|(k, _)| k == "__sidecar").map(|(_, v)| v.clone()))
            .filter(|s| !s.is_empty());
        for p in poses.iter_mut() {
            p.fields.retain(|(k, _)| k != "__sidecar");
        }
        if let Some(raw) = sidecar {
            match resolve_source_path(&opts.source_root, &raw) {
                Some(src_overlay) => {
                    if !opts.dry_run {
                        copy_into_store(pan, &src_overlay, &overlay_rel)?;
                    }
                    stats.overlays += 1;
                    for p in poses.iter_mut() {
                        p.fields.push(("overlayPath".into(), overlay_rel.clone()));
                    }
                }
                // The source names a file that isn't there. Say so: a pose
                // whose overlay is missing must not look like one that never
                // had an overlay.
                None => stats
                    .warnings
                    .push(format!("{}: pose overlay not found at {raw}", img.stem)),
            }
        }
        let rel = PanLayout::enrichment_rel_path("pose", &shard, &pan_id, None);
        if !opts.dry_run {
            write_data_file(pan, &rel, &subject.as_str().to_string(), "pose", &poses)?;
        }
        quads.extend(record_quads(subject.as_str(), "pose", &poses)?);
        enrichment.push((
            "poseData".to_string(),
            vec![EnrichmentRef { id: gen_pan_id(), model, path: rel, count: poses.len() }],
        ));
        stats.poses += poses.len();
    }

    if !caption_refs.is_empty() {
        enrichment.push(("captionData".to_string(), caption_refs));
    }

    // ── Vectors: the crown jewel's fuel. ────────────────────────────────────
    let mut vector_refs: Vec<EnrichmentRef> = Vec::new();
    for index in indexes {
        let Some(src) = find_vector(&opts.source_root, index, &img.shard, &img.stem) else { continue };
        let vec = match npy::read_f32_1d(&src) {
            Ok(v) => v,
            Err(e) => {
                stats.warnings.push(format!("{}: vector unreadable ({e:#})", img.stem));
                continue;
            }
        };
        let dim = vec.len();
        if !opts.dry_run {
            // add_vector writes the .npy into Pan's own layout AND indexes it,
            // so the migrated store is searchable, not merely populated.
            pan.add_vector(&pan_id, index, &vec)
                .with_context(|| format!("index vector for {pan_id}"))?;
        }
        let rel = format!("{}/{index}/{pan_id}.npy", PanLayout::VECTORS_SUBDIR);
        let rec = EnrichmentRecord::new(gen_pan_id(), "Embedding", index)
            .field("dim", dim.to_string())
            .field("vectorPath", &rel);
        quads.extend(record_quads(subject.as_str(), "embedding", std::slice::from_ref(&rec))?);
        vector_refs.push(EnrichmentRef { id: gen_pan_id(), model: index.clone(), path: rel, count: 1 });
        stats.vectors += 1;
    }
    if !vector_refs.is_empty() {
        enrichment.push(("vectorData".to_string(), vector_refs));
    }

    // ── Application passthrough. ────────────────────────────────────────────
    // Every non-pan root fact keeps its own namespace and lands in BOTH the
    // graph and the new packet (Rob, 08-25). `hasRegionId` is the one
    // exception: its referents were the source's own region descriptions,
    // which are now pan Regions — carrying it would leave pointers to
    // subjects this image no longer contains.
    let mut app_fields: BTreeMap<String, Vec<(String, xmp::FieldValue)>> = BTreeMap::new();
    let mut ns_of_prefix: BTreeMap<String, String> = BTreeMap::new();
    for (pred, values) in &root {
        if pred.starts_with(PAN_NS) || pred == &format!("{COPIA_NS}hasRegionId") {
            continue;
        }
        let Some((ns, local)) = split_ns(pred) else { continue };
        let prefix = prefix_for(&ns);
        *stats.passthrough_fields.entry(format!("{prefix}:{local}")).or_default() += 1;

        let p = NamedNode::new(pred.as_str()).map_err(|e| anyhow!("bad source predicate {pred}: {e}"))?;
        for v in values {
            quads.push(Quad::new(
                subject.clone(),
                p.clone(),
                Literal::new_simple_literal(v),
                GraphName::DefaultGraph,
            ));
        }
        ns_of_prefix.insert(prefix.clone(), ns);
        let fv = if values.len() == 1 {
            xmp::FieldValue::Scalar(values[0].clone())
        } else {
            xmp::FieldValue::Bag(values.clone())
        };
        app_fields.entry(prefix).or_default().push((local, fv));
    }
    let app_blocks: Vec<xmp::AppBlock> = app_fields
        .into_iter()
        .map(|(prefix, mut fields)| {
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            xmp::AppBlock {
                ns_iri: ns_of_prefix.get(&prefix).cloned().unwrap_or_default(),
                prefix,
                fields,
            }
        })
        .collect();

    if opts.dry_run {
        return Ok(true);
    }

    // ── Land it: the new packet into Pan's own copy of the bytes. ───────────
    let out_packet = xmp::build_packet(&xmp::ImagePacket {
        pan_id: pan_id.clone(),
        blob_path: blob_rel.clone(),
        created_at: created_at.clone(),
        media_type,
        width,
        height,
        caption: current_caption,
        enrichment: enrichment.clone(),
        app_blocks,
        sub_subjects: Vec::new(),
    });
    let stamped = xmp::write_packet_into_png_bytes(&bytes, &out_packet)
        .with_context(|| format!("write packet into {}", img.stem))?;
    let abs = pan.layout.abs(&blob_rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).context("create blob shard dir")?;
    }
    std::fs::write(&abs, &stamped).with_context(|| format!("write {}", abs.display()))?;

    // References into the graph LAST: they describe files that now exist.
    for (ref_local, refs) in &enrichment {
        for r in refs {
            quads.extend(ref_quads(subject.as_str(), ref_local, r)?);
        }
    }
    pan.insert_quads(&quads)?;
    Ok(true)
}

fn write_data_file(pan: &Pan, rel: &str, image_iri: &str, link_local: &str, records: &[EnrichmentRecord]) -> Result<()> {
    let abs = pan.layout.abs(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).context("create data-file dir")?;
    }
    std::fs::write(&abs, build_data_file(image_iri, link_local, records))
        .with_context(|| format!("write {}", abs.display()))
}

/// Resolve a path the SOURCE recorded, which across eras is absolute, or
/// relative to the store root, or relative to the store root's parent.
/// Returns None when no candidate exists on disk — the caller reports it
/// rather than quietly producing a record that points nowhere.
fn resolve_source_path(source_root: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let candidates = [
        PathBuf::from(raw),
        source_root.join(raw),
        source_root.parent().unwrap_or(source_root).join(raw),
    ];
    candidates.into_iter().find(|c| c.is_file())
}

fn copy_into_store(pan: &Pan, src: &Path, rel: &str) -> Result<()> {
    let abs = pan.layout.abs(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).context("create artifact dir")?;
    }
    std::fs::copy(src, &abs).with_context(|| format!("copy {} → {}", src.display(), abs.display()))?;
    Ok(())
}

fn pan_lit(subject: &NamedNode, local: &str, value: &str) -> Result<Quad> {
    Ok(Quad::new(
        subject.clone(),
        NamedNode::new(format!("{PAN_NS}{local}")).map_err(|e| anyhow!("bad predicate {local}: {e}"))?,
        Literal::new_simple_literal(value),
        GraphName::DefaultGraph,
    ))
}

/// Split a predicate IRI into `(namespace, local)` at the last `/` or `#`.
fn split_ns(iri: &str) -> Option<(String, String)> {
    let cut = iri.rfind(['/', '#'])?;
    let (ns, local) = iri.split_at(cut + 1);
    if local.is_empty() {
        return None;
    }
    Some((ns.to_string(), local.to_string()))
}

/// A short prefix for a source namespace. Known stack namespaces get their
/// house name; anything else gets a readable token derived from the IRI, so an
/// unexpected namespace is visible in the report rather than anonymous.
fn prefix_for(ns: &str) -> String {
    match ns {
        COPIA_NS => "copia".into(),
        "http://purl.org/dc/elements/1.1/" => "dc".into(),
        "http://ns.adobe.com/photoshop/1.0/" => "photoshop".into(),
        "http://ns.adobe.com/xap/1.0/" => "xmp".into(),
        "http://ns.adobe.com/xap/1.0/rights/" => "xmpRights".into(),
        "http://cipa.jp/exif/1.0/" => "exifEX".into(),
        other => other
            .trim_end_matches(['/', '#'])
            .rsplit(['/', '#'])
            .next()
            .unwrap_or("ns")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_gives_the_sources_own_time() {
        assert_eq!(
            created_at_from_stem("20260817-202652-43cf3ec9").as_deref(),
            Some("2026-08-17T20:26:52+00:00")
        );
        assert_eq!(created_at_from_stem("not-a-stem"), None);
    }

    #[test]
    fn sub_second_timestamps_normalize_but_keep_their_offset() {
        assert_eq!(
            normalize_timestamp("2026-08-17T20:26:50.100416+00:00").as_deref(),
            Some("2026-08-17T20:26:50+00:00")
        );
        assert_eq!(
            normalize_timestamp("2026-08-17T20:26:50+00:00").as_deref(),
            Some("2026-08-17T20:26:50+00:00")
        );
    }

    #[test]
    fn namespaces_get_readable_prefixes() {
        assert_eq!(prefix_for(COPIA_NS), "copia");
        assert_eq!(prefix_for("http://purl.org/dc/elements/1.1/"), "dc");
        // An unknown namespace still reports under a legible name.
        assert_eq!(prefix_for("https://example.com/mflux/"), "mflux");
    }

    #[test]
    fn every_caption_field_names_its_model() {
        // The migration's headline: a model change must never again mean a
        // vocabulary change, so each legacy field maps to a model NAME.
        for (field, model) in CAPTION_FIELDS {
            assert!(field.ends_with("Caption"));
            assert!(!model.is_empty());
        }
    }
}
