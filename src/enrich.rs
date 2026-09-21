//! Enrichment records — the two-layer model (Rob, 2026-08-25).
//!
//! Bulk model output does not ride inside the image. Each enricher writes a
//! DATA FILE beside the blob; the image's XMP carries one [`EnrichmentRef`]
//! per file — model, path, item count. So:
//!
//! - the image stays light and readable in any viewer, and
//! - the image still knows, from its own bytes, exactly what exists for it and
//!   where. Nothing is left to filesystem convention (the failure that made
//!   Pool's masks silently unfindable: a missing file and a never-run detector
//!   looked identical).
//!
//! A data file is N-Quads, `.nq` — the same quads the graph holds, written
//! standalone, each naming Pan's graph in its fourth column. It opens with the REFERENCE node (the same `<pan/Enrichment/id>`
//! the image's XMP names) linking each record with `pan:item`, and every
//! record inside is a first-class node with its own assigned id and its own
//! `https://repolex.ai/pan/<Class>/<id>` IRI. The image links only to the
//! reference; records are reached through it (goodlux, 2026-09-16, option B
//! of the record-link question). Loading a file into the graph is therefore
//! just parsing it; there is no translation layer anywhere.

use anyhow::{anyhow, Context, Result};
use oxigraph::io::RdfFormat;
use oxigraph::model::{Literal, NamedNode, Quad, Term};
use std::path::Path;

use crate::config::{now_local, PAN_MEDIA_NS, PAN_NS};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// One enrichment record: a Region, Pose, Caption or Embedding.
///
/// `class` is the CONCRETE ontology class (`Region`) — it names both the
/// `rdf:type` and the `<Class>` segment of the record's IRI. `fields` are
/// `pan:` local names in a stable order; the writer never invents a field, so
/// what a producer supplies is exactly what lands.
#[derive(Debug, Clone)]
pub struct EnrichmentRecord {
    pub id: String,
    pub class: String,
    pub model: String,
    /// When pand wrote it — RFC3339, system local time (`pan:producedDate`).
    pub produced_date: String,
    pub fields: Vec<(String, String)>,
}

impl EnrichmentRecord {
    pub fn new(id: impl Into<String>, class: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            class: class.into(),
            model: model.into(),
            produced_date: now_local(),
            fields: Vec::new(),
        }
    }

    /// Add a field, skipping empties — an absent value must not become an
    /// empty-string fact (that is the "found nothing" / "never looked"
    /// collision in miniature).
    pub fn field(mut self, local: &str, value: impl AsRef<str>) -> Self {
        let v = value.as_ref();
        if !v.is_empty() {
            self.fields.push((local.to_string(), v.to_string()));
        }
        self
    }

    /// This record's full angle-bracket IRI.
    pub fn iri(&self) -> String {
        format!("{PAN_MEDIA_NS}{}/{}", self.class, self.id)
    }
}

/// A reference, written into the image's packet, to one enricher's data file.
#[derive(Debug, Clone)]
pub struct EnrichmentRef {
    pub id: String,
    pub model: String,
    /// Path relative to the store's media root.
    pub path: String,
    /// `pan:count` — how many regions the file holds. Only a regionData
    /// reference (typed pan:RegionData) carries it (goodlux, 2026-09-16);
    /// caption, vector and pose references carry none.
    pub count: Option<usize>,
    /// `pan:producedDate` — RFC3339, system local time.
    pub produced_date: String,
    /// `pan:modelReplyPath` — the model server's own answer, saved whole beside
    /// the record, relative to the store's media root (goodlux, 2026-09-19).
    /// Absent when the stage saves no such file.
    pub model_reply_path: Option<String>,
    /// What Pan asked the segmentation node for. Only a regionData reference
    /// has one: the other stages send the image and nothing to record.
    pub request: Option<SegmentRequest>,
}

/// The segmentation call Pan made, kept with its answer (goodlux,
/// 2026-09-19). A noun that found nothing leaves no region, so without this an
/// image with no person region could not say whether Pan asked for one.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentRequest {
    /// Comma-separated, exactly as sent.
    pub nouns: String,
    pub min_confidence: f32,
    pub polygon_verts: u32,
}

impl EnrichmentRef {
    pub fn new(model: &str, path: &str, count: Option<usize>) -> Self {
        Self {
            id: crate::gen_pan_id(),
            model: model.to_string(),
            path: path.to_string(),
            count,
            produced_date: now_local(),
            model_reply_path: None,
            request: None,
        }
    }

    /// Record the segmentation call this reference's regions came from.
    pub fn with_request(mut self, r: SegmentRequest) -> Self {
        self.request = Some(r);
        self
    }

    /// Name the server's own answer file that sits beside the record.
    pub fn with_model_reply(mut self, rel: impl Into<String>) -> Self {
        let rel = rel.into();
        if !rel.trim().is_empty() {
            self.model_reply_path = Some(rel);
        }
        self
    }

    /// This reference's full IRI, `<pan/Enrichment/id>` — the subject the data
    /// file opens with and the node the image's XMP names.
    pub fn iri(&self) -> String {
        format!("{PAN_MEDIA_NS}Enrichment/{}", self.id)
    }
}

/// Author a standalone data file: N-Quads, one statement per line, every
/// line naming Pan's graph in its fourth column (goodlux, 2026-09-18). The
/// statements are the reference node linked to each record with `pan:item`,
/// then each record in full — exactly the quads [`record_quads`] hands the
/// store, serialized, so a file and the graph cannot disagree.
///
/// `ref_iri` is the reference's IRI (`<pan/Enrichment/id>`), the same node the
/// image's XMP names: image → reference → item.
pub fn build_data_file(ref_iri: &str, records: &[EnrichmentRecord]) -> Result<String> {
    quads_to_nquads(&record_quads(ref_iri, records)?)
}

/// Serialize quads as N-Quads text.
pub fn quads_to_nquads(quads: &[Quad]) -> Result<String> {
    let mut w = oxigraph::io::RdfSerializer::from_format(RdfFormat::NQuads).for_writer(Vec::new());
    for q in quads {
        w.serialize_quad(q.as_ref()).context("serialize quad")?;
    }
    let bytes = w.finish().context("finish N-Quads")?;
    String::from_utf8(bytes).context("N-Quads is UTF-8")
}

/// The quads a data file's content contributes to the graph — produced from
/// the SAME records the file is written from, so store and file cannot drift.
pub fn record_quads(ref_iri: &str, records: &[EnrichmentRecord]) -> Result<Vec<Quad>> {
    let reference =
        NamedNode::new(ref_iri).map_err(|e| anyhow!("bad reference IRI {ref_iri}: {e}"))?;
    let link = NamedNode::new(format!("{PAN_NS}item")).expect("pan:item");
    let mut quads = Vec::with_capacity(records.len() * 6);
    for r in records {
        let subj = NamedNode::new(r.iri()).map_err(|e| anyhow!("bad record IRI: {e}"))?;
        quads.push(Quad::new(
            reference.clone(),
            link.clone(),
            subj.clone(),
            crate::config::pan_graph(),
        ));
    }
    quads.extend(record_facts(records)?);
    Ok(quads)
}

/// What a record says about itself: its class, its identity, when it was
/// written and its fields — with nothing linking it to anything. A record that
/// hangs off an enrichment reference is reached through pan:item; the render
/// request an image arrived with hangs off the image itself, and neither is a
/// fact about the record.
pub fn record_facts(records: &[EnrichmentRecord]) -> Result<Vec<Quad>> {
    let rdf_type = NamedNode::new(RDF_TYPE).expect("rdf:type");
    let mut quads = Vec::with_capacity(records.len() * 5);
    for r in records {
        let subj = NamedNode::new(r.iri()).map_err(|e| anyhow!("bad record IRI: {e}"))?;
        quads.push(Quad::new(
            subj.clone(),
            rdf_type.clone(),
            NamedNode::new(format!("{PAN_NS}{}", r.class))
                .map_err(|e| anyhow!("bad class IRI: {e}"))?,
            crate::config::pan_graph(),
        ));
        quads.push(self_id_quad(&subj)?);
        if !r.model.is_empty() {
            quads.push(pan_quad(&subj, "model", &r.model)?);
        }
        quads.push(pan_quad(&subj, "producedDate", &r.produced_date)?);
        for (local, value) in &r.fields {
            quads.push(pan_quad(&subj, local, value)?);
        }
    }
    Ok(quads)
}

/// The quads an [`EnrichmentRef`] contributes: the image's own index of what
/// exists for it and where.
pub fn ref_quads(image_iri: &str, ref_local: &str, r: &EnrichmentRef) -> Result<Vec<Quad>> {
    let image = NamedNode::new(image_iri).map_err(|e| anyhow!("bad image IRI {image_iri}: {e}"))?;
    let node = NamedNode::new(r.iri()).map_err(|e| anyhow!("bad enrichment IRI: {e}"))?;
    let rdf_type = NamedNode::new(RDF_TYPE).expect("rdf:type");
    let mut quads = vec![
        Quad::new(
            image,
            NamedNode::new(format!("{PAN_NS}{ref_local}"))
                .map_err(|e| anyhow!("bad ref predicate: {e}"))?,
            node.clone(),
            crate::config::pan_graph(),
        ),
        Quad::new(
            node.clone(),
            rdf_type,
            // The segmentation reference is its own class, the only one
            // that counts (pan.ttl 0.3.6); every other reference is a plain
            // Enrichment.
            NamedNode::new(format!(
                "{PAN_NS}{}",
                if ref_local == "regionData" {
                    "RegionData"
                } else {
                    "Enrichment"
                }
            ))
            .expect("reference class IRI"),
            crate::config::pan_graph(),
        ),
        self_id_quad(&node)?,
        pan_quad(&node, "model", &r.model)?,
        pan_quad(&node, "path", &r.path)?,
    ];
    if ref_local == "regionData" {
        if let Some(count) = r.count {
            quads.push(pan_quad(&node, "count", &count.to_string())?);
        }
    }
    if let Some(reply) = &r.model_reply_path {
        quads.push(pan_quad(&node, "modelReplyPath", reply)?);
    }
    if let Some(req) = &r.request {
        quads.push(pan_quad(&node, "requestNouns", &req.nouns)?);
        quads.push(pan_quad(
            &node,
            "requestMinConfidence",
            &req.min_confidence.to_string(),
        )?);
        quads.push(pan_quad(
            &node,
            "requestPolygonVerts",
            &req.polygon_verts.to_string(),
        )?);
    }
    quads.push(pan_quad(&node, "producedDate", &r.produced_date)?);
    Ok(quads)
}

/// `<node> pan:id <node>` — the identity, an IRI pointing at the Thing
/// itself. Spelled pan: in the graph exactly as in the file (goodlux,
/// 2026-09-17: every fact Pan writes is pan:); pan:id is the universal id by
/// owl:equivalentProperty in pan.ttl, so a git-lex or subtexture query still
/// finds it. Every Pan node carries one.
pub fn self_id_quad(node: &NamedNode) -> Result<Quad> {
    Ok(Quad::new(
        node.clone(),
        NamedNode::new(format!("{PAN_NS}id")).map_err(|e| anyhow!("pan:id IRI: {e}"))?,
        node.clone(),
        crate::config::pan_graph(),
    ))
}

fn pan_quad(subject: &NamedNode, local: &str, value: &str) -> Result<Quad> {
    Ok(Quad::new(
        subject.clone(),
        NamedNode::new(format!("{PAN_NS}{local}"))
            .map_err(|e| anyhow!("bad predicate {local}: {e}"))?,
        Literal::new_simple_literal(value),
        crate::config::pan_graph(),
    ))
}

/// Read a data file back into triples — the proof that a file IS the graph
/// content, not a private format needing a translator.
pub fn read_data_file(path: &Path) -> Result<Vec<(String, String, Term)>> {
    let mut out = Vec::new();
    for q in read_nquads_file(path)? {
        out.push((
            q.subject
                .to_string()
                .trim_matches(|c| c == '<' || c == '>')
                .to_string(),
            q.predicate.as_str().to_string(),
            q.object,
        ));
    }
    Ok(out)
}

/// Parse an N-Quads file into quads, graph names and all.
pub fn read_nquads_file(path: &Path) -> Result<Vec<Quad>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    oxigraph::io::RdfParser::from_format(RdfFormat::NQuads)
        .for_reader(raw.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<EnrichmentRecord> {
        vec![EnrichmentRecord::new("x7q2mf", "Region", "sam3")
            .field("descriptor", "person")
            .field("polygon", "1,2;3,4")
            .field("bbox", "1,2,3,4")
            .field("score", "0.96")
            .field("maskPath", "")]
    }

    #[test]
    fn data_file_round_trips_through_a_real_rdf_parser() {
        let reference = "https://repolex.ai/pan/Enrichment/r7k2p9x4";
        let nq = build_data_file(reference, &sample()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("regions.nq");
        std::fs::write(&p, &nq).unwrap();
        assert!(
            nq.lines()
                .all(|l| l.ends_with(&format!("<{}> .", crate::config::PAN_GRAPH_IRI))),
            "every line names Pan's graph in its fourth column:\n{nq}"
        );

        let triples = read_data_file(&p).unwrap();
        let region_iri = "https://repolex.ai/pan/Region/x7q2mf";

        assert!(
            triples.iter().any(|(s, p, o)| s == reference
                && p == &format!("{PAN_NS}item")
                && matches!(o, Term::NamedNode(n) if n.as_str() == region_iri)),
            "the reference links to the region by IRI with pan:item"
        );
        assert!(
            triples.iter().any(|(s, p, o)| s == region_iri
                && p == &format!("{PAN_NS}descriptor")
                && o.to_string().contains("person")),
            "region carries its descriptor"
        );
    }

    #[test]
    fn empty_fields_never_become_empty_facts() {
        // maskPath was supplied empty: it must be ABSENT, not "".
        let nq = build_data_file("https://repolex.ai/pan/Enrichment/a", &sample()).unwrap();
        assert!(!nq.contains("maskPath"), "empty field is omitted entirely");
    }

    #[test]
    fn file_and_graph_agree() {
        // The same records produce the same statements on both paths.
        let reference = "https://repolex.ai/pan/Enrichment/r7k2p9x4";
        let recs = sample();
        let quads = record_quads(reference, &recs).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("r.nq");
        std::fs::write(&p, build_data_file(reference, &recs).unwrap()).unwrap();
        let from_file = read_data_file(&p).unwrap();
        assert_eq!(
            quads.len(),
            from_file.len(),
            "data file and graph quads carry the same statement count"
        );
    }

    #[test]
    fn reference_quads_name_model_path_and_count() {
        let r = EnrichmentRef::new("sam3", "sam3/2026/08/17/k7m2p9x4.xml", Some(15));
        let quads = ref_quads("https://repolex.ai/pan/Image/k7m2p9x4", "regionData", &r).unwrap();
        let has = |local: &str, val: &str| {
            quads.iter().any(|q| {
                q.predicate.as_str() == format!("{PAN_NS}{local}")
                    && q.object.to_string().contains(val)
            })
        };
        assert!(has("path", "sam3/2026/08/17/k7m2p9x4.xml"));
        assert!(has("count", "15"));
        assert!(has("model", "sam3"));
        assert!(
            quads
                .iter()
                .any(|q| q.predicate.as_str() == RDF_TYPE
                    && q.object.to_string().contains("RegionData")),
            "a regionData reference is typed pan:RegionData: {quads:?}"
        );
    }

    /// A pose file is counted by reading it; its reference is a plain
    /// Enrichment with no count (goodlux, 2026-09-16).
    #[test]
    fn pose_reference_is_plain_and_carries_no_count() {
        let r = EnrichmentRef::new("rtmw-x-l", "image/pose/2026/09/16/abc.xml", Some(2));
        let quads = ref_quads("https://repolex.ai/pan/Image/abcdefgh", "poseData", &r).unwrap();
        assert!(
            !quads
                .iter()
                .any(|q| q.predicate.as_str() == format!("{PAN_NS}count")),
            "{quads:?}"
        );
        assert!(
            quads.iter().any(|q| q.predicate.as_str() == RDF_TYPE
                && q.object.to_string().ends_with("Enrichment>")),
            "{quads:?}"
        );
    }

    /// A caption or vector reference names one file holding one answer;
    /// pan:count is written only for region and pose references
    /// (goodlux, 2026-09-16), so it must be absent here.
    #[test]
    fn caption_reference_carries_no_count() {
        let r = EnrichmentRef::new("qwen/qwen3.8-27b", "image/caption/2026/09/16/abc.xml", None);
        let quads = ref_quads("https://repolex.ai/pan/Image/abcdefgh", "captionData", &r).unwrap();
        assert!(
            !quads
                .iter()
                .any(|q| q.predicate.as_str() == format!("{PAN_NS}count")),
            "caption reference must not carry pan:count: {quads:?}"
        );
        assert!(quads
            .iter()
            .any(|q| q.predicate.as_str() == format!("{PAN_NS}path")));
    }
}
