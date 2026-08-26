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
//! A data file is plain RDF/XML — the same triples the graph holds, written
//! standalone. It names the image by its full IRI (no XMP base-IRI games), and
//! every record inside is a first-class node with its own assigned id and its
//! own `https://repolex.ai/pan/<Class>/<id>` IRI. Loading one into the graph
//! is therefore just parsing it; there is no translation layer anywhere.

use anyhow::{anyhow, Context, Result};
use oxigraph::io::RdfFormat;
use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};
use std::path::Path;

use crate::config::{PAN_MEDIA_NS, PAN_NS};

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
    pub fields: Vec<(String, String)>,
}

impl EnrichmentRecord {
    pub fn new(id: impl Into<String>, class: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            class: class.into(),
            model: model.into(),
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
    /// Path relative to the store's storage root.
    pub path: String,
    pub count: usize,
}

/// Escape text for XML character data / attribute values.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Author a standalone data file: the image linked to each record, then each
/// record described in full.
///
/// `link_local` is the membership predicate's local name (`region`, `pose`,
/// `captionItem`) — the same predicate the graph uses, so a file and the store
/// never disagree about how a record hangs off its image.
pub fn build_data_file(image_iri: &str, link_local: &str, records: &[EnrichmentRecord]) -> String {
    let mut out = String::with_capacity(512 + records.len() * 256);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"\n");
    out.push_str(&format!("         xmlns:pan=\"{PAN_NS}\">\n"));

    // The image, and what it has.
    out.push_str(&format!("  <rdf:Description rdf:about=\"{}\">\n", esc(image_iri)));
    for r in records {
        out.push_str(&format!(
            "    <pan:{link_local} rdf:resource=\"{}\"/>\n",
            esc(&r.iri())
        ));
    }
    out.push_str("  </rdf:Description>\n");

    // Each record, in full.
    for r in records {
        out.push_str(&format!("  <rdf:Description rdf:about=\"{}\">\n", esc(&r.iri())));
        out.push_str(&format!(
            "    <rdf:type rdf:resource=\"{PAN_NS}{}\"/>\n",
            esc(&r.class)
        ));
        out.push_str(&format!("    <pan:id>{}</pan:id>\n", esc(&r.id)));
        if !r.model.is_empty() {
            out.push_str(&format!("    <pan:model>{}</pan:model>\n", esc(&r.model)));
        }
        for (local, value) in &r.fields {
            out.push_str(&format!("    <pan:{local}>{}</pan:{local}>\n", esc(value)));
        }
        out.push_str("  </rdf:Description>\n");
    }

    out.push_str("</rdf:RDF>\n");
    out
}

/// The quads a data file's content contributes to the graph — produced from
/// the SAME records the file is written from, so store and file cannot drift.
pub fn record_quads(image_iri: &str, link_local: &str, records: &[EnrichmentRecord]) -> Result<Vec<Quad>> {
    let image = NamedNode::new(image_iri).map_err(|e| anyhow!("bad image IRI {image_iri}: {e}"))?;
    let link = NamedNode::new(format!("{PAN_NS}{link_local}"))
        .map_err(|e| anyhow!("bad link predicate {link_local}: {e}"))?;
    let rdf_type = NamedNode::new(RDF_TYPE).expect("rdf:type");
    let mut quads = Vec::with_capacity(records.len() * 6);

    for r in records {
        let subj = NamedNode::new(r.iri()).map_err(|e| anyhow!("bad record IRI: {e}"))?;
        quads.push(Quad::new(
            image.clone(),
            link.clone(),
            subj.clone(),
            GraphName::DefaultGraph,
        ));
        quads.push(Quad::new(
            subj.clone(),
            rdf_type.clone(),
            NamedNode::new(format!("{PAN_NS}{}", r.class)).map_err(|e| anyhow!("bad class IRI: {e}"))?,
            GraphName::DefaultGraph,
        ));
        quads.push(pan_quad(&subj, "id", &r.id)?);
        if !r.model.is_empty() {
            quads.push(pan_quad(&subj, "model", &r.model)?);
        }
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
    let node = NamedNode::new(format!("{PAN_MEDIA_NS}Enrichment/{}", r.id))
        .map_err(|e| anyhow!("bad enrichment IRI: {e}"))?;
    let rdf_type = NamedNode::new(RDF_TYPE).expect("rdf:type");
    Ok(vec![
        Quad::new(
            image,
            NamedNode::new(format!("{PAN_NS}{ref_local}")).map_err(|e| anyhow!("bad ref predicate: {e}"))?,
            node.clone(),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            node.clone(),
            rdf_type,
            NamedNode::new(format!("{PAN_NS}Enrichment")).expect("Enrichment IRI"),
            GraphName::DefaultGraph,
        ),
        pan_quad(&node, "id", &r.id)?,
        pan_quad(&node, "model", &r.model)?,
        pan_quad(&node, "path", &r.path)?,
        pan_quad(&node, "count", &r.count.to_string())?,
    ])
}

fn pan_quad(subject: &NamedNode, local: &str, value: &str) -> Result<Quad> {
    Ok(Quad::new(
        subject.clone(),
        NamedNode::new(format!("{PAN_NS}{local}")).map_err(|e| anyhow!("bad predicate {local}: {e}"))?,
        Literal::new_simple_literal(value),
        GraphName::DefaultGraph,
    ))
}

/// Read a data file back into triples — the proof that a file IS the graph
/// content, not a private format needing a translator.
pub fn read_data_file(path: &Path) -> Result<Vec<(String, String, Term)>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let store = oxigraph::store::Store::new().context("scratch store")?;
    store
        .load_from_reader(RdfFormat::RdfXml, raw.as_bytes())
        .with_context(|| format!("parse {}", path.display()))?;
    let mut out = Vec::new();
    for q in store.iter() {
        let q = q.context("read parsed data file")?;
        out.push((
            q.subject.to_string().trim_matches(|c| c == '<' || c == '>').to_string(),
            q.predicate.as_str().to_string(),
            q.object,
        ));
    }
    Ok(out)
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
        let img = "https://repolex.ai/pan/Image/k7m2p9x4";
        let xml = build_data_file(img, "region", &sample());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("regions.xml");
        std::fs::write(&p, &xml).unwrap();

        let triples = read_data_file(&p).unwrap();
        let region_iri = "https://repolex.ai/pan/Region/x7q2mf";

        assert!(
            triples.iter().any(|(s, p, o)| s == img
                && p == &format!("{PAN_NS}region")
                && matches!(o, Term::NamedNode(n) if n.as_str() == region_iri)),
            "image links to the region by IRI"
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
        let xml = build_data_file("https://repolex.ai/pan/Image/a", "region", &sample());
        assert!(!xml.contains("maskPath"), "empty field is omitted entirely");
    }

    #[test]
    fn file_and_graph_agree() {
        // The same records produce the same statements on both paths.
        let img = "https://repolex.ai/pan/Image/k7m2p9x4";
        let recs = sample();
        let quads = record_quads(img, "region", &recs).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("r.xml");
        std::fs::write(&p, build_data_file(img, "region", &recs)).unwrap();
        let from_file = read_data_file(&p).unwrap();
        assert_eq!(
            quads.len(),
            from_file.len(),
            "data file and graph quads carry the same statement count"
        );
    }

    #[test]
    fn reference_quads_name_model_path_and_count() {
        let r = EnrichmentRef {
            id: "e1e1e1".into(),
            model: "sam3".into(),
            path: "sam3/2026/08/17/k7m2p9x4.xml".into(),
            count: 15,
        };
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
    }
}
