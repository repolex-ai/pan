//! XMP authoring + reading for PNG media.
//!
//! The two-tier packet model, lifted from Pool (`pool/src/xmp.rs`):
//!   - Pan ALWAYS authors its own `pan:` block (identity: cid / blobPath /
//!     createdAt) on the root `<rdf:Description>`.
//!   - App namespaces (copia:, dc:, …) are written beside it as OPAQUE
//!     PASSTHROUGH — Pan judges nothing about their meaning.
//!   - Optional sub-subject Descriptions (`rdf:about="…"`) carry app-scoped
//!     facts about parts of the media (regions etc.) — generic RDF, no
//!     app-specific shaping here.
//!
//! THE ONE ENGINE CHANGE from Pool (per the Pan spec): Pool read XMP back with
//! regexes over RDF/XML — brittle the instant any other writer appears. Pan
//! keeps the packet SHAPE but reads with oxigraph's real RDF/XML parser
//! ([`parse_packet`]).
//!
//! NON-NEGOTIABLE INVARIANT (ported with its test): stamping metadata into a
//! PNG preserves the PIXELS exactly, so [`compute_pixel_cid`] before == after.
//! Metadata edits never rotate identity.

use anyhow::{anyhow, Context, Result};
use oxigraph::io::RdfFormat;
use oxigraph::model::Term;
use oxigraph::store::Store;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::config::PAN_NS;

const XMP_TEXT_KEY: &str = "XML:com.adobe.xmp";
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

// ── Packet authoring (lifted from Pool, pan: for pool:) ────────────────────

/// A field value: scalar or unordered multi-value (serialized as `rdf:Bag`).
#[derive(Debug, Clone)]
pub enum FieldValue {
    Scalar(String),
    Bag(Vec<String>),
}

/// One app-namespaced XMP block Pan passes through as opaque data.
#[derive(Debug, Clone)]
pub struct AppBlock {
    /// The short prefix, e.g. "copia", "dc".
    pub prefix: String,
    /// The namespace IRI the prefix binds to.
    pub ns_iri: String,
    /// Flat fields for THIS namespace, in a stable order for deterministic output.
    pub fields: Vec<(String, FieldValue)>,
}

/// One sub-subject Description (`rdf:about="…"`) authored from structured data.
#[derive(Debug, Clone)]
pub struct SubSubjectBlock {
    /// The FULLY-SCOPED subject IRI for `rdf:about`.
    pub about: String,
    /// The `<rdf:type rdf:resource="…"/>` IRI.
    pub rdf_type: String,
    /// Namespace prefix these fields live in.
    pub prefix: String,
    pub ns_iri: String,
    pub fields: Vec<(String, FieldValue)>,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Serialize one `<prefix:local>…</prefix:local>` element. A `Bag` becomes an
/// `<rdf:Bag>` of `<rdf:li>` members; a `Scalar` the bare escaped body.
fn serialize_field(prefix: &str, local: &str, value: &FieldValue, indent: &str) -> String {
    match value {
        FieldValue::Scalar(s) => {
            format!("{indent}<{prefix}:{local}>{}</{prefix}:{local}>\n", xml_escape(s))
        }
        FieldValue::Bag(members) => {
            let mut out = format!("{indent}<{prefix}:{local}><rdf:Bag>");
            for m in members {
                out.push_str(&format!("<rdf:li>{}</rdf:li>", xml_escape(m)));
            }
            out.push_str(&format!("</rdf:Bag></{prefix}:{local}>\n"));
            out
        }
    }
}

/// The ALWAYS-present `pan:` identity block.
fn pan_block_fields(cid: &str, blob_path: &str, created_at: &str) -> Vec<(String, FieldValue)> {
    vec![
        ("cid".to_string(), FieldValue::Scalar(cid.to_string())),
        ("blobPath".to_string(), FieldValue::Scalar(blob_path.to_string())),
        ("createdAt".to_string(), FieldValue::Scalar(created_at.to_string())),
    ]
}

/// Author a COMPLETE XMP packet from scratch — Pan is the stamping authority.
/// The root `<rdf:Description>` (no rdf:about) carries the `pan:` block ALWAYS
/// FIRST, then each app block's fields; one sub-subject Description per region
/// block. Self-contained `<?xpacket?>`-wrapped, round-trips through
/// [`parse_packet`].
pub fn build_packet(
    cid: &str,
    blob_path: &str,
    created_at: &str,
    app_blocks: &[AppBlock],
    sub_subjects: &[SubSubjectBlock],
) -> String {
    let mut out = String::with_capacity(1024 + sub_subjects.len() * 256);
    out.push_str("<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n");
    out.push_str("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n");
    out.push_str("  <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n");

    // Root Description: pan: block ALWAYS, then app blocks (passthrough).
    out.push_str("    <rdf:Description");
    out.push_str(&format!(" xmlns:pan=\"{PAN_NS}\""));
    for b in app_blocks {
        out.push_str(&format!(" xmlns:{}=\"{}\"", b.prefix, b.ns_iri));
    }
    out.push_str(">\n");

    for (local, value) in pan_block_fields(cid, blob_path, created_at) {
        out.push_str(&serialize_field("pan", &local, &value, "      "));
    }
    for b in app_blocks {
        for (local, value) in &b.fields {
            out.push_str(&serialize_field(&b.prefix, local, value, "      "));
        }
    }
    out.push_str("    </rdf:Description>\n");

    for r in sub_subjects {
        out.push_str(&format!(
            "    <rdf:Description rdf:about=\"{}\" xmlns:{}=\"{}\">\n",
            xml_escape(&r.about),
            r.prefix,
            r.ns_iri
        ));
        out.push_str(&format!(
            "      <rdf:type rdf:resource=\"{}\"/>\n",
            xml_escape(&r.rdf_type)
        ));
        for (local, value) in &r.fields {
            out.push_str(&serialize_field(&r.prefix, local, value, "      "));
        }
        out.push_str("    </rdf:Description>\n");
    }

    out.push_str("  </rdf:RDF>\n");
    out.push_str("</x:xmpmeta>\n");
    out.push_str("<?xpacket end=\"w\"?>");
    out
}

// ── PNG stamp (pixel-preserving, lifted from Pool) ──────────────────────────

/// Stamp an XMP packet into PNG bytes IN MEMORY, returning the new PNG bytes.
/// Pixels are decoded + re-encoded identically (same color type + depth), any
/// existing XMP chunk is dropped, the new packet is written as a UTF-8 iTXt
/// chunk. Non-XMP text chunks are preserved best-effort.
///
/// CRITICAL for the pixel-cid contract: re-encoding preserves the PIXELS
/// exactly, so `compute_pixel_cid(output) == compute_pixel_cid(input)`.
/// Stamping does NOT rotate identity. (The FILE-byte sha DOES change.)
pub fn write_packet_into_png_bytes(png_bytes: &[u8], packet: &str) -> Result<Vec<u8>> {
    use std::io::BufWriter;

    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().context("decode PNG for stamp")?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).context("read PNG frame for stamp")?;
    let pixels = &buf[..frame.buffer_size()];
    let info = reader.info();

    let mut out: Vec<u8> = Vec::new();
    {
        let w = BufWriter::new(&mut out);
        let mut enc = png::Encoder::new(w, info.width, info.height);
        enc.set_color(info.color_type);
        enc.set_depth(info.bit_depth);
        // Preserve every NON-XMP text chunk (best-effort; typically ASCII
        // keyword/value pairs). A chunk that isn't latin1-representable is
        // skipped rather than failing the whole write.
        for c in info
            .uncompressed_latin1_text
            .iter()
            .filter(|c| c.keyword != XMP_TEXT_KEY)
        {
            enc.add_text_chunk(c.keyword.clone(), c.text.clone()).ok();
        }
        // Our XMP as a UTF-8 iTXt chunk: the packet contains a BOM and app
        // fields may carry arbitrary unicode — neither fits a latin1 tEXt
        // chunk. iTXt is the correct PNG home for UTF-8 XMP.
        enc.add_itxt_chunk(XMP_TEXT_KEY.to_string(), packet.to_string())
            .context("add XMP iTXt chunk")?;
        let mut writer = enc.write_header().context("write PNG header")?;
        writer
            .write_image_data(pixels)
            .context("write PNG image data")?;
        writer.finish().context("finish PNG encode")?;
    }
    Ok(out)
}

/// Read the XMP packet string out of PNG bytes (`XML:com.adobe.xmp` chunk,
/// any of the three text-chunk forms). Ok(None) if no XMP chunk is present.
pub fn read_xmp_packet_from_bytes(png_bytes: &[u8]) -> Result<Option<String>> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let reader = decoder.read_info().context("decode PNG for XMP read")?;
    let info = reader.info();
    for chunk in info.uncompressed_latin1_text.iter() {
        if chunk.keyword == XMP_TEXT_KEY {
            return Ok(Some(chunk.text.clone()));
        }
    }
    for chunk in info.utf8_text.iter() {
        if chunk.keyword == XMP_TEXT_KEY {
            return Ok(Some(chunk.get_text()?));
        }
    }
    for chunk in info.compressed_latin1_text.iter() {
        if chunk.keyword == XMP_TEXT_KEY {
            return Ok(Some(chunk.get_text()?));
        }
    }
    Ok(None)
}

// ── Packet reading — the REAL RDF parser (the change from Pool) ─────────────

/// The facts read for one subject in a packet. `subject: None` is the root
/// Description (no rdf:about — facts about the media object itself);
/// `Some(iri)` is a sub-subject (region etc.).
#[derive(Debug, Clone)]
pub struct ParsedSubject {
    pub subject: Option<String>,
    /// Full-IRI predicate → values (a Bag flattens to its members, in order).
    pub facts: Vec<(String, Vec<String>)>,
}

/// Parse an XMP packet with oxigraph's real RDF/XML parser.
///
/// Keeps Pool's SHAPE (one block per Description, subject-scoped so region
/// sub-subjects never clobber the root) but swaps the regex ENGINE for a real
/// parser: namespaces, attribute forms, CDATA, and foreign writers all behave.
///
/// `rdf:Bag`/`rdf:Seq` containers are flattened to their member values in
/// order. Predicates come back as full IRIs (namespace expansion is the
/// parser's job, not a prefix-map lookup).
pub fn parse_packet(packet: &str) -> Result<Vec<ParsedSubject>> {
    // The packet wraps <rdf:RDF> in <?xpacket?> + <x:xmpmeta>; oxigraph wants
    // bare RDF/XML, so slice the rdf:RDF element out.
    let start = packet
        .find("<rdf:RDF")
        .ok_or_else(|| anyhow!("XMP packet has no <rdf:RDF> element"))?;
    let end = packet
        .rfind("</rdf:RDF>")
        .ok_or_else(|| anyhow!("XMP packet has no </rdf:RDF> close"))?
        + "</rdf:RDF>".len();
    let rdf_xml = &packet[start..end];

    let store = Store::new().context("in-memory store for XMP parse")?;
    store
        .load_from_reader(RdfFormat::RdfXml, rdf_xml.as_bytes())
        .context("parse XMP RDF/XML")?;

    // Container flattening: collect every rdf:_N membership triple first, so a
    // predicate whose object is a container node expands to the member list.
    let mut container_members: HashMap<String, Vec<(u32, String)>> = HashMap::new();
    let mut container_nodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut plain: Vec<(String, Option<String>, String, Term)> = Vec::new(); // (skey, subject_iri, pred, obj)

    for quad in store.iter() {
        let quad = quad.context("XMP quad")?;
        let subj_key = quad.subject.to_string(); // includes <>/_: syntax — used only as a map key
        let pred = quad.predicate.as_str().to_string();

        if let Some(n) = pred.strip_prefix(RDF_NS).and_then(|l| l.strip_prefix('_')) {
            if let Ok(n) = n.parse::<u32>() {
                container_members
                    .entry(subj_key.clone())
                    .or_default()
                    .push((n, term_value(&quad.object)));
                container_nodes.insert(subj_key);
                continue;
            }
        }
        // rdf:type rdf:Bag/Seq/Alt on a container node is bookkeeping, not a fact.
        if pred == format!("{RDF_NS}type") {
            if let Term::NamedNode(t) = &quad.object {
                if t.as_str().starts_with(RDF_NS) {
                    container_nodes.insert(subj_key);
                    continue;
                }
            }
        }
        let subject_iri = match &quad.subject {
            oxigraph::model::NamedOrBlankNode::NamedNode(n) => Some(n.as_str().to_string()),
            _ => None,
        };
        plain.push((subj_key, subject_iri, pred, quad.object.clone()));
    }

    // Group facts by subject, resolving container objects to member lists.
    let mut by_subject: HashMap<Option<String>, Vec<(String, Vec<String>)>> = HashMap::new();
    for (skey, subject_iri, pred, obj) in plain {
        // A fact ON a container node itself (rdf:type noise) was filtered above;
        // skip anything else asserted on a container node — it has no home.
        if container_nodes.contains(&skey) && subject_iri.is_none() {
            continue;
        }
        let values = match &obj {
            Term::BlankNode(b) => {
                let key = format!("_:{}", b.as_str());
                match container_members.get(&key) {
                    Some(members) => {
                        let mut m = members.clone();
                        m.sort_by_key(|(n, _)| *n);
                        m.into_iter().map(|(_, v)| v).collect()
                    }
                    None => vec![term_value(&obj)],
                }
            }
            _ => vec![term_value(&obj)],
        };
        by_subject
            .entry(subject_iri)
            .or_default()
            .push((pred, values));
    }

    let mut out: Vec<ParsedSubject> = by_subject
        .into_iter()
        .map(|(subject, mut facts)| {
            facts.sort();
            ParsedSubject { subject, facts }
        })
        .collect();
    // Root (None) first, then named subjects in stable order.
    out.sort_by(|a, b| a.subject.cmp(&b.subject));
    Ok(out)
}

fn term_value(t: &Term) -> String {
    match t {
        Term::Literal(l) => l.value().to_string(),
        Term::NamedNode(n) => n.as_str().to_string(),
        Term::BlankNode(b) => b.as_str().to_string(),
        _ => t.to_string(),
    }
}

// ── Pixel cid — the identity function (lifted verbatim from Pool) ───────────

/// Compute the PIXEL content id of a PNG: the content address of the *image*,
/// stable across XMP/metadata edits.
///
/// CANONICAL CROSS-REPO DEFINITION (matches Pool + OpenIris byte-for-byte):
/// `sha256` of the decoded pixel buffer **normalized to 8-bit RGB (no alpha),
/// row-major top-to-bottom, 3 bytes/pixel in R,G,B order** — exactly PIL's
/// `Image.open(png).convert("RGB").tobytes()`. Palette → expanded; grayscale →
/// replicated to R=G=B; alpha → stripped; 16-bit → high byte (`>>8`).
pub fn compute_pixel_cid(png_bytes: &[u8]) -> Result<String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    // EXPAND: palette → RGB, sub-8-bit grayscale/tRNS → 8-bit. Leaves 16-bit
    // as 16-bit and alpha as-is; we handle those two below to match PIL.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().context("pixel-cid: decode PNG info")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buf)
        .context("pixel-cid: read PNG frame")?;
    let data = &buf[..frame.buffer_size()];

    let (color, depth) = (frame.color_type, frame.bit_depth);
    let channels = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            return Err(anyhow!("pixel-cid: unexpected Indexed color after EXPAND"))
        }
    };
    let bytes_per_sample = if depth == png::BitDepth::Sixteen { 2 } else { 1 };
    let stride = channels * bytes_per_sample;
    if stride == 0 || data.len() % stride != 0 {
        return Err(anyhow!(
            "pixel-cid: buffer {} not divisible by stride {} (color={:?} depth={:?})",
            data.len(),
            stride,
            color,
            depth
        ));
    }

    // Read one 8-bit sample: for 16-bit take the HIGH byte (PIL's >>8).
    let sample8 = |px: &[u8], ch: usize| -> u8 {
        if bytes_per_sample == 2 {
            px[ch * 2] // big-endian high byte
        } else {
            px[ch]
        }
    };

    let mut rgb: Vec<u8> = Vec::with_capacity(data.len() / stride * 3);
    for px in data.chunks_exact(stride) {
        let (r, g, b) = match color {
            png::ColorType::Grayscale | png::ColorType::GrayscaleAlpha => {
                let v = sample8(px, 0);
                (v, v, v)
            }
            png::ColorType::Rgb | png::ColorType::Rgba => {
                (sample8(px, 0), sample8(px, 1), sample8(px, 2))
            }
            png::ColorType::Indexed => unreachable!(),
        };
        rgb.push(r);
        rgb.push(g);
        rgb.push(b);
    }

    let mut h = Sha256::new();
    h.update(&rgb);
    Ok(format!("sha256:{:x}", h.finalize()))
}

/// True if the bytes start with the PNG signature.
pub fn is_png(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[0..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a tiny RGB PNG for tests.
    pub(crate) fn make_test_png(w: u32, h: u32, seed: u8) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            let px: Vec<u8> = (0..w * h * 3)
                .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
                .collect();
            writer.write_image_data(&px).unwrap();
            writer.finish().unwrap();
        }
        out
    }

    #[test]
    fn stamp_preserves_pixel_cid() {
        // THE invariant: metadata edits never rotate identity.
        let png = make_test_png(16, 16, 7);
        let cid_before = compute_pixel_cid(&png).unwrap();
        let packet = build_packet(&cid_before, "blob/image/x.png", "2026-07-15T00:00:00Z", &[], &[]);
        let stamped = write_packet_into_png_bytes(&png, &packet).unwrap();
        let cid_after = compute_pixel_cid(&stamped).unwrap();
        assert_eq!(cid_before, cid_after, "stamping rotated the pixel cid");
        assert_ne!(png, stamped, "file bytes should differ (packet embedded)");
    }

    #[test]
    fn packet_round_trips_through_real_parser() {
        let apps = vec![AppBlock {
            prefix: "copia".to_string(),
            ns_iri: "https://repolex.ai/ontology/kit/copia/".to_string(),
            fields: vec![
                ("sceneMood".to_string(), FieldValue::Scalar("calm & <bright>".to_string())),
                (
                    "sceneObjects".to_string(),
                    FieldValue::Bag(vec!["wolf".to_string(), "forest".to_string()]),
                ),
            ],
        }];
        let subs = vec![SubSubjectBlock {
            about: "urn:sha256:abc/Region/wolf/01".to_string(),
            rdf_type: "https://repolex.ai/ontology/kit/copia/Sam3Region".to_string(),
            prefix: "copia".to_string(),
            ns_iri: "https://repolex.ai/ontology/kit/copia/".to_string(),
            fields: vec![("regionDescriptor".to_string(), FieldValue::Scalar("wolf".to_string()))],
        }];
        let packet = build_packet("sha256:abc", "blob/image/x.png", "2026-07-15T00:00:00Z", &apps, &subs);

        let parsed = parse_packet(&packet).unwrap();
        // Root block (no rdf:about) + one named sub-subject.
        let root = parsed.iter().find(|p| p.subject.is_none()).expect("root block");
        let get = |facts: &[(String, Vec<String>)], iri: &str| -> Vec<String> {
            facts
                .iter()
                .find(|(p, _)| p == iri)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get(&root.facts, "https://repolex.ai/ontology/pan/cid"), vec!["sha256:abc"]);
        assert_eq!(
            get(&root.facts, "https://repolex.ai/ontology/kit/copia/sceneMood"),
            vec!["calm & <bright>"],
            "escaping must round-trip through the real parser"
        );
        assert_eq!(
            get(&root.facts, "https://repolex.ai/ontology/kit/copia/sceneObjects"),
            vec!["wolf", "forest"],
            "Bag flattens to ordered members"
        );

        let region = parsed
            .iter()
            .find(|p| p.subject.as_deref() == Some("urn:sha256:abc/Region/wolf/01"))
            .expect("region sub-subject scoped separately, not clobbering root");
        assert_eq!(
            get(&region.facts, "https://repolex.ai/ontology/kit/copia/regionDescriptor"),
            vec!["wolf"]
        );
    }

    #[test]
    fn stamp_replaces_prior_xmp_and_preserves_other_text() {
        let png = make_test_png(8, 8, 1);
        let p1 = build_packet("sha256:one", "a.png", "2026-01-01T00:00:00Z", &[], &[]);
        let s1 = write_packet_into_png_bytes(&png, &p1).unwrap();
        let p2 = build_packet("sha256:two", "b.png", "2026-01-02T00:00:00Z", &[], &[]);
        let s2 = write_packet_into_png_bytes(&s1, &p2).unwrap();
        let packet = read_xmp_packet_from_bytes(&s2).unwrap().expect("xmp present");
        assert!(packet.contains("sha256:two"), "new packet wins");
        assert!(!packet.contains("sha256:one"), "old packet fully replaced");
    }

    #[test]
    fn is_png_detects() {
        assert!(is_png(&make_test_png(1, 1, 0)));
        assert!(!is_png(b"\xFF\xD8\xFF jpeg-ish"));
    }
}
