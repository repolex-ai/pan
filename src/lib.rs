//! Pan — a standalone media store that speaks git-lex: stores media, describes
//! it with a graph, searches by graph pattern AND vector similarity.
//!
//! The RDF+vector fusion engine is lifted from Pool (the proven crown jewel:
//! `pool/src/lib.rs` fn search / VectorIndex / add_vector). The accretions —
//! queue, router, allowlist gate, soul routing — are deliberately absent.
//!
//! Design notes carried from the spec:
//! - **Facts live in the DEFAULT graph.** Pool kept Moments in a named graph
//!   and every consumer tripped on it (`?s ?p ?o` read zero). One store = one
//!   media graph; `SELECT ?s ?p ?o` just works. Graph names carrying identity
//!   was the disease; Pan doesn't have graph names at all.
//! - **pan:id** is the identity: an ASSIGNED short random id (8 base32 chars),
//!   minted at put. NOT content-derived — two puts of the same bytes are two
//!   different media objects with different ids. The subject IRI is a standard
//!   full https IRI, `https://repolex.ai/pan/Image/<panId>`, written
//!   once at put and looked up as data thereafter (never re-derived).
//! - **Loud failures.** Unresolvable predicates and broken config are errors,
//!   never silent drops.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use oxigraph::model::{GraphName, Literal, NamedNode, Quad, NamedOrBlankNode};
pub use oxigraph::model::Term;
pub use oxigraph::sparql::{QueryResults, QuerySolution};
use oxigraph::store::Store;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

pub mod config;
pub mod daemon;
pub mod enrich;
pub mod facts;
pub mod import;
pub mod layout;
pub mod npy;
pub mod thumbnail;
pub mod xmp;

pub use config::{PanConfig, PAN_MEDIA_NS, PAN_NS};
pub use facts::Facts;
pub use layout::PanLayout;

/// The angle-bracket form of a pan identity, as it appears everywhere a
/// person or another tool sees it: `<pan/Image/k7m2p9x4>`. Same notation
/// git-lex uses for every other Thing in the system.
pub fn bracket_iri(iri: &str) -> String {
    match iri.strip_prefix("https://repolex.ai/") {
        Some(rest) => format!("<{rest}>"),
        None => iri.to_string(),
    }
}

/// Accept an identity in any of the forms a caller might hand over — the
/// bracket form `<pan/Image/x>`, the full IRI, or the bare id — and return
/// the bare id the store resolves by. Never guesses the class: the bare id
/// is looked up via `pan:id`, which is unique across classes in a store.
pub fn bare_id(given: &str) -> String {
    let s = given.trim();
    let s = s.strip_prefix('<').and_then(|r| r.strip_suffix('>')).unwrap_or(s);
    s.rsplit('/').next().unwrap_or(s).to_string()
}

/// The Pan base ontology, shipped with the binary. Written to `<root>/pan.ttl`
/// reference copy at open; NOT loaded into the media graph (facts stay pure —
/// schema is documentation, not data).
pub const PAN_ONTOLOGY_TTL: &str = include_str!("../ontology/pan.ttl");

/// The panId alphabet: RFC 4648 base32, lowercased. Short, IRI/filename-safe.
const PAN_ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
const PAN_ID_LEN: usize = 8;

/// Generate a candidate panId: 8 random base32 chars (40 bits). panIds are
/// ASSIGNED, not content-derived — identity is the media OBJECT (bytes +
/// mutable description), not the pixels, so a hash would be a collision
/// footgun, not a feature. Collision safety is the caller's mint loop
/// (`Pan::mint_pan_id` retries against the store).
pub(crate) fn gen_pan_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..PAN_ID_LEN)
        .map(|_| PAN_ID_ALPHABET[rng.gen_range(0..PAN_ID_ALPHABET.len())] as char)
        .collect()
}

/// A caller-supplied panId reaches filesystem paths (vector sidecars) — reject
/// anything that isn't a bare token before it touches `Path::join` (the same
/// discipline as [`validate_index_name`]).
pub(crate) fn validate_pan_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        return Err(anyhow!("invalid panId {id:?}"));
    }
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        return Err(anyhow!("invalid panId {id:?}: only [A-Za-z0-9_-] allowed"));
    }
    Ok(())
}

/// The ONTOLOGY CLASS of a media object, from the MIME major type. This is
/// the Capitalized class local-name from ontology/pan.ttl — it names both the
/// instance-path segment (`…/pan/Image/<panId>`) and the `rdf:type` object
/// (`pan:Image`). Only classes the ontology DECLARES are minted: image/* →
/// Image; everything else falls back to the base class Media until its class
/// (Audio, Video, …) is declared. The precise MIME stays in `pan:mediaType`.
pub(crate) fn media_class(media_type: &str) -> &str {
    match media_type.split('/').next() {
        Some("image") => "Image",
        _ => "Media",
    }
}

/// Mint the subject IRI for a NEW media object — the subtexture-wide shape
/// `https://repolex.ai/pan/<Class>/<panId>` (e.g. `…/pan/Image/k7m2p9x4`).
/// No `urn:`, no store identity in the subject (the store is the scope).
/// Minted exactly once, at put; every later lookup resolves the panId to
/// this IRI via the graph.
pub(crate) fn media_subject_iri(media_type: &str, pan_id: &str) -> Result<NamedNode> {
    let class = media_class(media_type);
    NamedNode::new(format!("{PAN_MEDIA_NS}{class}/{pan_id}"))
        .map_err(|e| anyhow!("invalid media subject IRI: {e}"))
}

pub(crate) fn pan_iri(local: &str) -> NamedNode {
    NamedNode::new(format!("{PAN_NS}{local}")).expect("valid pan IRI")
}

/// Validate a vector index name before it ever reaches `Path::join`.
///
/// An index name becomes a single directory component under `hnsw_root` /
/// `vectors_root`. A caller-supplied name that contains a path separator, a
/// `..` component, an absolute-path root, or a NUL would let a write escape the
/// store root (the traversal hole the review caught). We restrict to a safe
/// filename charset — the index-id namespace is ours to define, and every
/// legitimate index name (`qwen-vl-2b-2048`, `clip-768`) already fits.
fn validate_index_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("index name must not be empty"));
    }
    if name.len() > 128 {
        return Err(anyhow!("index name too long (max 128)"));
    }
    if name == "." || name == ".." {
        return Err(anyhow!("invalid index name: {name:?}"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(anyhow!(
            "invalid index name {name:?}: only [A-Za-z0-9._-] allowed (no path separators)"
        ));
    }
    Ok(())
}

fn term_str(t: &Term) -> String {
    match t {
        Term::Literal(l) => l.value().to_string(),
        Term::NamedNode(n) => n.as_str().to_string(),
        Term::BlankNode(b) => b.as_str().to_string(),
        _ => format!("{t}"),
    }
}

/// One vector index: usearch HNSW + the panId↔key bijection sidecar
/// (`keymap.json`). Lifted from Pool. The index for a name lives at
/// `<hnsw_root>/<name>/index.usearch`; dim is fixed by the first insert.
struct VectorIndex {
    dim: usize,
    index: Index,
    id_to_key: HashMap<String, u64>,
    key_to_id: HashMap<u64, String>,
    next_key: u64,
    path: PathBuf,
    dirty: bool,
}

impl VectorIndex {
    /// Open or create the named index. `dim` is a HINT — for a brand-new index
    /// it fixes the dimensionality; for an EXISTING index on disk the on-disk
    /// index's true dim WINS (usearch stores it in the file). This is the fix
    /// for the dim-poisoning hole: a caller who lazy-loads with a wrong length
    /// no longer overwrites the index's real dim, so a later correct query is
    /// not spuriously rejected and the keymap is never corrupted.
    fn create(hnsw_root: &Path, name: &str, dim: usize) -> Result<Self> {
        validate_index_name(name)?;
        let dir = hnsw_root.join(name);
        fs::create_dir_all(&dir).context("create hnsw index dir")?;
        let path = dir.join("index.usearch");

        let opts = IndexOptions {
            dimensions: dim,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            connectivity: 16,
            expansion_add: 128,
            expansion_search: 64,
            multi: false,
        };
        let index = Index::new(&opts)?;
        index.reserve(1024)?;

        let mut id_to_key = HashMap::new();
        let mut key_to_id = HashMap::new();
        let mut next_key = 0u64;
        // The authoritative dim: the caller's hint until an on-disk index
        // overrides it with its real, persisted dimensionality.
        let mut true_dim = dim;

        if path.exists() {
            index.load(path.to_str().unwrap())?;
            // The loaded index carries its own dimensionality; trust it over
            // the caller's hint (which may be a stray query length).
            let loaded = index.dimensions();
            if loaded != 0 {
                true_dim = loaded;
            }
            let map_path = dir.join("keymap.json");
            if map_path.exists() {
                let raw = fs::read_to_string(&map_path)?;
                let m: HashMap<String, u64> = serde_json::from_str(&raw)?;
                next_key = m.values().copied().max().map(|m| m + 1).unwrap_or(0);
                for (id, key) in &m {
                    key_to_id.insert(*key, id.clone());
                }
                id_to_key = m;
            }
        }

        Ok(Self {
            dim: true_dim,
            index,
            id_to_key,
            key_to_id,
            next_key,
            path,
            dirty: false,
        })
    }

    fn save(&self) -> Result<()> {
        self.index.save(self.path.to_str().unwrap())?;
        let map_path = self.path.parent().unwrap().join("keymap.json");
        fs::write(&map_path, serde_json::to_string(&self.id_to_key)?)?;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub id: String,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct IndexStats {
    pub dim: usize,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PutResult {
    /// The assigned identity, bare — new on EVERY put (never content-derived).
    pub id: String,
    /// The full subject IRI written for this object.
    pub iri: String,
    pub media_path: String,
    pub created_at: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// True when a thumbnail was generated and declared. False = the bytes
    /// could not be decoded as an image (still stored; `pan state` says so).
    pub thumbnail: bool,
}

/// What exists for one media object, read from the graph alone — the
/// substance of `pan state`. Each entry is (stage, models present).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaState {
    pub id: String,
    pub iri: String,
    pub media_type: String,
    pub created_at: String,
    pub thumbnail: bool,
    /// enrichment link (embedding / captionItem / region / pose) → models
    /// that have a record on this object.
    pub enrichment: Vec<(String, Vec<String>)>,
}

/// One item of stage work: an image lacking a given model's output.
#[derive(Debug, Clone)]
pub struct PendingItem {
    pub id: String,
    pub iri: String,
    pub media_path: String,
    pub media_type: String,
}

/// One open Pan store.
pub struct Pan {
    pub cfg: PanConfig,
    pub layout: PanLayout,
    store: Store,
    indexes: Mutex<HashMap<String, VectorIndex>>,
}

impl Pan {
    /// Open (or initialize) the store at `root`. Creates the layout dirs, opens
    /// the graph store, writes the reference ontology copy. Config is read from
    /// `<root>/pan.yml` (optional — all defaults when absent).
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("create store root {}", root.display()))?;
        let cfg = PanConfig::load(root)?;
        let layout = PanLayout::resolve(root, cfg.storage_root_override.as_deref());
        fs::create_dir_all(&layout.oxigraph_root).context("create oxigraph root")?;
        fs::create_dir_all(&layout.hnsw_root).context("create hnsw root")?;
        fs::create_dir_all(&layout.blob_root).context("create blob root")?;
        fs::create_dir_all(&layout.vectors_root).context("create vectors root")?;

        // Reference copy of the shipped base ontology (documentation at rest;
        // NOT loaded into the media graph).
        let ttl_path = root.join("pan.ttl");
        if !ttl_path.exists() {
            fs::write(&ttl_path, PAN_ONTOLOGY_TTL).context("write pan.ttl reference copy")?;
        }

        let store = Store::open(&layout.oxigraph_root)
            .with_context(|| format!("open oxigraph at {}", layout.oxigraph_root.display()))?;

        Ok(Pan {
            cfg,
            layout,
            store,
            indexes: Mutex::new(HashMap::new()),
        })
    }

    // ── CRUD ────────────────────────────────────────────────────────────────

    /// Mint a fresh panId, retrying on the (astronomically rare) collision
    /// with an id already in the store.
    fn mint_pan_id(&self) -> Result<String> {
        loop {
            let cand = gen_pan_id();
            if self.subject_for(&cand)?.is_none() {
                return Ok(cand);
            }
        }
    }

    /// Resolve a panId to its subject IRI. The IRI is DATA, written once at
    /// put and looked up here via `pan:id` — never re-derived, so the mint
    /// scheme can evolve without breaking lookups of existing objects.
    pub fn subject_for(&self, pan_id: &str) -> Result<Option<NamedNode>> {
        let obj = Literal::new_simple_literal(pan_id);
        for quad in self.store.quads_for_pattern(
            None,
            Some(pan_iri("id").as_ref()),
            Some(obj.as_ref().into()),
            Some(GraphName::DefaultGraph.as_ref()),
        ) {
            let quad = quad.context("resolve panId")?;
            if let NamedOrBlankNode::NamedNode(n) = quad.subject {
                return Ok(Some(n));
            }
        }
        Ok(None)
    }

    /// Store media bytes as a NEW media object. Every put mints a fresh
    /// assigned panId — putting the same bytes twice creates two different
    /// objects (identity is the object, never the pixels; there is no dedup).
    ///
    /// For PNGs: any existing XMP app facts are ingested into the graph (real
    /// RDF parser; sub-subjects are REBASED onto the new subject), then Pan
    /// re-authors + stamps its own packet (pan: identity block + the graph's
    /// app facts) — pixels unchanged.
    ///
    /// `facts` are caller-supplied descriptions (loud on unresolvable
    /// predicates).
    pub fn put(&self, bytes: &[u8], content_type: Option<&str>, facts: Facts) -> Result<PutResult> {
        let png = xmp::is_png(bytes);
        let media_type = content_type.map(|s| s.to_string()).unwrap_or_else(|| {
            if png { "image/png".to_string() } else { "application/octet-stream".to_string() }
        });
        let ext = match media_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "bin",
        };

        let pan_id = self.mint_pan_id()?;
        let subject = media_subject_iri(&media_type, &pan_id)?;
        let created_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // media/image/YYYY/MM/DD/<panId>.<ext> — date shard from createdAt,
        // filename from the assigned id.
        let shard = created_at.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
        let rel_path = format!("{}/{shard}/{pan_id}.{ext}", PanLayout::MEDIA_SUBPATH);
        let abs_path = self.layout.storage_root.join(&rel_path);

        // Identity facts — Pan's own block. The instance is TYPED against the
        // kit ontology class (`a pan:Image`) — same class that names the path.
        let rdf_type = NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#type").expect("rdf:type IRI");
        let mut quads = vec![
            Quad::new(
                subject.clone(),
                rdf_type.clone(),
                pan_iri(media_class(&media_type)),
                GraphName::DefaultGraph,
            ),
            self.quad(&subject, "id", &pan_id),
            self.quad(&subject, "mediaPath", &rel_path),
            self.quad(&subject, "createdAt", &created_at),
            self.quad(&subject, "mediaType", &media_type),
        ];

        // Ingest existing XMP app facts (the walker-lite: media traveling in
        // carries its descriptions with it). pan: identity fields are skipped —
        // Pan re-authors those; a foreign store's blobPath/panId are not facts
        // about THIS object. Sub-subjects (regions etc.) were scoped under the
        // SOURCE object's subject; the source packet's own pan:id tells us
        // where that scope starts, so they REBASE onto the new subject and stay
        // query/delete-reachable here. A malformed foreign packet must NOT fail
        // the store — media-in is the job; it is logged and the bytes still land.
        if png {
            match xmp::read_xmp_packet_from_bytes(bytes) {
                Ok(Some(packet)) => match xmp::parse_packet(&packet) {
                    Ok(blocks) => {
                        let source_pan_id: Option<String> = blocks
                            .iter()
                            .find(|b| b.subject.is_none())
                            .and_then(|b| b.facts.iter().find(|(p, _)| p == &format!("{PAN_NS}id")))
                            .and_then(|(_, v)| v.first().map(|t| t.value().to_string()));
                        for block in &blocks {
                            let subj = match &block.subject {
                                None => subject.clone(),
                                Some(iri) => {
                                    // Rebase `<source-subject>/<tail>` → `<subject>/<tail>`.
                                    // No source panId, or an IRI outside its scope
                                    // = unknowable foreign subject; drop rather
                                    // than orphan.
                                    let Some(src) = &source_pan_id else { continue };
                                    let marker = format!("/{src}/");
                                    let Some(pos) = iri.find(&marker) else { continue };
                                    let tail = &iri[pos + marker.len()..];
                                    match NamedNode::new(format!("{}/{tail}", subject.as_str())) {
                                        Ok(n) => n,
                                        Err(_) => continue,
                                    }
                                }
                            };
                            for (pred, values) in &block.facts {
                                if pred.starts_with(PAN_NS) {
                                    continue;
                                }
                                let p = match NamedNode::new(pred.as_str()) {
                                    Ok(p) => p,
                                    Err(_) => continue, // skip an unusable predicate IRI, don't fail
                                };
                                for v in values {
                                    // Preserve IRI-ness: an IRI object (e.g. an
                                    // rdf:type) stays an IRI, so type queries work
                                    // on the traveled fact.
                                    let obj: oxigraph::model::Term = if v.is_iri() {
                                        match NamedNode::new(v.value()) {
                                            Ok(n) => n.into(),
                                            Err(_) => Literal::new_simple_literal(v.value()).into(),
                                        }
                                    } else {
                                        Literal::new_simple_literal(v.value()).into()
                                    };
                                    quads.push(Quad::new(subj.clone(), p.clone(), obj, GraphName::DefaultGraph));
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!(pan_id = %pan_id, "skipping unparseable travel XMP: {e:#}"),
                },
                Ok(None) => {}
                Err(e) => tracing::warn!(pan_id = %pan_id, "skipping unreadable travel XMP: {e:#}"),
            }
        }

        // Caller facts — loud resolution, nothing written on error.
        quads.extend(facts.into_quads(&subject, &self.cfg.prefixes, &self.cfg.default_prefix)?);

        // Thumbnail — a rendition Pan makes itself, declared as its own node.
        // Not decodable as an image = no thumbnail, still stored; the state
        // view says so rather than the put failing.
        let mut thumb_rel: Option<String> = None;
        let mut thumb_jpeg: Vec<u8> = Vec::new();
        let mut width = None;
        let mut height = None;
        let mut thumb_quads: Vec<Quad> = Vec::new();
        if media_type.starts_with("image/") {
            match thumbnail::make(bytes) {
                Ok(t) => {
                    width = Some(t.source_width);
                    height = Some(t.source_height);
                    quads.push(self.quad(&subject, "width", &t.source_width.to_string()));
                    quads.push(self.quad(&subject, "height", &t.source_height.to_string()));
                    let rel = format!("thumbnail/{shard}/{pan_id}.jpg");
                    let tid = gen_pan_id();
                    let tnode = NamedNode::new(format!("{PAN_MEDIA_NS}Thumbnail/{tid}"))
                        .map_err(|e| anyhow!("thumbnail IRI: {e}"))?;
                    thumb_quads.push(Quad::new(subject.clone(), pan_iri("thumbnail"), tnode.clone(), GraphName::DefaultGraph));
                    thumb_quads.push(Quad::new(tnode.clone(), rdf_type.clone(), pan_iri("Thumbnail"), GraphName::DefaultGraph));
                    thumb_quads.push(self.quad(&tnode, "id", &tid));
                    thumb_quads.push(self.quad(&tnode, "path", &rel));
                    thumb_quads.push(self.quad(&tnode, "width", &t.width.to_string()));
                    thumb_quads.push(self.quad(&tnode, "height", &t.height.to_string()));
                    thumb_rel = Some(rel);
                    thumb_jpeg = t.jpeg;
                }
                Err(e) => tracing::warn!(id = %pan_id, "no thumbnail: {e:#}"),
            }
        }
        quads.extend(thumb_quads);

        // THE ORDER (Rob, 2026-09-03): bytes on disk → XMP → thumbnail → graph.
        // The packet is built from the same quads the graph is about to receive
        // (a scratch store, so packet and graph cannot disagree), then the
        // graph commit is ONE transaction — an image exists in the system only
        // once that commit lands. A failure before it leaves no half-object:
        // the files written so far are removed.
        if let Some(parent) = abs_path.parent() {
            fs::create_dir_all(parent).context("create media shard dir")?;
        }
        let land = || -> Result<()> {
            if png {
                let scratch = Store::new().context("scratch store")?;
                for q in &quads {
                    scratch.insert(q.as_ref()).context("scratch insert")?;
                }
                let packet = self.build_packet_from(&scratch, &pan_id, &rel_path, &created_at)?;
                let stamped = xmp::write_packet_into_png_bytes(bytes, &packet)?;
                fs::write(&abs_path, &stamped).with_context(|| format!("write media {}", abs_path.display()))?;
            } else {
                fs::write(&abs_path, bytes).with_context(|| format!("write media {}", abs_path.display()))?;
            }
            if let Some(rel) = &thumb_rel {
                let tabs = self.layout.abs(rel);
                if let Some(parent) = tabs.parent() {
                    fs::create_dir_all(parent).context("create thumbnail shard dir")?;
                }
                fs::write(&tabs, &thumb_jpeg).with_context(|| format!("write thumbnail {}", tabs.display()))?;
            }
            self.insert_quads(&quads)?;
            Ok(())
        };
        if let Err(e) = land() {
            let _ = fs::remove_file(&abs_path);
            if let Some(rel) = &thumb_rel {
                let _ = fs::remove_file(self.layout.abs(rel));
            }
            return Err(e);
        }

        Ok(PutResult {
            id: pan_id,
            iri: subject.into_string(),
            media_path: rel_path,
            created_at,
            width,
            height,
            thumbnail: thumb_rel.is_some(),
        })
    }

    /// Insert quads as ONE transaction: all land or none do. Every graph write
    /// in Pan goes through here so a crash mid-write can never leave a
    /// half-described object.
    pub fn insert_quads(&self, quads: &[Quad]) -> Result<()> {
        let mut t = self.store.start_transaction().context("start transaction")?;
        for q in quads {
            t.insert(q.as_ref());
        }
        t.commit().context("commit transaction")?;
        Ok(())
    }

    /// One `pan:` field of an arbitrary node (an enrichment/thumbnail node
    /// the image links to), by the node's full IRI.
    pub fn node_field(&self, node_iri: &str, local: &str) -> Result<Option<String>> {
        let node = NamedNode::new(node_iri).map_err(|e| anyhow!("invalid node IRI {node_iri}: {e}"))?;
        for q in self.store.quads_for_pattern(
            Some((&node).into()),
            Some(pan_iri(local).as_ref()),
            None,
            Some(GraphName::DefaultGraph.as_ref()),
        ) {
            let q = q.context("read node field")?;
            return Ok(Some(term_str(&q.object)));
        }
        Ok(None)
    }

    /// What exists for one object, from the graph alone.
    pub fn state_for(&self, id: &str) -> Result<Option<MediaState>> {
        let Some(subject) = self.subject_for(id)? else { return Ok(None) };
        let facts = self.facts_for(id)?;
        let one = |local: &str| -> String {
            facts
                .iter()
                .find(|(p, _)| p == &format!("{PAN_NS}{local}"))
                .and_then(|(_, v)| v.first().cloned())
                .unwrap_or_default()
        };
        let mut enrichment = Vec::new();
        for link in ["embedding", "captionItem", "region", "pose"] {
            let mut models: Vec<String> = Vec::new();
            for (pred, values) in &facts {
                if pred != &format!("{PAN_NS}{link}") {
                    continue;
                }
                for node_iri in values {
                    let Ok(node) = NamedNode::new(node_iri.as_str()) else { continue };
                    for q in self.store.quads_for_pattern(
                        Some((&node).into()),
                        Some(pan_iri("model").as_ref()),
                        None,
                        Some(GraphName::DefaultGraph.as_ref()),
                    ) {
                        let q = q.context("read model")?;
                        let m = term_str(&q.object);
                        if !models.contains(&m) {
                            models.push(m);
                        }
                    }
                }
            }
            models.sort();
            enrichment.push((link.to_string(), models));
        }
        Ok(Some(MediaState {
            id: id.to_string(),
            iri: subject.into_string(),
            media_type: one("mediaType"),
            created_at: one("createdAt"),
            thumbnail: facts.iter().any(|(p, _)| p == &format!("{PAN_NS}thumbnail")),
            enrichment,
        }))
    }

    /// Images that have NO record from `model` under `link_local` (embedding /
    /// captionItem / region / pose) — the stage engine's work list. The graph
    /// is the queue: pending means absent, nothing else is tracked.
    pub fn pending_for(&self, link_local: &str, model: &str, limit: usize) -> Result<Vec<PendingItem>> {
        let model_lit = model.replace('\\', "\\\\").replace('"', "\\\"");
        let q = format!(
            "SELECT ?s ?id ?path ?type WHERE {{
               ?s a pan:Image ; pan:id ?id ; pan:mediaPath ?path ; pan:mediaType ?type .
               FILTER NOT EXISTS {{ ?s pan:{link_local} ?e . ?e pan:model \"{model_lit}\" }}
             }} ORDER BY ?id LIMIT {limit}"
        );
        let mut out = Vec::new();
        if let QueryResults::Solutions(sols) = self.query(&q)? {
            for s in sols {
                let s = s?;
                let get = |v: &str| s.get(v).map(term_str).unwrap_or_default();
                out.push(PendingItem {
                    id: get("id"),
                    iri: get("s"),
                    media_path: get("path"),
                    media_type: get("type"),
                });
            }
        }
        Ok(out)
    }

    /// Record one model's output for an image as a data file beside the media
    /// plus the graph statements that describe it, then refresh the image's
    /// XMP so its own packet lists the new file. `kind` is the data-file
    /// directory (caption / sam3 / pose); `link_local` the membership
    /// predicate; `ref_local` the reference predicate.
    pub fn write_enrichment(
        &self,
        id: &str,
        kind: &str,
        link_local: &str,
        ref_local: &str,
        model: &str,
        records: &[enrich::EnrichmentRecord],
        variant: Option<&str>,
    ) -> Result<String> {
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        let created_at = self
            .facts_for(id)?
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}createdAt"))
            .and_then(|(_, v)| v.first().cloned())
            .unwrap_or_default();
        let shard = created_at.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
        let rel = PanLayout::enrichment_rel_path(kind, &shard, id, variant);
        let abs = self.layout.abs(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).context("create enrichment dir")?;
        }
        fs::write(&abs, enrich::build_data_file(subject.as_str(), link_local, records))
            .with_context(|| format!("write {}", abs.display()))?;

        let mut quads = enrich::record_quads(subject.as_str(), link_local, records)?;
        let r = enrich::EnrichmentRef { id: gen_pan_id(), model: model.to_string(), path: rel.clone(), count: records.len() };
        quads.extend(enrich::ref_quads(subject.as_str(), ref_local, &r)?);
        if let Err(e) = self.insert_quads(&quads) {
            let _ = fs::remove_file(&abs);
            return Err(e);
        }
        self.restamp(id)?;
        Ok(rel)
    }

    /// Record an embedding: vector into the index + `.npy` sidecar, an
    /// Embedding node (model, dim, vectorPath) and a vectorData reference on
    /// the image, XMP refreshed.
    pub fn write_embedding(&self, id: &str, model: &str, index_name: &str, vec: &[f32]) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        self.add_vector(id, index_name, vec)?;
        self.flush()?;
        let rel = self
            .layout
            .vector_sidecar_path(index_name, id)
            .strip_prefix(&self.layout.storage_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let rec = enrich::EnrichmentRecord::new(gen_pan_id(), "Embedding", model)
            .field("dim", vec.len().to_string())
            .field("vectorPath", &rel);
        let mut quads = enrich::record_quads(subject.as_str(), "embedding", std::slice::from_ref(&rec))?;
        let r = enrich::EnrichmentRef { id: gen_pan_id(), model: model.to_string(), path: rel, count: 1 };
        quads.extend(enrich::ref_quads(subject.as_str(), "vectorData", &r)?);
        self.insert_quads(&quads)?;
        self.restamp(id)?;
        Ok(())
    }

    /// Set the image's current caption text (the one field a plain viewer
    /// shows). Replaces any previous value; XMP refreshed.
    pub fn set_caption(&self, id: &str, text: &str) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        let mut t = self.store.start_transaction().context("start transaction")?;
        let old: Vec<Quad> = self
            .store
            .quads_for_pattern(Some((&subject).into()), Some(pan_iri("caption").as_ref()), None, Some(GraphName::DefaultGraph.as_ref()))
            .collect::<std::result::Result<_, _>>()
            .context("read caption")?;
        for q in &old {
            t.remove(q.as_ref());
        }
        t.insert(self.quad(&subject, "caption", text).as_ref());
        t.commit().context("commit caption")?;
        self.restamp(id)
    }

    /// Mint a fresh, unused id. Public so an importer can assign identity
    /// before it has anything to store.
    pub fn new_id(&self) -> Result<String> {
        self.mint_pan_id()
    }

    /// Whether any media in this store was imported from `source` — the
    /// re-run guard, so an interrupted migration resumes instead of
    /// duplicating (identity is assigned per put, so a second import of the
    /// same file would otherwise silently create a second object).
    pub fn imported_from(&self, source: &str) -> Result<bool> {
        let obj = Literal::new_simple_literal(source);
        Ok(self
            .store
            .quads_for_pattern(
                None,
                Some(pan_iri("importedFrom").as_ref()),
                Some(obj.as_ref().into()),
                Some(GraphName::DefaultGraph.as_ref()),
            )
            .next()
            .is_some())
    }

    /// Assert one triple on an arbitrary subject IRI. `object_is_iri` picks
    /// whether the object is a NamedNode (an IRI, e.g. an `rdf:type`) or a
    /// plain literal. Used to describe sub-subjects (regions etc.) that hang
    /// off a media object's subject. Does NOT re-stamp — call `restamp(panId)`
    /// when the sub-subject belongs to a PNG and you want the travel copy
    /// refreshed.
    pub fn describe_subject(&self, subject_iri: &str, predicate_iri: &str, object: &str, object_is_iri: bool) -> Result<()> {
        let s = NamedNode::new(subject_iri).map_err(|e| anyhow!("invalid subject IRI {subject_iri}: {e}"))?;
        let p = NamedNode::new(predicate_iri).map_err(|e| anyhow!("invalid predicate IRI {predicate_iri}: {e}"))?;
        let o: oxigraph::model::Term = if object_is_iri {
            NamedNode::new(object).map_err(|e| anyhow!("invalid object IRI {object}: {e}"))?.into()
        } else {
            Literal::new_simple_literal(object).into()
        };
        self.store
            .insert(Quad::new(s, p, o, GraphName::DefaultGraph).as_ref())
            .context("insert triple")?;
        Ok(())
    }

    /// Read media bytes + facts by panId.
    pub fn get(&self, pan_id: &str) -> Result<(Vec<u8>, Vec<(String, Vec<String>)>)> {
        let facts = self.facts_for(pan_id)?;
        let media_path = facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}mediaPath"))
            .and_then(|(_, v)| v.first().cloned())
            .ok_or_else(|| anyhow!("panId not found: {pan_id}"))?;
        let abs = self.layout.storage_root.join(&media_path);
        let bytes = fs::read(&abs).with_context(|| format!("read media {}", abs.display()))?;
        Ok((bytes, facts))
    }

    /// All facts for a panId's subject: full-IRI predicate → values. Empty vec
    /// = unknown panId.
    pub fn facts_for(&self, pan_id: &str) -> Result<Vec<(String, Vec<String>)>> {
        let Some(subject) = self.subject_for(pan_id)? else {
            return Ok(vec![]);
        };
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for quad in self
            .store
            .quads_for_pattern(Some((&subject).into()), None, None, Some(GraphName::DefaultGraph.as_ref()))
        {
            let quad = quad.context("read facts")?;
            map.entry(quad.predicate.as_str().to_string())
                .or_default()
                .push(term_str(&quad.object));
        }
        let mut out: Vec<_> = map.into_iter().collect();
        out.sort();
        Ok(out)
    }

    /// Merge additional facts onto an existing panId (LOUD on unresolvable
    /// predicates, 404-style error on unknown panId). PNGs are re-stamped so
    /// the XMP mirror follows the graph.
    pub fn describe(&self, pan_id: &str, facts: Facts) -> Result<()> {
        let Some(subject) = self.subject_for(pan_id)? else {
            return Err(anyhow!("panId not found: {pan_id}"));
        };
        let quads = facts.into_quads(&subject, &self.cfg.prefixes, &self.cfg.default_prefix)?;
        for q in &quads {
            self.store.insert(q.as_ref()).context("insert quad")?;
        }
        self.restamp(pan_id)?;
        Ok(())
    }

    /// Delete a panId: triples (subject + its sub-subjects), media file, vector
    /// sidecars, and index entries.
    pub fn delete(&self, pan_id: &str) -> Result<()> {
        let Some(subject) = self.subject_for(pan_id)? else {
            return Err(anyhow!("panId not found: {pan_id}"));
        };
        let facts = self.facts_for(pan_id)?;

        // Media file first (facts still know where it is).
        if let Some(media_path) = facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}mediaPath"))
            .and_then(|(_, v)| v.first())
        {
            let abs = self.layout.storage_root.join(media_path);
            if abs.exists() {
                fs::remove_file(&abs).with_context(|| format!("remove media {}", abs.display()))?;
            }
        }

        // Subject triples + sub-subject triples (subjects under `<subject>/…`).
        let mut to_remove: Vec<Quad> = Vec::new();
        for quad in self.store.iter() {
            let quad = quad.context("scan for delete")?;
            let subj_iri = match &quad.subject {
                NamedOrBlankNode::NamedNode(n) => n.as_str(),
                _ => continue,
            };
            if subj_iri == subject.as_str() || subj_iri.starts_with(&format!("{}/", subject.as_str())) {
                to_remove.push(quad);
            }
        }
        for q in &to_remove {
            self.store.remove(q.as_ref()).context("remove quad")?;
        }

        // Vector index entries + sidecars, across all on-disk indexes.
        validate_pan_id(pan_id)?;
        let index_names: Vec<String> = fs::read_dir(&self.layout.hnsw_root)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().join("index.usearch").exists())
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut indexes = self.indexes.lock().unwrap();
        for name in index_names {
            if !indexes.contains_key(&name) {
                // dim unknown until load; probe via a throwaway load with dim from
                // the keymap-less path is impossible — usearch stores dim in the
                // file, but the wrapper needs one. Load lazily only if the keymap
                // knows this panId, using the sidecar's dim.
                let keymap_path = self.layout.hnsw_root.join(&name).join("keymap.json");
                let known = fs::read_to_string(&keymap_path)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<HashMap<String, u64>>(&raw).ok())
                    .map(|m| m.contains_key(pan_id))
                    .unwrap_or(false);
                if !known {
                    continue;
                }
                let sidecar = self.layout.vector_sidecar_path(&name, pan_id);
                let dim = npy::read_f32_1d(&sidecar).map(|v| v.len()).unwrap_or(0);
                if dim == 0 {
                    continue; // can't load without a dim; index entry becomes stale but harmless
                }
                indexes.insert(name.clone(), VectorIndex::create(&self.layout.hnsw_root, &name, dim)?);
            }
            if let Some(vi) = indexes.get_mut(&name) {
                if let Some(key) = vi.id_to_key.remove(pan_id) {
                    vi.key_to_id.remove(&key);
                    vi.index.remove(key).ok();
                    vi.dirty = true;
                }
            }
            let sidecar = self.layout.vector_sidecar_path(&name, pan_id);
            if sidecar.exists() {
                fs::remove_file(&sidecar).ok();
            }
        }
        Ok(())
    }

    // ── Vectors + search (the crown jewel, lifted from Pool) ────────────────

    /// Attach a vector to an existing panId: writes the raw `.npy` sidecar
    /// (media-derived, follows storage) and adds to the named HNSW index.
    ///
    /// **Idempotent**: if `pan_id` is already indexed in `index_name`, no-op
    /// and `Ok(false)`. `Ok(true)` when a new entry was added.
    pub fn add_vector(&self, pan_id: &str, index_name: &str, vec: &[f32]) -> Result<bool> {
        validate_pan_id(pan_id)?;
        let mut indexes = self.indexes.lock().unwrap();
        if !indexes.contains_key(index_name) {
            let vi = VectorIndex::create(&self.layout.hnsw_root, index_name, vec.len())?;
            indexes.insert(index_name.to_string(), vi);
        }
        let vi = indexes.get_mut(index_name).unwrap();

        if vec.len() != vi.dim {
            return Err(anyhow!(
                "vector dim {} does not match index {} dim {}",
                vec.len(),
                index_name,
                vi.dim
            ));
        }

        // Idempotency gate: usearch throws on duplicate-key insert; same panId
        // = same vector, no work to do.
        if vi.id_to_key.contains_key(pan_id) {
            return Ok(false);
        }

        // Raw sidecar first (reembed/migration source of truth for the vector).
        npy::write_f32_1d(&self.layout.vector_sidecar_path(index_name, pan_id), vec)?;

        let key = vi.next_key;
        vi.next_key += 1;
        vi.id_to_key.insert(pan_id.to_string(), key);
        vi.key_to_id.insert(key, pan_id.to_string());

        let capacity_needed = vi.id_to_key.len();
        if vi.index.capacity() < capacity_needed {
            vi.index.reserve(capacity_needed.max(1024))?;
        }
        vi.index
            .add(key, vec)
            .map_err(|e| anyhow!("usearch add (panId {}, index {}): {}", pan_id, index_name, e))?;
        vi.dirty = true;
        Ok(true)
    }

    /// Whether `pan_id` is already present in `index_name`.
    pub fn contains_id(&self, pan_id: &str, index_name: &str) -> bool {
        let indexes = self.indexes.lock().unwrap();
        indexes
            .get(index_name)
            .map(|vi| vi.id_to_key.contains_key(pan_id))
            .unwrap_or(false)
    }

    /// `(dim, count)` for every index visible on disk or in memory.
    pub fn index_stats(&self) -> Vec<(String, IndexStats)> {
        let indexes = self.indexes.lock().unwrap();
        let mut out: Vec<(String, IndexStats)> = indexes
            .iter()
            .map(|(name, vi)| {
                (
                    name.clone(),
                    IndexStats {
                        dim: vi.dim,
                        count: vi.id_to_key.len(),
                    },
                )
            })
            .collect();
        // On-disk indexes not yet loaded: report count from keymap, dim unknown (0).
        if let Ok(rd) = fs::read_dir(&self.layout.hnsw_root) {
            for e in rd.filter_map(|e| e.ok()) {
                let name = match e.file_name().to_str() {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                if indexes.contains_key(&name) || !e.path().join("index.usearch").exists() {
                    continue;
                }
                let count = fs::read_to_string(e.path().join("keymap.json"))
                    .ok()
                    .and_then(|raw| serde_json::from_str::<HashMap<String, u64>>(&raw).ok())
                    .map(|m| m.len())
                    .unwrap_or(0);
                out.push((name, IndexStats { dim: 0, count }));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Hybrid query — THE reason Pan exists. SPARQL `where` (must constrain
    /// `?s`, which binds `?id` via `pan:id`) gates the candidate set;
    /// usearch kNN ranks by cosine similarity to `like`. Strategy A: pre-filter
    /// then search, joined at the application layer by the panId↔key map — no
    /// custom SPARQL UDF. Lifted from Pool with one simplification: facts live
    /// in the default graph, so there is no GRAPH wrapper to splice into.
    ///
    /// An empty `where_clause` means "no graph gate" — pure kNN over the index.
    pub fn search(&self, where_clause: &str, like: &[f32], k: usize, index_name: &str) -> Result<Vec<SearchHit>> {
        validate_index_name(index_name)?;
        // Same prologue as query() so a gate can use rdf:/rdfs:/owl:/xsd: and
        // every configured app prefix — no surprise "unknown prefix" between
        // the two SPARQL paths.
        let q = format!(
            "{}
             SELECT DISTINCT ?id WHERE {{
               ?s pan:id ?id .
               {where_clause}
             }}",
            self.prefix_prologue()
        );
        let mut candidate_ids: HashSet<String> = HashSet::new();
        if let QueryResults::Solutions(sols) = self.store.query(&q).map_err(|e| anyhow!("search where-clause: {e}"))? {
            for s in sols {
                let s = s?;
                if let Some(t) = s.get("id") {
                    candidate_ids.insert(term_str(t));
                }
            }
        }

        if candidate_ids.is_empty() {
            return Ok(vec![]);
        }

        let mut indexes = self.indexes.lock().unwrap();
        // Lazy-load from disk: search may be the first touch after open.
        if !indexes.contains_key(index_name) {
            let index_path = self.layout.hnsw_root.join(index_name).join("index.usearch");
            if index_path.exists() {
                // dim hint 0: this index exists on disk, so create() reads its
                // real dim from the file — never from the query length.
                let vi = VectorIndex::create(&self.layout.hnsw_root, index_name, 0)?;
                indexes.insert(index_name.to_string(), vi);
            }
        }
        let vi = indexes
            .get_mut(index_name)
            .ok_or_else(|| anyhow!("no such index: {index_name} (no vectors attached yet?)"))?;

        if vi.dim != like.len() {
            return Err(anyhow!(
                "query embedding dim {} does not match index {} dim {}",
                like.len(),
                index_name,
                vi.dim
            ));
        }

        let candidate_keys: HashSet<u64> = candidate_ids
            .iter()
            .filter_map(|c| vi.id_to_key.get(c).copied())
            .collect();

        if candidate_keys.is_empty() {
            return Ok(vec![]);
        }

        // Adaptive search breadth from prefilter selectivity (ported verbatim —
        // non-obvious quality tuning).
        let total = vi.id_to_key.len() as f32;
        let selectivity = (candidate_keys.len() as f32 / total).max(0.001);
        let ef = ((k as f32 / selectivity).clamp(64.0, 4096.0)) as usize;
        vi.index.change_expansion_search(ef);

        let matches = vi.index.filtered_search(like, k, |key| candidate_keys.contains(&key))?;

        let mut hits = Vec::with_capacity(matches.keys.len());
        for (key, distance) in matches.keys.iter().zip(matches.distances.iter()) {
            if let Some(id) = vi.key_to_id.get(key) {
                hits.push(SearchHit {
                    id: id.clone(),
                    score: 1.0 - *distance,
                });
            }
        }
        Ok(hits)
    }

    // ── SPARQL ──────────────────────────────────────────────────────────────

    /// Run a SPARQL query with the store's prefixes pre-declared (every query
    /// path goes through the same prologue — the git-lex `add_prefixes`
    /// discipline). Standard W3C prefixes (rdf/rdfs/owl/xsd) are included.
    pub fn query(&self, sparql: &str) -> Result<QueryResults> {
        let prologue = self.prefix_prologue();
        self.store
            .query(&format!("{prologue}{sparql}"))
            .map_err(|e| anyhow!("SPARQL error: {e}"))
    }

    fn prefix_prologue(&self) -> String {
        let mut p = String::from(
            "PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>\n\
             PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>\n\
             PREFIX owl: <http://www.w3.org/2002/07/owl#>\n\
             PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n",
        );
        for (short, ns) in &self.cfg.prefixes {
            p.push_str(&format!("PREFIX {short}: <{ns}>\n"));
        }
        p
    }

    // ── XMP restamp (graph → packet mirror) ─────────────────────────────────

    /// Rebuild + restamp a PNG's XMP from the CURRENT graph state. The one
    /// mirror rule: the packet carries the pan: identity block plus every
    /// graph fact expressible in a configured prefix (unmapped namespaces stay
    /// graph-only — the graph is the full truth, XMP is the travel copy).
    pub fn restamp(&self, pan_id: &str) -> Result<()> {
        let facts = self.facts_for(pan_id)?;
        let media_path = match facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}mediaPath"))
            .and_then(|(_, v)| v.first())
        {
            Some(p) => p.clone(),
            None => return Err(anyhow!("panId not found: {pan_id}")),
        };
        let abs = self.layout.storage_root.join(&media_path);
        let bytes = fs::read(&abs).with_context(|| format!("read media {}", abs.display()))?;
        if !xmp::is_png(&bytes) {
            return Ok(()); // non-PNG media has no XMP mirror (v1)
        }
        let created_at = facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}createdAt"))
            .and_then(|(_, v)| v.first().cloned())
            .unwrap_or_default();
        let packet = self.build_packet_from(&self.store, pan_id, &media_path, &created_at)?;
        let stamped = xmp::write_packet_into_png_bytes(&bytes, &packet)?;
        fs::write(&abs, &stamped).with_context(|| format!("write media {}", abs.display()))?;
        Ok(())
    }

    /// Graph facts → XMP packet, read from `store` (the live store on
    /// restamp; a scratch store holding the about-to-be-committed quads at
    /// ingest). Facts group into app blocks by reverse prefix lookup;
    /// multi-value predicates become Bags; sub-subjects (`<subj>/…`)
    /// re-author as their own Descriptions.
    fn build_packet_from(&self, store: &Store, pan_id: &str, media_path: &str, created_at: &str) -> Result<String> {
        let subject = {
            let obj = Literal::new_simple_literal(pan_id);
            let mut found = None;
            for quad in store.quads_for_pattern(None, Some(pan_iri("id").as_ref()), Some(obj.as_ref().into()), Some(GraphName::DefaultGraph.as_ref())) {
                let quad = quad.context("resolve id")?;
                if let NamedOrBlankNode::NamedNode(n) = quad.subject {
                    // The media object, not an enrichment node that happens to share nothing.
                    if n.as_str().contains("/Image/") || n.as_str().contains("/Media/") {
                        found = Some(n);
                        break;
                    }
                }
            }
            found.ok_or_else(|| anyhow!("id not found: {pan_id}"))?
        };
        let subj_prefix = format!("{}/", subject.as_str());
        let read_facts = |s: &NamedNode| -> Result<Vec<(String, Vec<String>)>> {
            let mut map: HashMap<String, Vec<String>> = HashMap::new();
            for quad in store.quads_for_pattern(Some(s.into()), None, None, Some(GraphName::DefaultGraph.as_ref())) {
                let quad = quad.context("read facts")?;
                map.entry(quad.predicate.as_str().to_string()).or_default().push(term_str(&quad.object));
            }
            let mut out: Vec<_> = map.into_iter().collect();
            out.sort();
            Ok(out)
        };

        // Reverse prefix map, longest namespace first (most-specific wins).
        let mut rev: Vec<(&String, &String)> = self.cfg.prefixes.iter().collect();
        rev.sort_by_key(|(_, ns)| std::cmp::Reverse(ns.len()));
        let split_pred = |iri: &str| -> Option<(String, String, String)> {
            for (short, ns) in &rev {
                if let Some(local) = iri.strip_prefix(ns.as_str()) {
                    if !local.is_empty() && !local.contains('/') {
                        return Some(((*short).clone(), (*ns).clone(), local.to_string()));
                    }
                }
            }
            None
        };

        let all_facts = read_facts(&subject)?;
        let pan_field = |local: &str| -> Option<String> {
            all_facts
                .iter()
                .find(|(p, _)| p == &format!("{PAN_NS}{local}"))
                .and_then(|(_, v)| v.first().cloned())
        };

        // Enrichment references: follow each reference predicate to its
        // Enrichment node and read back what that node declares. The packet is
        // the image's own index of what exists for it, rebuilt from the graph
        // so the two cannot disagree.
        let mut enrichment: Vec<(String, Vec<enrich::EnrichmentRef>)> = Vec::new();
        for ref_local in ["regionData", "poseData", "captionData", "vectorData"] {
            let mut refs: Vec<enrich::EnrichmentRef> = Vec::new();
            for (pred, values) in &all_facts {
                if pred != &format!("{PAN_NS}{ref_local}") {
                    continue;
                }
                for node_iri in values {
                    let Ok(node) = NamedNode::new(node_iri.as_str()) else { continue };
                    let mut got = enrich::EnrichmentRef {
                        id: String::new(),
                        model: String::new(),
                        path: String::new(),
                        count: 0,
                    };
                    for q in store.quads_for_pattern(
                        Some((&node).into()),
                        None,
                        None,
                        Some(GraphName::DefaultGraph.as_ref()),
                    ) {
                        let q = q.context("read enrichment reference")?;
                        let v = term_str(&q.object);
                        match q.predicate.as_str().strip_prefix(PAN_NS) {
                            Some("id") => got.id = v,
                            Some("model") => got.model = v,
                            Some("path") => got.path = v,
                            Some("count") => got.count = v.parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                    if !got.path.is_empty() {
                        refs.push(got);
                    }
                }
            }
            refs.sort_by(|a, b| a.path.cmp(&b.path));
            if !refs.is_empty() {
                enrichment.push((ref_local.to_string(), refs));
            }
        }

        // Root facts → app blocks (pan: identity fields re-authored, not copied).
        let mut app_fields: HashMap<(String, String), Vec<(String, xmp::FieldValue)>> = HashMap::new();
        for (pred, values) in all_facts.iter().cloned() {
            if pred.starts_with(PAN_NS) {
                continue;
            }
            if let Some((prefix, ns, local)) = split_pred(&pred) {
                let fv = if values.len() == 1 {
                    xmp::FieldValue::Scalar(values[0].clone())
                } else {
                    xmp::FieldValue::Bag(values.clone())
                };
                app_fields.entry((prefix, ns)).or_default().push((local, fv));
            }
        }
        let mut app_blocks: Vec<xmp::AppBlock> = app_fields
            .into_iter()
            .map(|((prefix, ns_iri), mut fields)| {
                fields.sort_by(|a, b| a.0.cmp(&b.0));
                xmp::AppBlock { prefix, ns_iri, fields }
            })
            .collect();
        app_blocks.sort_by(|a, b| a.prefix.cmp(&b.prefix));

        // Sub-subjects: named subjects scoped under this object's subject IRI.
        // Keep object term-type so rdf:type (and other IRI objects) re-author
        // as `rdf:resource` on the way out, not as string literals.
        let rdf_type_iri = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let mut sub_facts: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
        for quad in store.iter() {
            let quad = quad.context("scan sub-subjects")?;
            let subj_iri = match &quad.subject {
                NamedOrBlankNode::NamedNode(n) => n.as_str().to_string(),
                _ => continue,
            };
            if !subj_iri.starts_with(&subj_prefix) {
                continue;
            }
            sub_facts
                .entry(subj_iri)
                .or_default()
                .entry(quad.predicate.as_str().to_string())
                .or_default()
                .push(term_str(&quad.object));
        }
        let mut sub_blocks: Vec<xmp::SubSubjectBlock> = Vec::new();
        for (about, mut preds) in sub_facts {
            // rdf:type → the sub-subject's rdf:resource type IRI.
            let rdf_type = preds
                .remove(rdf_type_iri)
                .and_then(|v| v.into_iter().next())
                .unwrap_or_default();

            // Every mappable predicate keeps its own namespace — a region with
            // copia: AND dc: fields exports BOTH, none silently dropped.
            let mut namespaces: Vec<(String, String)> = Vec::new();
            let mut fields: Vec<(String, String, xmp::FieldValue)> = Vec::new();
            let mut sorted: Vec<_> = preds.into_iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            for (pred, terms) in sorted {
                if let Some((prefix, ns, local)) = split_pred(&pred) {
                    if !namespaces.iter().any(|(p, _)| p == &prefix) {
                        namespaces.push((prefix.clone(), ns));
                    }
                    let values: Vec<String> = terms;
                    let fv = if values.len() == 1 {
                        xmp::FieldValue::Scalar(values[0].clone())
                    } else {
                        xmp::FieldValue::Bag(values)
                    };
                    fields.push((prefix, local, fv));
                }
            }
            if !fields.is_empty() || !rdf_type.is_empty() {
                namespaces.sort();
                sub_blocks.push(xmp::SubSubjectBlock { about, rdf_type, namespaces, fields });
            }
        }
        sub_blocks.sort_by(|a, b| a.about.cmp(&b.about));

        // The thumbnail node, if declared: path + size go into the packet.
        let thumbnail = pan_field("thumbnail").and_then(|node_iri| {
            let node = NamedNode::new(node_iri).ok()?;
            let mut path = None;
            let mut w = None;
            let mut h = None;
            for q in store.quads_for_pattern(Some((&node).into()), None, None, Some(GraphName::DefaultGraph.as_ref())) {
                let q = q.ok()?;
                let v = term_str(&q.object);
                match q.predicate.as_str().strip_prefix(PAN_NS) {
                    Some("path") => path = Some(v),
                    Some("width") => w = v.parse().ok(),
                    Some("height") => h = v.parse().ok(),
                    _ => {}
                }
            }
            Some((path?, w?, h?))
        });

        Ok(xmp::build_packet(&xmp::ImagePacket {
            pan_id: pan_id.to_string(),
            media_path: media_path.to_string(),
            created_at: created_at.to_string(),
            media_type: pan_field("mediaType").unwrap_or_default(),
            width: pan_field("width").and_then(|v| v.parse().ok()),
            height: pan_field("height").and_then(|v| v.parse().ok()),
            caption: pan_field("caption"),
            thumbnail,
            enrichment,
            app_blocks,
            sub_subjects: sub_blocks,
        }))
    }

    /// Persist dirty vector indexes. Called on Drop too.
    pub fn flush(&self) -> Result<()> {
        let mut indexes = self.indexes.lock().unwrap();
        for vi in indexes.values_mut() {
            if vi.dirty {
                vi.save()?;
                vi.dirty = false;
            }
        }
        Ok(())
    }

    fn quad(&self, subject: &NamedNode, local: &str, value: &str) -> Quad {
        Quad::new(
            subject.clone(),
            pan_iri(local),
            Literal::new_simple_literal(value),
            GraphName::DefaultGraph,
        )
    }
}

impl Drop for Pan {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
