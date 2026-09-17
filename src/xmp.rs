//! XMP authoring + reading for PNG media.
//!
//! The two-tier packet model, lifted from Pool (`pool/src/xmp.rs`):
//!   - Pan ALWAYS authors its own `pan:` block (identity: panId / blobPath /
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
//! PNG preserves the PIXELS exactly, so [`pixel_hash`] before == after.
//! (`pixel_hash` is a pixel-equality instrument for pinning this invariant —
//! it is NOT an identity; identity is the assigned panId.)

use anyhow::{anyhow, Context, Result};
use oxigraph::io::RdfFormat;
use oxigraph::model::Term;
use oxigraph::store::Store;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

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

/// Escape text for XML character data / attribute values.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One field inside a Description: a scalar, or an rdf:Bag of members.
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

/// The thumbnail reference as written in the image's XMP `pan:thumbnail`
/// struct: the Thumbnail node's id (every reference in the file carries its
/// own pan:id, goodlux 2026-09-16), its path relative to the media root, and
/// its pixel size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThumbRef {
    pub id: String,
    pub path: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ImagePacket {
    /// The media object's full IRI (`pan:id`).
    pub iri: String,
    pub media_path: String,
    pub created_date: String,
    pub media_type: String,
    /// The file this source was made from (pan:sourceFile): the original
    /// under img/original/ when converted, the source itself otherwise.
    pub source_file: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// One sentence (pan:shortDescription) and the detailed description
    /// (pan:longDescription) the caption stage wrote.
    pub short_description: Option<String>,
    pub long_description: Option<String>,
    /// Every physical thing the caption model named (pan:sceneObjects), one
    /// value each — an rdf:Bag in the file.
    pub scene_objects: Vec<String>,
    /// The scene fields, `(local name, value)`, e.g. `("sceneMood", "serene")`.
    pub scene: Vec<(String, String)>,
    /// Facts a person set with `pan set`, `(local name, value)`, e.g.
    /// `("rating", "4")` — the settable fields of pan.ttl.
    pub curation: Vec<(String, String)>,
    /// References Pan itself put on the image, in bracket form — photoset
    /// membership, `<pan/Photoset/id>`, one `pan:relatedToId` element each
    /// (pan.ttl 0.4.2). A producer's relatedToId is not here: it stays in
    /// the producer's own Description.
    pub related_to: Vec<String>,
    /// Set once every configured stage has a record.
    pub ready_date: Option<String>,
    /// The thumbnail Pan made, with the Thumbnail node's own id.
    pub thumbnail: Option<ThumbRef>,
    /// `(reference predicate local name, references)`, e.g.
    /// `("regionData", [...])`. Empty groups are omitted.
    pub enrichment: Vec<(String, Vec<crate::enrich::EnrichmentRef>)>,
}

/// git-lex's angle-bracket form of one of Pan's IRIs: `<pan/Image/abc>` for
/// `https://repolex.ai/pan/Image/abc`. The kit namespace base is dropped and
/// the `pan/Class/id` tail is kept — exactly what a soul writes in
/// frontmatter, and what `iri_from_bracket` turns back into the IRI.
pub fn bracket_of_iri(iri: &str) -> String {
    match iri.strip_prefix(crate::config::PAN_MEDIA_NS) {
        Some(tail) => format!("<pan/{tail}>"),
        None => match iri.strip_prefix("https://repolex.ai/") {
            Some(tail) => format!("<{tail}>"),
            None => iri.to_string(),
        },
    }
}

/// Serialize one enrichment reference bag as a flat property of the media object.
fn serialize_enrichment(local: &str, refs: &[crate::enrich::EnrichmentRef], indent: &str) -> String {
    let mut out = format!("{indent}<pan:{local}>\n{indent} <rdf:Bag>\n");
    for r in refs {
        out.push_str(&format!("{indent}  <rdf:li rdf:parseType=\"Resource\">\n"));
        out.push_str(&format!("{indent}   <pan:id>&lt;pan/Enrichment/{}&gt;</pan:id>\n", xml_escape(&r.id)));
        if !r.model.is_empty() {
            out.push_str(&format!("{indent}   <pan:model>{}</pan:model>\n", xml_escape(&r.model)));
        }
        out.push_str(&format!("{indent}   <pan:path>{}</pan:path>\n", xml_escape(&r.path)));
        if let Some(count) = r.count {
            out.push_str(&format!("{indent}   <pan:count>{count}</pan:count>\n"));
        }
        out.push_str(&format!("{indent}   <pan:producedDate>{}</pan:producedDate>\n", xml_escape(&r.produced_date)));
        out.push_str(&format!("{indent}  </rdf:li>\n"));
    }
    out.push_str(&format!("{indent} </rdf:Bag>\n{indent}</pan:{local}>\n"));
    out
}

/// Author a COMPLETE XMP packet from scratch — Pan is the authority for its
/// own media's metadata. Self-contained `<?xpacket?>`-wrapped, and readable
/// back through [`parse_packet`].
pub fn build_packet(p: &ImagePacket) -> String {
    compose_packet(None, &build_pan_description(p))
}

/// Pan's own root Description: identity, size, caption, thumbnail and
/// enrichment references as FLAT properties of the media object — the same
/// shape the graph has and the same shape Horae's copia block uses, so a
/// viewer labels them "Media Path", not "Image Media Path" (Rob, 2026-09-05:
/// no wrapper struct). `rdf:about=""` is the standard Adobe form — it
/// resolves to the parser's base IRI, i.e. the media object. Nothing but pan:
/// vocabulary lives here — the file carries only the pan and copia
/// namespaces, never git-lex (goodlux, 2026-09-07). Other namespaces ride in
/// their own Descriptions, untouched.
///
/// Identity and creation time are `pan:id` and `pan:createdDate`, in the
/// file AND in the graph — one spelling, no translation at the boundary
/// (goodlux, 2026-09-17). pan:id is the universal id by owl:equivalentProperty
/// in pan.ttl, so a git-lex or subtexture query still finds it.
///
/// Every identity in the file is written in git-lex's angle-bracket form,
/// `<pan/Image/id>` — the same text a soul writes in frontmatter — never the
/// expanded IRI (goodlux, 2026-09-05).
pub fn build_pan_description(p: &ImagePacket) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("    <rdf:Description rdf:about=\"\"");
    out.push_str(&format!(" xmlns:pan=\"{PAN_NS}\">\n"));
    out.push_str(&format!("      <pan:id>{}</pan:id>\n", xml_escape(&bracket_of_iri(&p.iri))));
    out.push_str(&format!("      <pan:createdDate>{}</pan:createdDate>\n", xml_escape(&p.created_date)));
    let mut ident: Vec<(String, FieldValue)> = vec![
        ("mediaPath".into(), FieldValue::Scalar(p.media_path.clone())),
    ];
    if !p.media_type.is_empty() {
        ident.push(("mediaType".into(), FieldValue::Scalar(p.media_type.clone())));
    }
    if !p.source_file.is_empty() {
        ident.push(("sourceFile".into(), FieldValue::Scalar(p.source_file.clone())));
    }
    if let Some(w) = p.width {
        ident.push(("width".into(), FieldValue::Scalar(w.to_string())));
    }
    if let Some(h) = p.height {
        ident.push(("height".into(), FieldValue::Scalar(h.to_string())));
    }
    if let Some(c) = &p.short_description {
        ident.push(("shortDescription".into(), FieldValue::Scalar(c.clone())));
    }
    if let Some(c) = &p.long_description {
        ident.push(("longDescription".into(), FieldValue::Scalar(c.clone())));
    }
    if !p.scene_objects.is_empty() {
        ident.push(("sceneObjects".into(), FieldValue::Bag(p.scene_objects.clone())));
    }
    for (local, value) in &p.scene {
        ident.push((local.clone(), FieldValue::Scalar(value.clone())));
    }
    for (local, value) in &p.curation {
        ident.push((local.clone(), FieldValue::Scalar(value.clone())));
    }
    // One flat element per reference, never a Bag: the reader turns a
    // bracket literal on this predicate into the edge, and a Bag member
    // would not be on the predicate.
    for r in &p.related_to {
        ident.push(("relatedToId".into(), FieldValue::Scalar(r.clone())));
    }
    if let Some(r) = &p.ready_date {
        ident.push(("readyDate".into(), FieldValue::Scalar(r.clone())));
    }
    for (local, value) in &ident {
        out.push_str(&serialize_field("pan", local, value, "      "));
    }
    if let Some(t) = &p.thumbnail {
        out.push_str("      <pan:thumbnail rdf:parseType=\"Resource\">\n");
        out.push_str(&format!("       <pan:id>&lt;pan/Thumbnail/{}&gt;</pan:id>\n", xml_escape(&t.id)));
        out.push_str(&format!("       <pan:path>{}</pan:path>\n", xml_escape(&t.path)));
        out.push_str(&format!("       <pan:width>{}</pan:width>\n", t.width));
        out.push_str(&format!("       <pan:height>{}</pan:height>\n", t.height));
        out.push_str("      </pan:thumbnail>\n");
    }
    for (local, refs) in &p.enrichment {
        if refs.is_empty() {
            continue;
        }
        out.push_str(&serialize_enrichment(local, refs, "      "));
    }
    out.push_str("    </rdf:Description>\n");
    out
}

// ── PNG stamp (pixel-preserving, lifted from Pool) ──────────────────────────

/// Stamp an XMP packet into PNG bytes IN MEMORY, returning the new PNG bytes.
/// Pixels are decoded + re-encoded identically (same color type + depth), any
/// existing XMP chunk is dropped, the new packet is written as a UTF-8 iTXt
/// chunk. Non-XMP text chunks are preserved best-effort.
///
/// CRITICAL for the stamp invariant: re-encoding preserves the PIXELS
/// exactly, so `pixel_hash(output) == pixel_hash(input)` — a metadata edit
/// never touches the image. (The FILE bytes DO change.)
pub fn write_packet_into_png_bytes(png_bytes: &[u8], packet: &str) -> Result<Vec<u8>> {
    // Chunk surgery, not re-encoding: every chunk the producer wrote (IDAT,
    // sdapi `parameters`, EXIF, ICC, …) is copied byte-for-byte; only the XMP
    // chunk changes. Nothing is ever stripped (Rob, 2026-09-03).
    crate::pngchunk::replace_xmp(png_bytes, packet)
}

/// Read the XMP packet string out of PNG bytes (`XML:com.adobe.xmp` chunk,
/// any of the three text-chunk forms). Ok(None) if no XMP chunk is present.
pub fn read_xmp_packet_from_bytes(png_bytes: &[u8]) -> Result<Option<String>> {
    crate::pngchunk::read_xmp(png_bytes)
}

/// Split an XMP packet into its top-level `<rdf:Description …>…</rdf:Description>`
/// elements, verbatim. `pan_authored` says whether an element is Pan's own
/// root block (it declares the pan namespace and carries `pan:mediaPath`), which
/// Pan re-authors on every write; every OTHER Description is someone else's
/// and is preserved exactly as found.
pub fn split_descriptions(packet: &str) -> Vec<(bool, String)> {
    let Some(start) = find_rdf_open(packet) else { return Vec::new() };
    let Some(end) = packet.rfind("</rdf:RDF>") else { return Vec::new() };
    let body = &packet[start..end];
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = body[i..].find("<rdf:Description") {
        let s0 = i + off;
        // Find the matching close, allowing nested Descriptions (rare, but
        // parseType="Resource" structs never use the element name so a plain
        // depth count on the tag name is enough).
        let mut depth = 0usize;
        let mut j = s0;
        let mut close = None;
        while j < body.len() {
            if body[j..].starts_with("<rdf:Description") {
                // self-closing?
                let gt = body[j..].find('>').map(|g| j + g);
                match gt {
                    Some(g) if body[..g].ends_with('/') => {
                        if depth == 0 {
                            close = Some(g + 1);
                            break;
                        }
                        j = g + 1;
                        continue;
                    }
                    Some(g) => {
                        depth += 1;
                        j = g + 1;
                        continue;
                    }
                    None => break,
                }
            }
            if body[j..].starts_with("</rdf:Description>") {
                depth -= 1;
                j += "</rdf:Description>".len();
                if depth == 0 {
                    close = Some(j);
                    break;
                }
                continue;
            }
            j += body[j..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
        let Some(c) = close else { break };
        let elem = &body[s0..c];
        // Pan's own block is the one that names a pan Image as its identity
        // (`<pan:id>&lt;pan/Image/…&gt;`). Files Pan wrote earlier on
        // 2026-09-07 named it `<git-lex:id>` instead; that block is Pan's too
        // and is replaced, not kept as a producer's. A producer's block never
        // names a pan Image, whatever pan: fields it tries to write.
        let pan_authored = elem.contains(PAN_NS)
            && (elem.contains("<pan:id>&lt;pan/Image/") || elem.contains("<git-lex:id>&lt;pan/Image/"));
        out.push((pan_authored, elem.to_string()));
        i = c;
    }
    out
}

/// Assemble the packet Pan writes into the image: every Description that was
/// already there and is not Pan's own (kept verbatim — the producer's copia
/// block, an Adobe block, anything), then Pan's root Description. Standard
/// XMP wrapping. Pan never adds a third party's metadata; whatever is in the
/// file when it arrives is what travels (Rob, 2026-09-04).
pub fn compose_packet(existing: Option<&str>, pan_description: &str) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n");
    out.push_str("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n");
    out.push_str("  <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n");
    if let Some(prev) = existing {
        for (pan_authored, elem) in split_descriptions(prev) {
            if !pan_authored {
                out.push_str("    ");
                out.push_str(elem.trim());
                out.push('\n');
            }
        }
    }
    out.push_str(pan_description);
    out.push_str("  </rdf:RDF>\n");
    out.push_str("</x:xmpmeta>\n");
    out.push_str("<?xpacket end=\"w\"?>");
    out
}

/// What the XMP a file ARRIVED with says, as typed quads, read with the real
/// RDF/XML parser: `rdf:about=""` resolves to `media_iri` (facts about this
/// object), named subjects (a producer's Moment, a region) stay themselves,
/// `rdf:datatype` survives. Pan's own block from a previous store is left
/// out (its identity is that store's, not this object's), and so is any
/// stray pan:/git-lex: predicate.
///
/// A packet that is not well-formed RDF/XML is an error — the caller refuses
/// the file (same rule Rob set for the copia block: reject malformed, never
/// repair, never store what the graph cannot say). No `<rdf:RDF>` at all is
/// also an error: an XMP chunk with no RDF is not XMP.
pub fn load_packet_statements(packet: &str, media_iri: &str) -> Result<Vec<oxigraph::model::Quad>> {
    let (s, e) = match (find_rdf_open(packet), packet.rfind("</rdf:RDF>")) {
        (Some(s), Some(e)) if e > s => (s, e + "</rdf:RDF>".len()),
        _ => return Err(anyhow!("XMP packet has no rdf:RDF element")),
    };
    // Root tag with every namespace declared anywhere in the element hoisted
    // onto it, so a Description can be dropped without losing a binding.
    let hoisted = hoist_namespaces(&packet[s..e]);
    // First: is the WHOLE element valid RDF/XML? Asked of the full text, not
    // of the Descriptions picked out of it — a Description that never closes
    // would otherwise be picked out as "none" and pass in silence.
    Store::new()
        .context("scratch store")?
        .load_from_reader(
            oxigraph::io::RdfParser::from_format(RdfFormat::RdfXml).with_base_iri(media_iri).map_err(|e| anyhow!("base IRI: {e}"))?,
            hoisted.as_bytes(),
        )
        .map_err(|e| anyhow!("XMP in the file is not valid RDF/XML: {e}"))?;
    let Some(root_end) = hoisted.find('>') else { return Err(anyhow!("XMP packet: unterminated rdf:RDF tag")) };
    let root_tag = &hoisted[..=root_end];
    let mut body = String::new();
    for (pan_authored, elem) in split_descriptions(&hoisted) {
        if !pan_authored {
            body.push_str(&elem);
            body.push('\n');
        }
    }
    let rebuilt = format!("{root_tag}{body}</rdf:RDF>");
    let store = Store::new().context("scratch store")?;
    store
        .load_from_reader(
            oxigraph::io::RdfParser::from_format(RdfFormat::RdfXml).with_base_iri(media_iri).map_err(|e| anyhow!("base IRI: {e}"))?,
            rebuilt.as_bytes(),
        )
        .map_err(|e| anyhow!("XMP in the file is not valid RDF/XML: {e}"))?;
    // Pan's own Description is already out. What is left out here is only
    // what must never travel between stores: pan: vocabulary (a previous
    // store's paths and records) and a git-lex:id some older writer put in
    // the file (identity of that store, never this object's). Everything else a
    // producer asserts loads as written — Pan builds no rules around what a
    // particular producer happens to send (Rob, 2026-09-07); a wrong subject
    // in a producer's block is the producer's to fix.
    let git_lex_id = format!("{}id", crate::config::GIT_LEX_NS);
    // The ONE pan: field a producer may write (pan.ttl v0.3.1; Rob ruled on
    // 2026-09-04 that Horae sends pan:relatedToId=<copia/Moment/id>). Every other
    // pan: statement in an arriving file is a previous store's and stays out.
    let pan_related_to = format!("{PAN_NS}relatedToId");
    let git_lex_related_to = format!("{}relatedToId", crate::config::GIT_LEX_NS);
    let all: Vec<oxigraph::model::Quad> = store.iter().collect::<std::result::Result<_, _>>().context("read packet quads")?;
    let quads: Vec<oxigraph::model::Quad> = all
        .into_iter()
        .filter(|q| {
            let p = q.predicate.as_str();
            p == pan_related_to || !(p.starts_with(PAN_NS) || p == git_lex_id)
        })
        .map(|q| {
            // A reference written the git-lex way — `<copia/Moment/x>` as a
            // field value, exactly as frontmatter writes it — becomes the IRI
            // in the graph, so the join is an edge, not a string. The file
            // keeps the text. Applies to pan:relatedToId (the ruled field) and
            // to git-lex:relatedToId (the universal one) alike.
            let p = q.predicate.as_str();
            if p != pan_related_to && p != git_lex_related_to {
                return q;
            }
            let Term::Literal(lit) = &q.object else { return q };
            match crate::iri_from_bracket(lit.value()).and_then(|iri| oxigraph::model::NamedNode::new(iri).ok()) {
                Some(node) => oxigraph::model::Quad::new(q.subject.clone(), q.predicate.clone(), node, q.graph_name.clone()),
                None => q,
            }
        })
        .collect();
    Ok(quads)
}

// ── Packet reading — the REAL RDF parser (the change from Pool) ─────────────

/// One object of a fact, preserving whether it was an IRI or a literal — so
/// `rdf:type` and other IRI-valued predicates survive ingest as IRIs rather
/// than degrading to string literals (the round-trip corruption the review
/// caught).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjTerm {
    Iri(String),
    Literal(String),
}

impl ObjTerm {
    pub fn value(&self) -> &str {
        match self {
            ObjTerm::Iri(s) | ObjTerm::Literal(s) => s,
        }
    }
    pub fn is_iri(&self) -> bool {
        matches!(self, ObjTerm::Iri(_))
    }
}

/// The facts read for one subject in a packet. `subject: None` is the root
/// Description (no rdf:about, or rdf:about="" — facts about the media object
/// itself); `Some(iri)` is a named sub-subject (region etc.).
#[derive(Debug, Clone)]
pub struct ParsedSubject {
    pub subject: Option<String>,
    /// Full-IRI predicate → object terms (a Bag/Seq flattens to its members,
    /// in order; IRI objects keep their IRI-ness).
    pub facts: Vec<(String, Vec<ObjTerm>)>,
    /// Nested structs (`rdf:parseType="Resource"`), keyed by the predicate
    /// pointing at them: one inner list per struct, each a list of its fields.
    /// This is where an enrichment reference bag arrives — a predicate with
    /// several structured members, none of which is a plain literal.
    pub structs: Vec<(String, Vec<Vec<(String, ObjTerm)>>)>,
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
    // bare RDF/XML, so slice the rdf:RDF element out. Match on the FULL tag
    // (with its `<` and a following space or `>`) so an rdf:RDF string sitting
    // inside a literal value can't fool the slice.
    let start = find_rdf_open(packet)
        .ok_or_else(|| anyhow!("XMP packet has no <rdf:RDF> element"))?;
    let end = packet
        .rfind("</rdf:RDF>")
        .ok_or_else(|| anyhow!("XMP packet has no </rdf:RDF> close"))?
        + "</rdf:RDF>".len();
    if end <= start {
        return Err(anyhow!("XMP packet <rdf:RDF> close precedes its open"));
    }
    let rdf_xml = hoist_namespaces(&packet[start..end]);
    let rdf_xml = sanitize_about_iris(&rdf_xml);

    let store = Store::new().context("in-memory store for XMP parse")?;
    // Give the RDF/XML parser a base IRI: an empty rdf:about="" resolves
    // against it, so the standard Adobe root Description (rdf:about="") no
    // longer errors and is recognizable as the root by matching this base.
    store
        .load_from_reader(
            oxigraph::io::RdfParser::from_format(RdfFormat::RdfXml).with_base_iri(XMP_BASE).unwrap(),
            rdf_xml.as_bytes(),
        )
        .context("parse XMP RDF/XML")?;

    // ── First pass: index container membership and container-typed nodes. ──
    // A container node (rdf:Bag/Seq/Alt) carries rdf:_N members; we flatten
    // those to an ordered value list keyed by the container's node id.
    let mut container_members: HashMap<String, Vec<(u32, ObjTerm)>> = HashMap::new();
    let mut container_nodes: HashSet<String> = HashSet::new();
    let type_pred = format!("{RDF_NS}type");
    for quad in store.iter() {
        let quad = quad.context("XMP quad (container pass)")?;
        let subj_key = subject_key(&quad.subject);
        let pred = quad.predicate.as_str();
        if let Some(n) = pred.strip_prefix(RDF_NS).and_then(|l| l.strip_prefix('_')) {
            if let Ok(n) = n.parse::<u32>() {
                container_members
                    .entry(subj_key.clone())
                    .or_default()
                    .push((n, obj_term(&quad.object)));
                container_nodes.insert(subj_key);
                continue;
            }
        }
        if pred == type_pred {
            if let Term::NamedNode(t) = &quad.object {
                if matches!(
                    t.as_str(),
                    _ if t.as_str().starts_with(RDF_NS)
                        && matches!(t.as_str().strip_prefix(RDF_NS), Some("Bag" | "Seq" | "Alt"))
                ) {
                    container_nodes.insert(subj_key);
                }
            }
        }
    }

    // ── Second pass: gather facts per TOP-LEVEL subject. ──
    // Top-level = a named subject (rdf:about IRI) OR the base IRI (the empty
    // rdf:about="" root Description). Container nodes and other blank nodes are
    // NOT subjects — a fact whose object is such a node is resolved inline:
    //   - container node  → flatten to its ordered members
    //   - other blank node → a nested struct; skip it rather than emit a
    //     blank-node label as a garbage literal (the fabricated-fact bug).
    let mut by_subject: HashMap<Option<String>, Vec<(String, Vec<ObjTerm>)>> = HashMap::new();
    for quad in store.iter() {
        let quad = quad.context("XMP quad (fact pass)")?;
        let subj_key = subject_key(&quad.subject);
        // Only real subjects surface. Blank/container nodes are inlined, never
        // their own ParsedSubject.
        let subject_out: Option<String> = match &quad.subject {
            oxigraph::model::NamedOrBlankNode::NamedNode(n) => {
                if n.as_str() == XMP_BASE {
                    None // the empty-about root Description = the media object
                } else {
                    Some(n.as_str().to_string())
                }
            }
            // Blank-node subject: skip unless it's a container we already
            // flattened at its referencing predicate.
            oxigraph::model::NamedOrBlankNode::BlankNode(_) => continue,
        };

        let pred = quad.predicate.as_str();
        // rdf:_N and container rdf:type were consumed in pass one.
        if pred.strip_prefix(RDF_NS).and_then(|l| l.strip_prefix('_')).and_then(|n| n.parse::<u32>().ok()).is_some() {
            continue;
        }

        let values: Vec<ObjTerm> = match &quad.object {
            Term::BlankNode(b) => {
                let key = format!("_:{}", b.as_str());
                match container_members.get(&key) {
                    Some(members) => {
                        let mut m = members.clone();
                        m.sort_by_key(|(n, _)| *n);
                        m.into_iter().map(|(_, v)| v).collect()
                    }
                    // A non-container blank node = nested struct. Drop it: we
                    // do NOT fabricate a fact whose value is a blank-node label.
                    None => continue,
                }
            }
            other => vec![obj_term(other)],
        };
        let _ = subj_key; // (kept for parity with the container pass keys)
        by_subject.entry(subject_out).or_default().push((pred.to_string(), values));
    }

    // ── Third pass: nested structs (rdf:parseType="Resource"). ──
    // A struct is a blank node with ordinary predicates — Pan's own `pan:image`
    // wrapper, and each `rdf:li` inside an enrichment reference bag. Pass two
    // deliberately drops blank-node values rather than fabricating a fact whose
    // value is a node label; here we recover their CONTENTS, which is the only
    // way an image's own identity block survives a read.
    let mut struct_facts: HashMap<String, Vec<(String, ObjTerm)>> = HashMap::new();
    for quad in store.iter() {
        let quad = quad.context("XMP quad (struct pass)")?;
        let key = subject_key(&quad.subject);
        if !key.starts_with("_:") || container_nodes.contains(&key) {
            continue;
        }
        let pred = quad.predicate.as_str();
        if pred == type_pred
            || pred
                .strip_prefix(RDF_NS)
                .and_then(|l| l.strip_prefix('_'))
                .and_then(|n| n.parse::<u32>().ok())
                .is_some()
        {
            continue;
        }
        // A struct field whose value is itself a container (a nested reference
        // bag) is reported through `structs` on the OWNING subject below; here
        // we keep only the scalar fields.
        if let Term::BlankNode(_) = &quad.object {
            continue;
        }
        struct_facts
            .entry(key)
            .or_default()
            .push((pred.to_string(), obj_term(&quad.object)));
    }

    // Attach struct values to whoever points at them, and HOIST Pan's own
    // `pan:image` wrapper onto the media subject: the wrapper exists to give
    // flat viewers readable labels, and must not become a level of indirection
    // for anything reading the facts back.
    let image_pred = format!("{PAN_NS}image");
    // Owner key: None = the media object (rdf:about=""), Some(iri) = a named
    // subject, Some("_:x") = a struct node (the `pan:image` wrapper owns the
    // reference bags, so it must be a valid owner or its contents vanish).
    let mut structs_by_subject: HashMap<Option<String>, Vec<(String, Vec<Vec<(String, ObjTerm)>>)>> =
        HashMap::new();
    for quad in store.iter() {
        let quad = quad.context("XMP quad (struct-attach pass)")?;
        let owner: Option<String> = match &quad.subject {
            oxigraph::model::NamedOrBlankNode::NamedNode(n) if n.as_str() == XMP_BASE => None,
            oxigraph::model::NamedOrBlankNode::NamedNode(n) => Some(n.as_str().to_string()),
            oxigraph::model::NamedOrBlankNode::BlankNode(b) => Some(format!("_:{}", b.as_str())),
        };
        let Term::BlankNode(b) = &quad.object else { continue };
        let key = format!("_:{}", b.as_str());
        let pred = quad.predicate.as_str().to_string();

        if let Some(fields) = struct_facts.get(&key) {
            if pred == image_pred {
                // Hoist: the wrapper's fields are the media object's own facts.
                let entry = by_subject.entry(owner.clone()).or_default();
                for (p, v) in fields {
                    entry.push((p.clone(), vec![v.clone()]));
                }
            } else {
                structs_by_subject
                    .entry(owner.clone())
                    .or_default()
                    .push((pred.clone(), vec![fields.clone()]));
            }
        }

        // A container of structs (an enrichment reference bag): collect every
        // member's fields under the referencing predicate.
        if container_nodes.contains(&key) {
            let members: Vec<Vec<(String, ObjTerm)>> = container_members
                .get(&key)
                .map(|m| {
                    let mut m = m.clone();
                    m.sort_by_key(|(n, _)| *n);
                    m.into_iter()
                        .filter_map(|(_, t)| match t {
                            ObjTerm::Literal(ref l) => struct_facts.get(&format!("_:{l}")).cloned(),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !members.is_empty() {
                structs_by_subject
                    .entry(owner.clone())
                    .or_default()
                    .push((pred, members));
            }
        }
    }

    // Hoisted `pan:image` fields may include reference bags; those were
    // attached to the blank wrapper, so re-home them onto the media subject.
    if let Some(wrapper_key) = store
        .iter()
        .filter_map(|q| q.ok())
        .find(|q| q.predicate.as_str() == image_pred)
        .and_then(|q| match q.object {
            Term::BlankNode(b) => Some(format!("_:{}", b.as_str())),
            _ => None,
        })
    {
        if let Some(inner) = structs_by_subject.remove(&Some(wrapper_key)) {
            structs_by_subject.entry(None).or_default().extend(inner);
        }
    }

    let mut out: Vec<ParsedSubject> = by_subject
        .into_iter()
        .map(|(subject, mut facts)| {
            facts.sort_by(|a, b| a.0.cmp(&b.0));
            let mut structs = structs_by_subject.remove(&subject).unwrap_or_default();
            structs.sort_by(|a, b| a.0.cmp(&b.0));
            ParsedSubject { subject, facts, structs }
        })
        .collect();
    // Root (None) first, then named subjects in stable order.
    out.sort_by(|a, b| a.subject.cmp(&b.subject));
    Ok(out)
}

/// The synthetic base IRI the RDF/XML parser resolves rdf:about="" against.
/// A root Description with an empty about becomes THIS subject; we map it back
/// to `None` (= the media object itself). Never stored — a parse-time marker
/// only (kept in the pan namespace family; no urn: anywhere in Pan).
const XMP_BASE: &str = "https://repolex.ai/ontology/pan/xmp-root";


/// Re-declare every namespace prefix the packet defines onto the `<rdf:RDF>`
/// element, so a prefix declared inside one element is usable by its SIBLINGS.
///
/// Why this exists: Pool wrote `xmlns:copia=` on the root `<rdf:Description>`
/// only, then used `copia:` in the sibling Descriptions that carry regions and
/// poses. XML scopes a declaration to the element it appears on and that
/// element's descendants — siblings are NOT covered — so those packets are
/// strictly malformed. Lenient tools (exiftool) accept them; a real RDF parser
/// refuses the whole document, which would mean importing a legacy image with
/// NO metadata at all rather than with all of it.
///
/// The repair invents nothing: it collects the declarations the document
/// itself makes and widens their scope to the whole document. A prefix bound
/// to two different namespaces in one packet is genuinely ambiguous, so it is
/// left exactly as written and the parser's own error stands.
fn hoist_namespaces(rdf_xml: &str) -> String {
    let mut bindings: HashMap<String, String> = HashMap::new();
    let mut conflicted: HashSet<String> = HashSet::new();

    let bytes = rdf_xml.as_bytes();
    let mut i = 0;
    while let Some(rel) = rdf_xml[i..].find("xmlns:") {
        let at = i + rel;
        let after = at + "xmlns:".len();
        let Some(eq) = rdf_xml[after..].find('=') else { break };
        let prefix = &rdf_xml[after..after + eq];
        if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            i = after;
            continue;
        }
        let vstart = after + eq + 1;
        let quote = match bytes.get(vstart) {
            Some(b'"') => '"',
            Some(b'\'') => '\'',
            _ => {
                i = after;
                continue;
            }
        };
        let Some(vend) = rdf_xml[vstart + 1..].find(quote) else { break };
        let ns = &rdf_xml[vstart + 1..vstart + 1 + vend];
        match bindings.get(prefix) {
            Some(existing) if existing != ns => {
                conflicted.insert(prefix.to_string());
            }
            _ => {
                bindings.insert(prefix.to_string(), ns.to_string());
            }
        }
        i = vstart + 1 + vend;
    }

    // Nothing to widen if the root already declares everything.
    let Some(open_end) = rdf_xml.find('>') else { return rdf_xml.to_string() };
    let root_tag = &rdf_xml[..open_end];
    let mut additions = String::new();
    let mut names: Vec<&String> = bindings.keys().collect();
    names.sort();
    for prefix in names {
        if conflicted.contains(prefix) {
            continue;
        }
        if root_tag.contains(&format!("xmlns:{prefix}=")) {
            continue;
        }
        additions.push_str(&format!(" xmlns:{prefix}=\"{}\"", bindings[prefix]));
    }
    if additions.is_empty() {
        return rdf_xml.to_string();
    }
    format!("{}{}{}", root_tag, additions, &rdf_xml[open_end..])
}

/// Percent-encode spaces in rdf:about="..." IRIs so strict RDF/XML parsers don't fail
/// on legacy SAM3 descriptors like rdf:about="Sam3Region:tree trunk/01".
fn sanitize_about_iris(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(pos) = rest.find("rdf:about=") {
        out.push_str(&rest[..pos + "rdf:about=".len()]);
        let after = &rest[pos + "rdf:about=".len()..];
        if let Some(quote) = after.chars().next() {
            if quote == '"' || quote == '\'' {
                out.push(quote);
                let val_start = 1;
                if let Some(val_end) = after[val_start..].find(quote) {
                    let val = &after[val_start..val_start + val_end];
                    let sanitized = val.replace(' ', "%20");
                    out.push_str(&sanitized);
                    out.push(quote);
                    rest = &after[val_start + val_end + 1..];
                    continue;
                }
            }
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Find the real `<rdf:RDF` element open (followed by whitespace or `>`), not
/// a literal that merely contains the substring.
fn find_rdf_open(s: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = s[from..].find("<rdf:RDF") {
        let idx = from + rel;
        let after = s[idx + "<rdf:RDF".len()..].chars().next();
        if matches!(after, Some(c) if c.is_whitespace() || c == '>') {
            return Some(idx);
        }
        from = idx + "<rdf:RDF".len();
    }
    None
}

fn subject_key(s: &oxigraph::model::NamedOrBlankNode) -> String {
    match s {
        oxigraph::model::NamedOrBlankNode::NamedNode(n) => n.as_str().to_string(),
        oxigraph::model::NamedOrBlankNode::BlankNode(b) => format!("_:{}", b.as_str()),
    }
}

fn obj_term(t: &Term) -> ObjTerm {
    match t {
        Term::Literal(l) => ObjTerm::Literal(l.value().to_string()),
        Term::NamedNode(n) => {
            // The base IRI leaking into an object position (rare) is treated as
            // a plain reference to the root; keep it as an IRI verbatim.
            ObjTerm::Iri(n.as_str().to_string())
        }
        Term::BlankNode(b) => ObjTerm::Literal(b.as_str().to_string()),
        _ => ObjTerm::Literal(t.to_string()),
    }
}

// ── Pixel hash — the stamp-invariant instrument (lifted verbatim from Pool) ──

/// Compute a hash of a PNG's PIXELS, stable across XMP/metadata edits.
///
/// NOT an identity — Pan's identity is the assigned panId. This exists to PIN
/// the stamp invariant (stamping never touches the image): equal hash before
/// and after = pixels untouched.
///
/// CANONICAL CROSS-REPO DEFINITION (matches Pool + OpenIris byte-for-byte):
/// `sha256` of the decoded pixel buffer **normalized to 8-bit RGB (no alpha),
/// row-major top-to-bottom, 3 bytes/pixel in R,G,B order** — exactly PIL's
/// `Image.open(png).convert("RGB").tobytes()`. Palette → expanded; grayscale →
/// replicated to R=G=B; alpha → stripped; 16-bit → high byte (`>>8`).
pub fn pixel_hash(png_bytes: &[u8]) -> Result<String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    // EXPAND: palette → RGB, sub-8-bit grayscale/tRNS → 8-bit. Leaves 16-bit
    // as 16-bit and alpha as-is; we handle those two below to match PIL.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().context("pixel-hash: decode PNG info")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buf)
        .context("pixel-hash: read PNG frame")?;
    let data = &buf[..frame.buffer_size()];

    let (color, depth) = (frame.color_type, frame.bit_depth);
    let channels = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            return Err(anyhow!("pixel-hash: unexpected Indexed color after EXPAND"))
        }
    };
    let bytes_per_sample = if depth == png::BitDepth::Sixteen { 2 } else { 1 };
    let stride = channels * bytes_per_sample;
    if stride == 0 || data.len() % stride != 0 {
        return Err(anyhow!(
            "pixel-hash: buffer {} not divisible by stride {} (color={:?} depth={:?})",
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
pub(crate) mod tests {
    use super::*;

    /// Test helper: a packet with only the identity block filled in.
    fn simple_packet(id: &str, media: &str, created: &str) -> String {
        build_packet(&ImagePacket {
            iri: format!("https://repolex.ai/pan/Image/{id}"),
            media_path: media.into(),
            source_file: String::new(),
            created_date: created.into(),
            ..Default::default()
        })
    }


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
    fn stamp_preserves_pixels() {
        // THE invariant: a metadata edit never touches the image.
        let png = make_test_png(16, 16, 7);
        let hash_before = pixel_hash(&png).unwrap();
        let packet = simple_packet("abc123xy", "media/image/x.png", "2026-07-15T00:00:00Z");
        let stamped = write_packet_into_png_bytes(&png, &packet).unwrap();
        let hash_after = pixel_hash(&stamped).unwrap();
        assert_eq!(hash_before, hash_after, "stamping changed the pixels");
        assert_ne!(png, stamped, "file bytes should differ (packet embedded)");
    }

    fn get<'a>(facts: &'a [(String, Vec<ObjTerm>)], iri: &str) -> Vec<&'a ObjTerm> {
        facts
            .iter()
            .find(|(p, _)| p == iri)
            .map(|(_, v)| v.iter().collect())
            .unwrap_or_default()
    }
    fn vals(facts: &[(String, Vec<ObjTerm>)], iri: &str) -> Vec<String> {
        get(facts, iri).into_iter().map(|t| t.value().to_string()).collect()
    }

    /// A packet as a producer (Horae) writes it into the file before Pan ever
    /// sees it: standard wrapping, one copia Description about the Moment.
    fn producer_packet(descriptions: &str) -> String {
        format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n{descriptions}\n</rdf:RDF>\n\
             </x:xmpmeta>\n<?xpacket end=\"w\"?>"
        )
    }

    #[test]
    fn packet_round_trips_through_real_parser() {
        // The file arrives with a producer's copia Description; Pan appends
        // its own block. Both must come back through the real parser, the
        // copia one untouched and typed.
        const COPIA: &str = "https://repolex.ai/ontology/copia/";
        let copia_block = format!(
            "<rdf:Description rdf:about=\"https://repolex.ai/copia/Moment/3hyh7rwekpmq\" xmlns:copia=\"{COPIA}\" xmlns:git-lex=\"https://repolex.ai/ontology/git-lex/\">\n\
               <copia:momentId>3hyh7rwekpmq</copia:momentId>\n\
               <copia:sceneMood>calm &amp; &lt;bright&gt;</copia:sceneMood>\n\
               <copia:genSteps rdf:datatype=\"http://www.w3.org/2001/XMLSchema#integer\">12</copia:genSteps>\n\
               <git-lex:createdDate rdf:datatype=\"http://www.w3.org/2001/XMLSchema#dateTime\">2026-09-03T23:54:18-07:00</git-lex:createdDate>\n\
               <copia:sceneObjects><rdf:Bag><rdf:li>wolf</rdf:li><rdf:li>forest</rdf:li></rdf:Bag></copia:sceneObjects>\n\
             </rdf:Description>"
        );
        let arrived = producer_packet(&copia_block);
        let quads = load_packet_statements(&arrived, "https://repolex.ai/pan/Image/abc123xy").unwrap();
        assert!(quads.len() >= 5);
        assert!(
            quads.iter().any(|q| q.predicate.as_str() == "https://repolex.ai/ontology/git-lex/createdDate"),
            "a producer's git-lex:createdDate on its own subject is declared vocabulary and must load"
        );
        let steps = quads
            .iter()
            .find(|q| q.predicate.as_str() == format!("{COPIA}genSteps"))
            .expect("genSteps loaded");
        assert!(
            steps.object.to_string().contains("XMLSchema#integer"),
            "rdf:datatype survives the real parser: {}",
            steps.object
        );

        let pan_desc = build_pan_description(&ImagePacket {
            iri: "https://repolex.ai/pan/Image/abc123xy".into(),
            media_path: "image/2026/09/04/abc123xy.png".into(),
            source_file: String::new(),
            created_date: "2026-09-04T01:00:00-07:00".into(),
            curation: vec![],
            thumbnail: Some(ThumbRef { id: "th1umb01".into(), path: "image/img/jpg/2026/09/04/abc123xy_512.jpg".into(), width: 341, height: 512 }),
            ..Default::default()
        });
        let packet = compose_packet(Some(&arrived), &pan_desc);
        assert!(packet.contains(&copia_block), "the producer's Description is carried verbatim");
        let parsed = parse_packet(&packet).unwrap();
        let root = parsed.iter().find(|p| p.subject.is_none()).expect("root block");
        assert_eq!(
            vals(&root.facts, "https://repolex.ai/ontology/pan/mediaPath"),
            vec!["image/2026/09/04/abc123xy.png"]
        );
        let moment = parsed
            .iter()
            .find(|p| p.subject.as_deref() == Some("https://repolex.ai/copia/Moment/3hyh7rwekpmq"))
            .expect("copia Description is its own subject");
        assert_eq!(vals(&moment.facts, &format!("{COPIA}sceneMood")), vec!["calm & <bright>"]);
        assert_eq!(vals(&moment.facts, &format!("{COPIA}sceneObjects")), vec!["wolf", "forest"]);

        // A rewrite keeps the copia Description and re-authors only Pan's.
        let again = compose_packet(Some(&packet), &pan_desc);
        let parts = split_descriptions(&again);
        assert_eq!(parts.iter().filter(|(pan, _)| *pan).count(), 1, "exactly one pan block");
        assert_eq!(parts.iter().filter(|(pan, _)| !*pan).count(), 1, "the copia block survives");

        // Reading the rewritten packet back skips Pan's own block: a previous
        // store's identity is not a fact about the next store's object.
        let back = load_packet_statements(&again, "https://repolex.ai/pan/Image/next").unwrap();
        assert!(back.iter().all(|q| !q.predicate.as_str().starts_with(PAN_NS)), "no pan: predicates read back");
        assert!(back.iter().any(|q| q.predicate.as_str() == format!("{COPIA}momentId")), "copia facts read back");
    }

    #[test]
    fn pan_related_to_id_from_a_producer_is_the_one_pan_field_let_through() {
        // Horae's line, as Rob ruled it ("pan:relatedToId=<copia/Moment/id>"):
        // on the "this file" Description, in Pan's namespace. Every other pan:
        // statement from a producer still stays out.
        let arrived = producer_packet(
            "<rdf:Description rdf:about=\"\" xmlns:pan=\"https://repolex.ai/ontology/pan/\">\
               <pan:relatedToId>&lt;copia/Moment/t8mjvhwszff9-3-4&gt;</pan:relatedToId>\
               <pan:mediaPath>somewhere/else.png</pan:mediaPath>\
             </rdf:Description>",
        );
        let image = "https://repolex.ai/pan/Image/pgtby2ft";
        let quads = load_packet_statements(&arrived, image).unwrap();
        let edge = quads
            .iter()
            .find(|q| q.predicate.as_str() == "https://repolex.ai/ontology/pan/relatedToId")
            .expect("pan:relatedToId loaded");
        assert_eq!(edge.subject.to_string(), format!("<{image}>"));
        assert_eq!(edge.object.to_string(), "<https://repolex.ai/copia/Moment/t8mjvhwszff9-3-4>", "resolved to the IRI");
        assert!(
            !quads.iter().any(|q| q.predicate.as_str() == "https://repolex.ai/ontology/pan/mediaPath"),
            "a producer's pan:mediaPath is NOT a fact about this copy and stays out"
        );
    }

    #[test]
    fn related_to_id_in_bracket_form_becomes_an_edge() {
        // The universal form is resolved the same way.
        let arrived = producer_packet(
            "<rdf:Description rdf:about=\"\" xmlns:git-lex=\"https://repolex.ai/ontology/git-lex/\">\
               <git-lex:relatedToId>&lt;copia/Moment/t8mjvhwszff9-3-4&gt;</git-lex:relatedToId>\
             </rdf:Description>",
        );
        let image = "https://repolex.ai/pan/Image/pgtby2ft";
        let quads = load_packet_statements(&arrived, image).unwrap();
        let edge = quads
            .iter()
            .find(|q| q.predicate.as_str() == "https://repolex.ai/ontology/git-lex/relatedToId")
            .expect("relatedToId loaded");
        assert_eq!(edge.subject.to_string(), format!("<{image}>"), "about=\"\" is this image");
        assert_eq!(
            edge.object.to_string(),
            "<https://repolex.ai/copia/Moment/t8mjvhwszff9-3-4>",
            "the bracket form is resolved to the IRI: an edge, not a string"
        );
        // Something that only looks like a reference stays what it was.
        assert_eq!(crate::iri_from_bracket("<not a ref>"), None);
        assert_eq!(crate::iri_from_bracket("<copia/Moment>"), None);
        assert_eq!(crate::iri_from_bracket("copia/Moment/x"), None);
    }

    #[test]
    fn malformed_packet_is_rejected() {
        let iri = "https://repolex.ai/pan/Image/x";
        assert!(load_packet_statements(&producer_packet("<rdf:Description><unclosed>"), iri).is_err());
        assert!(load_packet_statements("not xml at all", iri).is_err());
        assert!(load_packet_statements("", iri).is_err());
        // Well-formed but empty RDF is not an error: nothing to say is allowed.
        assert!(load_packet_statements(&producer_packet(""), iri).unwrap().is_empty());
    }

    #[test]
    fn parses_standard_adobe_root_description() {
        // The real-world case the review caught: a packet whose root uses
        // rdf:about="" (standard Adobe/XMP form) must parse, not error.
        let packet = format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
             <rdf:RDF xmlns:rdf=\"{RDF_NS}\">\n\
             <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n\
             <dc:title>Hello</dc:title>\n\
             </rdf:Description>\n\
             </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>"
        );
        let parsed = parse_packet(&packet).unwrap();
        let root = parsed.iter().find(|p| p.subject.is_none()).expect("empty-about = root");
        assert_eq!(vals(&root.facts, "http://purl.org/dc/elements/1.1/title"), vec!["Hello"]);
    }

    #[test]
    fn stamp_replaces_prior_xmp_and_preserves_other_text() {
        let png = make_test_png(8, 8, 1);
        let p1 = simple_packet("oldid111", "a.png", "2026-01-01T00:00:00Z");
        let s1 = write_packet_into_png_bytes(&png, &p1).unwrap();
        let p2 = simple_packet("newid222", "b.png", "2026-01-02T00:00:00Z");
        let s2 = write_packet_into_png_bytes(&s1, &p2).unwrap();
        let packet = read_xmp_packet_from_bytes(&s2).unwrap().expect("xmp present");
        assert!(packet.contains("newid222"), "new packet wins");
        assert!(!packet.contains("oldid111"), "old packet fully replaced");
    }

    #[test]
    fn is_png_detects() {
        assert!(is_png(&make_test_png(1, 1, 0)));
        assert!(!is_png(b"\xFF\xD8\xFF jpeg-ish"));
    }
}

#[cfg(test)]
mod legacy_tests {
    use super::*;

    /// Pool's real defect, reproduced exactly: `xmlns:copia` declared on the
    /// root Description only, then used by a SIBLING Description. Strictly
    /// malformed; lenient readers accept it. If Pan refused these, importing a
    /// legacy image would silently yield an image with no metadata rather than
    /// one with all of it.
    const POOL_SHAPE: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about="" xmlns:copia="https://repolex.ai/ontology/copia/">
      <copia:caption>a wolf</copia:caption>
    </rdf:Description>
  <rdf:Description rdf:about="Sam3Region:wolf/01">
    <rdf:type rdf:resource="https://repolex.ai/ontology/copia/Sam3Region"/>
    <copia:regionDescriptor>wolf</copia:regionDescriptor>
    <copia:regionScore>0.91</copia:regionScore>
  </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;

    #[test]
    fn out_of_scope_prefix_is_repaired_not_refused() {
        let parsed = parse_packet(POOL_SHAPE).expect("legacy packet must parse");
        let root = parsed.iter().find(|p| p.subject.is_none()).expect("root");
        assert!(
            root.facts.iter().any(|(p, v)| p.ends_with("caption")
                && v.iter().any(|t| t.value() == "a wolf")),
            "root facts survive"
        );
        let region = parsed
            .iter()
            .find(|p| p.subject.as_deref() == Some("Sam3Region:wolf/01"))
            .expect("the sibling Description must parse, not sink the document");
        assert!(
            region.facts.iter().any(|(p, v)| p.ends_with("regionDescriptor")
                && v.iter().any(|t| t.value() == "wolf")),
            "region facts survive"
        );
    }

    #[test]
    fn a_prefix_bound_two_ways_is_left_alone() {
        // Genuine ambiguity must not be silently resolved in our favour.
        let ambiguous = POOL_SHAPE.replace(
            r#"<rdf:Description rdf:about="Sam3Region:wolf/01">"#,
            r#"<rdf:Description rdf:about="Sam3Region:wolf/01" xmlns:copia="https://example.com/other/">"#,
        );
        // The second binding is in scope where it is used, so this parses; the
        // point is that the hoist did not overwrite it with the root's binding.
        let parsed = parse_packet(&ambiguous).expect("parses on its own declarations");
        let region = parsed
            .iter()
            .find(|p| p.subject.as_deref() == Some("Sam3Region:wolf/01"))
            .expect("region");
        assert!(
            region.facts.iter().any(|(p, _)| p.starts_with("https://example.com/other/")),
            "the element's own binding wins, never the hoisted one"
        );
    }
}

#[cfg(test)]
mod flat_block_tests {
    use super::*;

    #[test]
    fn pan_block_is_flat_and_every_id_is_in_bracket_form() {
        let p = ImagePacket {
            iri: "https://repolex.ai/pan/Image/altocnif".into(),
            media_path: "image/2026/09/05/20260905-000009-altocnif.png".into(),
            source_file: String::new(),
            created_date: "2026-09-05T00:00:09-07:00".into(),
            short_description: Some("A wolf on a ridge at dusk.".into()),
            long_description: Some("A grey wolf stands on a rocky ridge, lit from the left by a low sun.".into()),
            scene_objects: vec!["wolf".into(), "rock".into(), "sky".into()],
            scene: vec![("sceneMood".into(), "still".into())],
            curation: vec![],
            thumbnail: Some(ThumbRef { id: "th2umb02".into(), path: "image/img/jpg/2026/09/05/20260905-000009-altocnif_512.jpg".into(), width: 341, height: 512 }),
            enrichment: vec![(
                "captionData".into(),
                vec![crate::enrich::EnrichmentRef {
                    id: "jz55pu47".into(),
                    model: "qwen/qwen3.8-27b".into(),
                    path: "caption/2026/09/05/altocnif.xml".into(),
                    count: None,
                    produced_date: "2026-09-05T10:02:40-07:00".into(),
                }],
            )],
            ..Default::default()
        };
        let desc = build_pan_description(&p);
        assert!(!desc.contains("<pan:image"), "no wrapper struct: {desc}");
        assert!(desc.contains("<pan:id>&lt;pan/Image/altocnif&gt;</pan:id>"), "image id in bracket form: {desc}");
        assert!(desc.contains("<pan:id>&lt;pan/Enrichment/jz55pu47&gt;</pan:id>"), "enrichment id in bracket form: {desc}");
        assert!(desc.contains("<pan:id>&lt;pan/Thumbnail/th2umb02&gt;</pan:id>"), "thumbnail id in bracket form inside the struct: {desc}");
        let parsed = parse_packet(&build_packet(&p)).unwrap();
        let th = parsed
            .iter()
            .flat_map(|s| s.structs.iter())
            .find(|(pred, _)| pred.ends_with("/thumbnail"))
            .and_then(|(_, members)| members.first())
            .expect("the reader returns the thumbnail struct");
        let id = th.iter().find(|(f, _)| f.ends_with("/id")).map(|(_, v)| v.value().to_string());
        assert_eq!(id.as_deref(), Some("<pan/Thumbnail/th2umb02>"), "reader returns the thumbnail id: {th:?}");
        assert!(desc.contains("<pan:createdDate>"), "creation time under pan:, not git-lex: {desc}");
        assert!(desc.contains("<pan:shortDescription>A wolf on a ridge at dusk.</pan:shortDescription>"), "{desc}");
        assert!(desc.contains("<pan:longDescription>"), "{desc}");
        assert!(desc.contains("<pan:sceneObjects><rdf:Bag><rdf:li>wolf</rdf:li><rdf:li>rock</rdf:li><rdf:li>sky</rdf:li></rdf:Bag></pan:sceneObjects>"), "scene objects as a bag: {desc}");
        assert!(desc.contains("<pan:sceneMood>still</pan:sceneMood>"), "{desc}");
        assert!(!desc.contains("dc:"), "no dc:description: {desc}");
        assert!(!desc.contains("git-lex"), "the file carries only the pan namespace in Pan's block: {desc}");
        assert!(!desc.contains("https://repolex.ai/pan/"), "no expanded IRI anywhere in the block: {desc}");
        // Flat fields sit directly on the Description.
        assert!(desc.contains("      <pan:mediaPath>image/2026/09/05/20260905-000009-altocnif.png</pan:mediaPath>"));
        // And the whole thing still parses, with mediaPath on the root subject.
        let packet = compose_packet(None, &desc);
        let parsed = parse_packet(&packet).unwrap();
        let root = parsed.iter().find(|p| p.subject.is_none()).expect("root");
        let mp: Vec<String> = root
            .facts
            .iter()
            .filter(|(p, _)| p == &format!("{PAN_NS}mediaPath"))
            .flat_map(|(_, v)| v.iter().map(|t| t.value().to_string()))
            .collect();
        assert_eq!(mp, vec!["image/2026/09/05/20260905-000009-altocnif.png"]);
    }

    #[test]
    fn bracket_of_iri_covers_pan_and_other_kit_iris() {
        assert_eq!(bracket_of_iri("https://repolex.ai/pan/Image/abc"), "<pan/Image/abc>");
        assert_eq!(bracket_of_iri("https://repolex.ai/pan/Enrichment/x1"), "<pan/Enrichment/x1>");
        assert_eq!(bracket_of_iri("https://repolex.ai/copia/Moment/m1"), "<copia/Moment/m1>");
        assert_eq!(bracket_of_iri("urn:other"), "urn:other");
    }

    /// Dev aid, not a check: with `PAN_SAMPLE_PNG=/path/out.png` set, write a
    /// tiny PNG carrying a full new-shape packet so `exiftool` can be pointed
    /// at the real bytes. Does nothing otherwise.
    #[test]
    fn write_sample_png_when_asked() {
        let Ok(out) = std::env::var("PAN_SAMPLE_PNG") else { return };
        let desc = build_pan_description(&ImagePacket {
            iri: "https://repolex.ai/pan/Image/altocnif".into(),
            media_path: "image/2026/09/05/20260905-000009-altocnif.png".into(),
            source_file: String::new(),
            created_date: "2026-09-05T00:00:09-07:00".into(),
            media_type: "image/png".into(),
            width: Some(1280),
            height: Some(1920),
            short_description: Some("A sample caption.".into()),
            long_description: Some("A sample caption, at length.".into()),
            scene_objects: vec!["wolf".into()],
            curation: vec![],
            thumbnail: Some(ThumbRef { id: "th3umb03".into(), path: "image/img/jpg/2026/09/05/20260905-000009-altocnif_512.jpg".into(), width: 341, height: 512 }),
            enrichment: vec![(
                "captionData".into(),
                vec![crate::enrich::EnrichmentRef {
                    id: "jz55pu47".into(),
                    model: "qwen/qwen3.8-27b".into(),
                    path: "caption/2026/09/05/altocnif.xml".into(),
                    count: None,
                    produced_date: "2026-09-05T10:02:40-07:00".into(),
                }],
            )],
            ..Default::default()
        });
        let copia = "<rdf:Description rdf:about=\"\" xmlns:copia=\"https://repolex.ai/ontology/copia/\" xmlns:pan=\"https://repolex.ai/ontology/pan/\" xmlns:git-lex=\"https://repolex.ai/ontology/git-lex/\">\n  <copia:momentId>296pm7ygm6np-1-4</copia:momentId>\n  <copia:seed>4123927538</copia:seed>\n  <git-lex:createdDate>2026-09-04T21:31:34-07:00</git-lex:createdDate>\n  <pan:relatedToId>&lt;copia/Moment/296pm7ygm6np-1-4&gt;</pan:relatedToId>\n</rdf:Description>";
        let arrived = format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n{copia}\n</rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>"
        );
        let packet = compose_packet(Some(&arrived), &desc);
        let png = write_packet_into_png_bytes(&super::tests::make_test_png(8, 8, 1), &packet).unwrap();
        std::fs::write(&out, png).unwrap();
    }
}

