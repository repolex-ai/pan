//! Pan — a media store that speaks git-lex: stores media, describes it with a
//! graph, searches by graph pattern AND vector similarity.
//!
//! What this crate holds is ONE store (`Pan`). `pand` (src/daemon) opens
//! every store on the machine and is the only writer; `pan` (the CLI) and
//! Horae are its clients; git-lex reads the stores through the pan kit.
//!
//! Rules the code lives by (Rob, 2026-09-03), in the order they bite:
//! - Everything Pan says is declared in ontology/pan.ttl FIRST. No predicate
//!   is emitted that the ontology does not declare.
//! - Identity is the universal `git-lex:id`: the Thing's IRI
//!   `https://repolex.ai/pan/Image/<id>`, assigned once, never content-derived.
//! - Facts live in the DEFAULT graph. No graph names.
//! - Ingest order: bytes on disk (with Pan's XMP written into them) → thumbnail
//!   → ONE graph transaction. An object exists only after that commit.
//! - Pan writes its own block AND the producer's copia block into the image
//!   XMP, standard RDF-in-XMP, and never strips anything the image arrived
//!   with. Pixels are never touched (chunk surgery, no re-encode).
//! - Every `*Date` is RFC3339 in system local time.
//! - Loud failures: unresolvable predicates and broken config are errors.

use anyhow::{anyhow, Context, Result};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
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
pub mod layout;
pub mod npy;
pub mod pngchunk;
pub mod thumbnail;
pub mod xmp;

pub use config::{now_local, PanConfig, GIT_LEX_NS, PAN_MEDIA_NS, PAN_NS};
pub use facts::Facts;
pub use layout::PanLayout;

/// The Pan base ontology, shipped with the binary. Written to `<root>/pan.ttl`
/// as a reference copy at open; NOT loaded into the media graph.
pub const PAN_ONTOLOGY_TTL: &str = include_str!("../ontology/pan.ttl");

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The angle-bracket form of a pan identity, as it appears everywhere a
/// person or another tool sees it: `<pan/Image/k7m2p9x4>` — the same notation
/// git-lex uses for every other Thing.
pub fn bracket_iri(iri: &str) -> String {
    match iri.strip_prefix("https://repolex.ai/") {
        Some(rest) => format!("<{rest}>"),
        None => iri.to_string(),
    }
}

/// Accept an identity in any form a caller hands over — `<pan/Image/x>`, the
/// full IRI, or the bare id — and return the bare id.
pub fn bare_id(given: &str) -> String {
    let s = given.trim();
    let s = s.strip_prefix('<').and_then(|r| r.strip_suffix('>')).unwrap_or(s);
    s.rsplit('/').next().unwrap_or(s).to_string()
}

/// The id alphabet: RFC 4648 base32, lowercased. Short, IRI/filename-safe.
const PAN_ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
const PAN_ID_LEN: usize = 8;

/// A candidate id: 8 random base32 chars (40 bits). Assigned, never
/// content-derived. Collision safety is the caller's loop against the store.
pub fn gen_pan_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..PAN_ID_LEN)
        .map(|_| PAN_ID_ALPHABET[rng.gen_range(0..PAN_ID_ALPHABET.len())] as char)
        .collect()
}

/// A caller-supplied id reaches filesystem paths — reject anything that is
/// not a bare token before it touches `Path::join`.
pub(crate) fn validate_pan_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        return Err(anyhow!("invalid id {id:?}"));
    }
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        return Err(anyhow!("invalid id {id:?}: only [A-Za-z0-9_-] allowed"));
    }
    Ok(())
}

/// The ontology CLASS of a media object from its MIME major type — the
/// Capitalized local name from pan.ttl, naming both the IRI path segment and
/// the rdf:type. Only declared classes are used; everything not image/* is
/// the base class Media until its class is declared.
pub(crate) fn media_class(media_type: &str) -> &str {
    match media_type.split('/').next() {
        Some("image") => "Image",
        _ => "Media",
    }
}

pub(crate) fn media_subject_iri(media_type: &str, id: &str) -> Result<NamedNode> {
    NamedNode::new(format!("{PAN_MEDIA_NS}{}/{id}", media_class(media_type))).map_err(|e| anyhow!("invalid media IRI: {e}"))
}

pub(crate) fn pan_iri(local: &str) -> NamedNode {
    NamedNode::new(format!("{PAN_NS}{local}")).expect("valid pan IRI")
}

pub(crate) fn git_lex_iri(local: &str) -> NamedNode {
    NamedNode::new(format!("{GIT_LEX_NS}{local}")).expect("valid git-lex IRI")
}

fn rdf_type() -> NamedNode {
    NamedNode::new(RDF_TYPE).expect("rdf:type")
}

/// Validate a vector index name before it reaches `Path::join` — one
/// directory component, safe charset, no separators (the traversal hole the
/// review caught).
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
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(anyhow!("invalid index name {name:?}: only [A-Za-z0-9._-] allowed (no path separators)"));
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

/// One vector index: usearch HNSW + the id↔key bijection sidecar
/// (`keymap.json`). Lifted from Pool. Lives at `<hnsw_root>/<name>/`; dim is
/// fixed by the first insert and, for an existing index, by the file.
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
        let mut true_dim = dim;
        if path.exists() {
            index.load(path.to_str().unwrap())?;
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
        Ok(Self { dim: true_dim, index, id_to_key, key_to_id, next_key, path, dirty: false })
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
    /// The assigned identity, bare — new on EVERY put.
    pub id: String,
    /// The full IRI written for this object (`git-lex:id`).
    pub iri: String,
    pub media_path: String,
    pub created_date: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// False = the bytes could not be decoded as an image (still stored).
    pub thumbnail: bool,
    /// Statements loaded from the delivered metadata block.
    pub delivered_statements: usize,
}

/// What exists for one media object, read from the graph alone.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaState {
    pub id: String,
    pub iri: String,
    pub media_type: String,
    pub created_date: String,
    pub ready_date: Option<String>,
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
    /// The store's own identity (a soul's genesis SHA, or a bare store id).
    pub store_id: String,
    store: Store,
    indexes: Mutex<HashMap<String, VectorIndex>>,
}

impl Pan {
    /// Open a bare store at `root`: id from `<root>/pan.yml` (`storage_id`),
    /// media in the pocket.
    pub fn open(root: &Path) -> Result<Self> {
        let cfg = PanConfig::load(root)?;
        let id = cfg.storage_id.clone();
        Self::open_with(root, &id, None)
    }

    /// Open (or initialize) the store at `root` with an explicit identity and
    /// media root — what pand does for every store it manages. Writes the
    /// `pan:Store` node so the graph itself declares where its media lives.
    pub fn open_with(root: &Path, store_id: &str, media_root: Option<&Path>) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("create store root {}", root.display()))?;
        let cfg = PanConfig::load(root)?;
        let layout = PanLayout::resolve(root, media_root);
        fs::create_dir_all(&layout.oxigraph_root).context("create oxigraph root")?;
        fs::create_dir_all(&layout.hnsw_root).context("create hnsw root")?;
        fs::create_dir_all(&layout.media_root).with_context(|| format!("create media root {}", layout.media_root.display()))?;
        let ttl_path = root.join("pan.ttl");
        if fs::read_to_string(&ttl_path).ok().as_deref() != Some(PAN_ONTOLOGY_TTL) {
            fs::write(&ttl_path, PAN_ONTOLOGY_TTL).context("write pan.ttl reference copy")?;
        }
        let store = Store::open(&layout.oxigraph_root)
            .with_context(|| format!("open oxigraph at {}", layout.oxigraph_root.display()))?;
        let pan = Pan { cfg, layout, store_id: store_id.to_string(), store, indexes: Mutex::new(HashMap::new()) };
        pan.declare_store()?;
        Ok(pan)
    }

    /// The store node `<pan/Store/<id>>`: type, identity, media root. Replaces
    /// a stale media root (the volume moved) rather than adding a second one.
    fn declare_store(&self) -> Result<()> {
        let node = NamedNode::new(format!("{PAN_MEDIA_NS}Store/{}", self.store_id)).map_err(|e| anyhow!("store IRI: {e}"))?;
        let media_root = self.layout.media_root.to_string_lossy().to_string();
        let mut t = self.store.start_transaction().context("start transaction")?;
        let old: Vec<Quad> = self
            .store
            .quads_for_pattern(Some((&node).into()), Some(pan_iri("mediaRoot").as_ref()), None, Some(GraphName::DefaultGraph.as_ref()))
            .collect::<std::result::Result<_, _>>()
            .context("read store node")?;
        for q in &old {
            t.remove(q.as_ref());
        }
        t.insert(Quad::new(node.clone(), rdf_type(), pan_iri("Store"), GraphName::DefaultGraph).as_ref());
        t.insert(enrich::self_id_quad(&node)?.as_ref());
        t.insert(self.quad(&node, "mediaRoot", &media_root).as_ref());
        t.commit().context("commit store node")?;
        Ok(())
    }

    // ── identity ──────────────────────────────────────────────────────────────

    fn mint_pan_id(&self) -> Result<String> {
        loop {
            let cand = gen_pan_id();
            if self.subject_for(&cand)?.is_none() {
                return Ok(cand);
            }
        }
    }

    /// Resolve a bare id to the media object's IRI. Identity is the IRI
    /// itself (`git-lex:id`), so the lookup is: does `<pan/Image/id>` (or
    /// `<pan/Media/id>`) have a type in this store.
    pub fn subject_for(&self, id: &str) -> Result<Option<NamedNode>> {
        if validate_pan_id(id).is_err() {
            return Ok(None);
        }
        for class in ["Image", "Media"] {
            let cand = NamedNode::new(format!("{PAN_MEDIA_NS}{class}/{id}")).map_err(|e| anyhow!("candidate IRI: {e}"))?;
            let exists = self
                .store
                .quads_for_pattern(Some((&cand).into()), Some(rdf_type().as_ref()), None, Some(GraphName::DefaultGraph.as_ref()))
                .next()
                .is_some();
            if exists {
                return Ok(Some(cand));
            }
        }
        Ok(None)
    }

    // ── ingest ────────────────────────────────────────────────────────────────

    /// Store media bytes as a NEW object. Every put assigns a fresh id.
    ///
    /// `delivered_block` is the producer's metadata (Horae's copia block) as
    /// RDF/XML: validated, written into the image XMP verbatim, its triples
    /// loaded unchanged. `facts` are caller predicate→value pairs (loud on
    /// unresolvable predicates).
    ///
    /// Order: bytes on disk (with Pan's XMP written in, nothing stripped) →
    /// thumbnail → ONE graph transaction. Failure before the commit removes
    /// the files written so far.
    pub fn put(&self, bytes: &[u8], content_type: Option<&str>, delivered_block: Option<&str>, facts: Facts) -> Result<PutResult> {
        let png = xmp::is_png(bytes);
        let media_type = content_type
            .map(|s| s.to_string())
            .unwrap_or_else(|| if png { "image/png".to_string() } else { "application/octet-stream".to_string() });
        let ext = match media_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "bin",
        };

        let id = self.mint_pan_id()?;
        let subject = media_subject_iri(&media_type, &id)?;
        let created_date = now_local();
        let shard = created_date.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
        let rel_path = PanLayout::media_rel_path(&shard, &id, ext);
        let abs_path = self.layout.abs(&rel_path);

        // The producer's block: checked, never changed.
        let (delivered_descs, delivered_quads) = match delivered_block {
            Some(b) => xmp::validate_delivered_block(b, subject.as_str())?,
            None => (Vec::new(), Vec::new()),
        };

        let mut quads = vec![
            Quad::new(subject.clone(), rdf_type(), pan_iri(media_class(&media_type)), GraphName::DefaultGraph),
            enrich::self_id_quad(&subject)?,
            self.quad(&subject, "mediaPath", &rel_path),
            self.quad(&subject, "createdDate", &created_date),
            self.quad(&subject, "mediaType", &media_type),
        ];

        // Whatever XMP the image arrived with: its facts about the image
        // (rdf:about="") attach to this object; named subjects stay as they
        // are. Read with a real RDF parser; a malformed foreign packet is
        // logged and the bytes still land (media-in is the job).
        let existing_packet = if png {
            match xmp::read_xmp_packet_from_bytes(bytes) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(id = %id, "unreadable existing XMP, kept in file as-is: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        if let Some(packet) = &existing_packet {
            match xmp::parse_packet(packet) {
                Ok(blocks) => {
                    for block in &blocks {
                        let subj = match &block.subject {
                            None => subject.clone(),
                            Some(iri) => match NamedNode::new(iri.as_str()) {
                                Ok(n) => n,
                                Err(_) => continue,
                            },
                        };
                        for (pred, values) in &block.facts {
                            if pred.starts_with(PAN_NS) || pred.starts_with(GIT_LEX_NS) {
                                continue; // a previous store's pan facts are not facts about THIS object
                            }
                            let Ok(p) = NamedNode::new(pred.as_str()) else { continue };
                            for v in values {
                                let obj: Term = if v.is_iri() {
                                    NamedNode::new(v.value()).map(Into::into).unwrap_or_else(|_| Literal::new_simple_literal(v.value()).into())
                                } else {
                                    Literal::new_simple_literal(v.value()).into()
                                };
                                quads.push(Quad::new(subj.clone(), p.clone(), obj, GraphName::DefaultGraph));
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(id = %id, "existing XMP not parseable as RDF, kept in file as-is: {e:#}"),
            }
        }

        quads.extend(delivered_quads.iter().cloned());
        quads.extend(facts.into_quads(&subject, &self.cfg.prefixes, &self.cfg.default_prefix)?);

        // Thumbnail — declared as its own node; not decodable = no thumbnail,
        // still stored, `pan state` says so.
        let mut thumb: Option<(String, u32, u32)> = None;
        let mut thumb_jpeg: Vec<u8> = Vec::new();
        let mut width = None;
        let mut height = None;
        if media_type.starts_with("image/") {
            match thumbnail::make(bytes) {
                Ok(t) => {
                    width = Some(t.source_width);
                    height = Some(t.source_height);
                    quads.push(self.quad(&subject, "width", &t.source_width.to_string()));
                    quads.push(self.quad(&subject, "height", &t.source_height.to_string()));
                    let rel = PanLayout::thumbnail_rel_path(&shard, &id);
                    let tnode = NamedNode::new(format!("{PAN_MEDIA_NS}Thumbnail/{}", gen_pan_id())).map_err(|e| anyhow!("thumbnail IRI: {e}"))?;
                    quads.push(Quad::new(subject.clone(), pan_iri("thumbnail"), tnode.clone(), GraphName::DefaultGraph));
                    quads.push(Quad::new(tnode.clone(), rdf_type(), pan_iri("Thumbnail"), GraphName::DefaultGraph));
                    quads.push(enrich::self_id_quad(&tnode)?);
                    quads.push(self.quad(&tnode, "path", &rel));
                    quads.push(self.quad(&tnode, "width", &t.width.to_string()));
                    quads.push(self.quad(&tnode, "height", &t.height.to_string()));
                    quads.push(self.quad(&tnode, "producedDate", &created_date));
                    thumb = Some((rel, t.width, t.height));
                    thumb_jpeg = t.jpeg;
                }
                Err(e) => tracing::warn!(id = %id, "no thumbnail: {e:#}"),
            }
        }

        if let Some(parent) = abs_path.parent() {
            fs::create_dir_all(parent).context("create media shard dir")?;
        }
        let land = || -> Result<()> {
            if png {
                // Pan's block is built from the very quads the graph is about
                // to receive (scratch store), so packet and graph cannot disagree.
                let scratch = Store::new().context("scratch store")?;
                for q in &quads {
                    scratch.insert(q.as_ref()).context("scratch insert")?;
                }
                let pan_desc = xmp::build_pan_description(&self.image_packet_from(&scratch, &subject)?);
                let packet = xmp::compose_packet(existing_packet.as_deref(), &pan_desc, &delivered_descs);
                let written = xmp::write_packet_into_png_bytes(bytes, &packet)?;
                fs::write(&abs_path, &written).with_context(|| format!("write media {}", abs_path.display()))?;
            } else {
                fs::write(&abs_path, bytes).with_context(|| format!("write media {}", abs_path.display()))?;
            }
            if let Some((rel, _, _)) = &thumb {
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
            if let Some((rel, _, _)) = &thumb {
                let _ = fs::remove_file(self.layout.abs(rel));
            }
            return Err(e);
        }

        Ok(PutResult {
            id,
            iri: subject.into_string(),
            media_path: rel_path,
            created_date,
            width,
            height,
            thumbnail: thumb.is_some(),
            delivered_statements: delivered_quads.len(),
        })
    }

    /// Insert quads as ONE transaction: all land or none do.
    pub fn insert_quads(&self, quads: &[Quad]) -> Result<()> {
        let mut t = self.store.start_transaction().context("start transaction")?;
        for q in quads {
            t.insert(q.as_ref());
        }
        t.commit().context("commit transaction")?;
        Ok(())
    }

    // ── read ──────────────────────────────────────────────────────────────────

    /// Media bytes + facts by id.
    pub fn get(&self, id: &str) -> Result<(Vec<u8>, Vec<(String, Vec<String>)>)> {
        let facts = self.facts_for(id)?;
        let media_path = facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}mediaPath"))
            .and_then(|(_, v)| v.first().cloned())
            .ok_or_else(|| anyhow!("id not found: {id}"))?;
        let abs = self.layout.abs(&media_path);
        let bytes = fs::read(&abs).with_context(|| format!("read media {}", abs.display()))?;
        Ok((bytes, facts))
    }

    /// All facts on the object's subject: full-IRI predicate → values. Empty =
    /// unknown id.
    pub fn facts_for(&self, id: &str) -> Result<Vec<(String, Vec<String>)>> {
        let Some(subject) = self.subject_for(id)? else { return Ok(vec![]) };
        Self::facts_of(&self.store, &subject)
    }

    fn facts_of(store: &Store, subject: &NamedNode) -> Result<Vec<(String, Vec<String>)>> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for quad in store.quads_for_pattern(Some(subject.into()), None, None, Some(GraphName::DefaultGraph.as_ref())) {
            let quad = quad.context("read facts")?;
            map.entry(quad.predicate.as_str().to_string()).or_default().push(term_str(&quad.object));
        }
        let mut out: Vec<_> = map.into_iter().collect();
        out.sort();
        Ok(out)
    }

    /// One `pan:` field of an arbitrary node by its IRI.
    pub fn node_field(&self, node_iri: &str, local: &str) -> Result<Option<String>> {
        let node = NamedNode::new(node_iri).map_err(|e| anyhow!("invalid node IRI {node_iri}: {e}"))?;
        for q in self.store.quads_for_pattern(Some((&node).into()), Some(pan_iri(local).as_ref()), None, Some(GraphName::DefaultGraph.as_ref())) {
            let q = q.context("read node field")?;
            return Ok(Some(term_str(&q.object)));
        }
        Ok(None)
    }

    /// What exists for one object, from the graph alone.
    pub fn state_for(&self, id: &str) -> Result<Option<MediaState>> {
        let Some(subject) = self.subject_for(id)? else { return Ok(None) };
        let facts = self.facts_for(id)?;
        let one = |local: &str| -> Option<String> {
            facts.iter().find(|(p, _)| p == &format!("{PAN_NS}{local}")).and_then(|(_, v)| v.first().cloned())
        };
        let mut enrichment = Vec::new();
        for link in ["embedding", "captionItem", "region", "pose"] {
            let mut models: Vec<String> = Vec::new();
            for (pred, values) in &facts {
                if pred != &format!("{PAN_NS}{link}") {
                    continue;
                }
                for node_iri in values {
                    if let Some(m) = self.node_field(node_iri, "model")? {
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
            media_type: one("mediaType").unwrap_or_default(),
            created_date: one("createdDate").unwrap_or_default(),
            ready_date: one("readyDate"),
            thumbnail: facts.iter().any(|(p, _)| p == &format!("{PAN_NS}thumbnail")),
            enrichment,
        }))
    }

    /// Images with NO record from `model` under `link_local` — the stage
    /// engine's work list. The graph is the queue: pending means absent.
    pub fn pending_for(&self, link_local: &str, model: &str, limit: usize) -> Result<Vec<PendingItem>> {
        let model_lit = model.replace('\\', "\\\\").replace('"', "\\\"");
        let q = format!(
            "SELECT ?s ?path ?type WHERE {{
               ?s a pan:Image ; pan:mediaPath ?path ; pan:mediaType ?type .
               FILTER NOT EXISTS {{ ?s pan:{link_local} ?e . ?e pan:model \"{model_lit}\" }}
             }} ORDER BY ?s LIMIT {limit}"
        );
        let mut out = Vec::new();
        if let QueryResults::Solutions(sols) = self.query(&q)? {
            for s in sols {
                let s = s?;
                let get = |v: &str| s.get(v).map(term_str).unwrap_or_default();
                let iri = get("s");
                out.push(PendingItem { id: bare_id(&iri), iri, media_path: get("path"), media_type: get("type") });
            }
        }
        Ok(out)
    }

    /// Images that have every listed (link, model) pair recorded but no
    /// `pan:readyDate` yet — the ones the ladder can now mark ready.
    pub fn ready_candidates(&self, required: &[(String, String)], limit: usize) -> Result<Vec<String>> {
        let mut q = String::from("SELECT ?s WHERE { ?s a pan:Image . FILTER NOT EXISTS { ?s pan:readyDate ?r } ");
        for (i, (link, model)) in required.iter().enumerate() {
            let m = model.replace('\\', "\\\\").replace('"', "\\\"");
            q.push_str(&format!("?s pan:{link} ?e{i} . ?e{i} pan:model \"{m}\" . "));
        }
        q.push_str(&format!("}} LIMIT {limit}"));
        let mut out = Vec::new();
        if let QueryResults::Solutions(sols) = self.query(&q)? {
            for s in sols {
                let s = s?;
                if let Some(t) = s.get("s") {
                    out.push(bare_id(&term_str(t)));
                }
            }
        }
        Ok(out)
    }

    /// Set `pan:readyDate` now, once; a later call is a no-op. XMP refreshed.
    pub fn mark_ready(&self, id: &str) -> Result<bool> {
        let Some(subject) = self.subject_for(id)? else { return Err(anyhow!("id not found: {id}")) };
        let already = self
            .store
            .quads_for_pattern(Some((&subject).into()), Some(pan_iri("readyDate").as_ref()), None, Some(GraphName::DefaultGraph.as_ref()))
            .next()
            .is_some();
        if already {
            return Ok(false);
        }
        self.insert_quads(&[self.quad(&subject, "readyDate", &now_local())])?;
        self.restamp(id)?;
        Ok(true)
    }

    // ── describe / enrich ─────────────────────────────────────────────────────

    /// Merge caller facts onto an existing object (loud on unresolvable
    /// predicates). XMP refreshed.
    pub fn describe(&self, id: &str, facts: Facts) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else { return Err(anyhow!("id not found: {id}")) };
        let quads = facts.into_quads(&subject, &self.cfg.prefixes, &self.cfg.default_prefix)?;
        self.insert_quads(&quads)?;
        self.restamp(id)
    }

    fn created_date_of(&self, id: &str) -> Result<String> {
        Ok(self
            .facts_for(id)?
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}createdDate"))
            .and_then(|(_, v)| v.first().cloned())
            .unwrap_or_default())
    }

    /// Record one model's output for an object as a data file beside the
    /// media plus the graph statements that describe it, then refresh the
    /// XMP so the image's own packet lists the new file. `kind` = data-file
    /// directory (caption / sam3 / pose); `link_local` = membership predicate;
    /// `ref_local` = reference predicate.
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
        let Some(subject) = self.subject_for(id)? else { return Err(anyhow!("id not found: {id}")) };
        let created = self.created_date_of(id)?;
        let shard = created.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
        let rel = PanLayout::enrichment_rel_path(kind, &shard, id, variant);
        let abs = self.layout.abs(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).context("create enrichment dir")?;
        }
        fs::write(&abs, enrich::build_data_file(subject.as_str(), link_local, records)).with_context(|| format!("write {}", abs.display()))?;
        let mut quads = enrich::record_quads(subject.as_str(), link_local, records)?;
        let r = enrich::EnrichmentRef::new(model, &rel, records.len());
        quads.extend(enrich::ref_quads(subject.as_str(), ref_local, &r)?);
        if let Err(e) = self.insert_quads(&quads) {
            let _ = fs::remove_file(&abs);
            return Err(e);
        }
        self.restamp(id)?;
        Ok(rel)
    }

    /// Record an embedding: vector into the index + `.npy` sidecar, an
    /// Embedding node and a vectorData reference, XMP refreshed.
    pub fn write_embedding(&self, id: &str, model: &str, index_name: &str, vec: &[f32]) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else { return Err(anyhow!("id not found: {id}")) };
        self.add_vector(id, index_name, vec)?;
        self.flush()?;
        let rel = PanLayout::vector_rel_path(index_name, id);
        let rec = enrich::EnrichmentRecord::new(gen_pan_id(), "Embedding", model)
            .field("dim", vec.len().to_string())
            .field("vectorPath", &rel);
        let mut quads = enrich::record_quads(subject.as_str(), "embedding", std::slice::from_ref(&rec))?;
        quads.extend(enrich::ref_quads(subject.as_str(), "vectorData", &enrich::EnrichmentRef::new(model, &rel, 1))?);
        self.insert_quads(&quads)?;
        self.restamp(id)
    }

    /// Set the object's current caption text (replaces any previous value).
    pub fn set_caption(&self, id: &str, text: &str) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else { return Err(anyhow!("id not found: {id}")) };
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

    /// Delete an object: media, thumbnail, data files, vector sidecars +
    /// index entries, and every statement about it or its records.
    pub fn delete(&self, id: &str) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else { return Err(anyhow!("id not found: {id}")) };
        let facts = self.facts_for(id)?;
        let pan_val = |local: &str| -> Option<String> {
            facts.iter().find(|(p, _)| p == &format!("{PAN_NS}{local}")).and_then(|(_, v)| v.first().cloned())
        };
        // Files: media, thumbnail, every referenced data file.
        let mut rels: Vec<String> = Vec::new();
        rels.extend(pan_val("mediaPath"));
        if let Some(t) = pan_val("thumbnail") {
            rels.extend(self.node_field(&t, "path")?);
        }
        // Linked nodes (enrichment refs, records, thumbnail) — their statements go too.
        let mut linked: Vec<NamedNode> = Vec::new();
        for (pred, values) in &facts {
            if !pred.starts_with(PAN_NS) {
                continue;
            }
            for v in values {
                if v.starts_with(PAN_MEDIA_NS) {
                    if let Ok(n) = NamedNode::new(v.as_str()) {
                        if let Some(p) = self.node_field(v, "path")? {
                            rels.push(p);
                        }
                        linked.push(n);
                    }
                }
            }
        }
        for rel in &rels {
            let abs = self.layout.abs(rel);
            if abs.exists() {
                fs::remove_file(&abs).with_context(|| format!("remove {}", abs.display()))?;
            }
        }
        let mut t = self.store.start_transaction().context("start transaction")?;
        let mut targets = vec![subject.clone()];
        targets.extend(linked);
        for s in &targets {
            let qs: Vec<Quad> = self
                .store
                .quads_for_pattern(Some(s.into()), None, None, Some(GraphName::DefaultGraph.as_ref()))
                .collect::<std::result::Result<_, _>>()
                .context("scan for delete")?;
            for q in &qs {
                t.remove(q.as_ref());
            }
        }
        t.commit().context("commit delete")?;

        // Vector index entries, across every index on disk.
        validate_pan_id(id)?;
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
                let known = fs::read_to_string(self.layout.hnsw_root.join(&name).join("keymap.json"))
                    .ok()
                    .and_then(|raw| serde_json::from_str::<HashMap<String, u64>>(&raw).ok())
                    .map(|m| m.contains_key(id))
                    .unwrap_or(false);
                if !known {
                    continue;
                }
                indexes.insert(name.clone(), VectorIndex::create(&self.layout.hnsw_root, &name, 0)?);
            }
            if let Some(vi) = indexes.get_mut(&name) {
                if let Some(key) = vi.id_to_key.remove(id) {
                    vi.key_to_id.remove(&key);
                    vi.index.remove(key).ok();
                    vi.dirty = true;
                }
            }
            let sidecar = self.layout.vector_sidecar_path(&name, id);
            if sidecar.exists() {
                fs::remove_file(&sidecar).ok();
            }
        }
        Ok(())
    }

    // ── vectors + search (the crown jewel, lifted from Pool) ──────────────────

    /// Attach a vector: `.npy` sidecar + the named HNSW index. Idempotent per
    /// (id, index): `Ok(false)` when already present.
    pub fn add_vector(&self, id: &str, index_name: &str, vec: &[f32]) -> Result<bool> {
        validate_pan_id(id)?;
        let mut indexes = self.indexes.lock().unwrap();
        if !indexes.contains_key(index_name) {
            indexes.insert(index_name.to_string(), VectorIndex::create(&self.layout.hnsw_root, index_name, vec.len())?);
        }
        let vi = indexes.get_mut(index_name).unwrap();
        if vec.len() != vi.dim {
            return Err(anyhow!("vector dim {} does not match index {} dim {}", vec.len(), index_name, vi.dim));
        }
        if vi.id_to_key.contains_key(id) {
            return Ok(false);
        }
        npy::write_f32_1d(&self.layout.vector_sidecar_path(index_name, id), vec)?;
        let key = vi.next_key;
        vi.next_key += 1;
        vi.id_to_key.insert(id.to_string(), key);
        vi.key_to_id.insert(key, id.to_string());
        let needed = vi.id_to_key.len();
        if vi.index.capacity() < needed {
            vi.index.reserve(needed.max(1024))?;
        }
        vi.index.add(key, vec).map_err(|e| anyhow!("usearch add (id {}, index {}): {}", id, index_name, e))?;
        vi.dirty = true;
        Ok(true)
    }

    pub fn contains_id(&self, id: &str, index_name: &str) -> bool {
        let indexes = self.indexes.lock().unwrap();
        indexes.get(index_name).map(|vi| vi.id_to_key.contains_key(id)).unwrap_or(false)
    }

    /// `(dim, count)` for every index visible on disk or in memory.
    pub fn index_stats(&self) -> Vec<(String, IndexStats)> {
        let indexes = self.indexes.lock().unwrap();
        let mut out: Vec<(String, IndexStats)> = indexes
            .iter()
            .map(|(name, vi)| (name.clone(), IndexStats { dim: vi.dim, count: vi.id_to_key.len() }))
            .collect();
        if let Ok(rd) = fs::read_dir(&self.layout.hnsw_root) {
            for e in rd.filter_map(|e| e.ok()) {
                let Some(name) = e.file_name().to_str().map(String::from) else { continue };
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

    /// Hybrid query — THE reason Pan exists. The SPARQL `where` (constraining
    /// `?s`, the media subject) gates the candidate set; usearch kNN ranks by
    /// cosine similarity to `like`. Pre-filter then search, joined at the
    /// application layer by the id↔key map. Empty `where` = pure kNN.
    pub fn search(&self, where_clause: &str, like: &[f32], k: usize, index_name: &str) -> Result<Vec<SearchHit>> {
        validate_index_name(index_name)?;
        let q = format!(
            "{}
             SELECT DISTINCT ?s WHERE {{
               ?s a pan:Image .
               {where_clause}
             }}",
            self.prefix_prologue()
        );
        let mut candidate_ids: HashSet<String> = HashSet::new();
        if let QueryResults::Solutions(sols) = self.store.query(&q).map_err(|e| anyhow!("search where-clause: {e}"))? {
            for s in sols {
                let s = s?;
                if let Some(t) = s.get("s") {
                    candidate_ids.insert(bare_id(&term_str(t)));
                }
            }
        }
        if candidate_ids.is_empty() {
            return Ok(vec![]);
        }
        let mut indexes = self.indexes.lock().unwrap();
        if !indexes.contains_key(index_name) {
            if self.layout.hnsw_root.join(index_name).join("index.usearch").exists() {
                indexes.insert(index_name.to_string(), VectorIndex::create(&self.layout.hnsw_root, index_name, 0)?);
            }
        }
        let vi = indexes.get_mut(index_name).ok_or_else(|| anyhow!("no such index: {index_name} (no vectors attached yet?)"))?;
        if vi.dim != like.len() {
            return Err(anyhow!("query embedding dim {} does not match index {} dim {}", like.len(), index_name, vi.dim));
        }
        let candidate_keys: HashSet<u64> = candidate_ids.iter().filter_map(|c| vi.id_to_key.get(c).copied()).collect();
        if candidate_keys.is_empty() {
            return Ok(vec![]);
        }
        let total = vi.id_to_key.len() as f32;
        let selectivity = (candidate_keys.len() as f32 / total).max(0.001);
        let ef = ((k as f32 / selectivity).clamp(64.0, 4096.0)) as usize;
        vi.index.change_expansion_search(ef);
        let matches = vi.index.filtered_search(like, k, |key| candidate_keys.contains(&key))?;
        let mut hits = Vec::with_capacity(matches.keys.len());
        for (key, distance) in matches.keys.iter().zip(matches.distances.iter()) {
            if let Some(id) = vi.key_to_id.get(key) {
                hits.push(SearchHit { id: id.clone(), score: 1.0 - *distance });
            }
        }
        Ok(hits)
    }

    // ── SPARQL ────────────────────────────────────────────────────────────────

    /// Run a SPARQL query with the store's prefixes pre-declared (pan, git-lex,
    /// copia, pan.yml extras, rdf/rdfs/owl/xsd).
    pub fn query(&self, sparql: &str) -> Result<QueryResults<'_>> {
        let prologue = self.prefix_prologue();
        self.store.query(&format!("{prologue}{sparql}")).map_err(|e| anyhow!("SPARQL error: {e}"))
    }

    fn prefix_prologue(&self) -> String {
        let mut p = String::from(
            "PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>\n\
             PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>\n\
             PREFIX owl: <http://www.w3.org/2002/07/owl#>\n\
             PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n",
        );
        p.push_str(&format!("PREFIX git-lex: <{GIT_LEX_NS}>\n"));
        for (short, ns) in &self.cfg.prefixes {
            p.push_str(&format!("PREFIX {short}: <{ns}>\n"));
        }
        p
    }

    // ── XMP (graph → the image's own packet) ──────────────────────────────────

    /// Rewrite the image's XMP from the CURRENT graph: Pan's block is
    /// re-authored, every other Description already in the file is kept
    /// verbatim, no other chunk is touched.
    pub fn restamp(&self, id: &str) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else { return Err(anyhow!("id not found: {id}")) };
        let facts = self.facts_for(id)?;
        let Some(media_path) = facts.iter().find(|(p, _)| p == &format!("{PAN_NS}mediaPath")).and_then(|(_, v)| v.first()) else {
            return Err(anyhow!("id not found: {id}"));
        };
        let abs = self.layout.abs(media_path);
        let bytes = fs::read(&abs).with_context(|| format!("read media {}", abs.display()))?;
        if !xmp::is_png(&bytes) {
            return Ok(()); // non-PNG media carries no XMP (v1)
        }
        let existing = xmp::read_xmp_packet_from_bytes(&bytes).unwrap_or(None);
        let pan_desc = xmp::build_pan_description(&self.image_packet_from(&self.store, &subject)?);
        let packet = xmp::compose_packet(existing.as_deref(), &pan_desc, &[]);
        let written = xmp::write_packet_into_png_bytes(&bytes, &packet)?;
        fs::write(&abs, &written).with_context(|| format!("write media {}", abs.display()))?;
        Ok(())
    }

    /// Pan's own block for one object, read from `store` (the live store on
    /// restamp; a scratch store holding the about-to-be-committed quads at
    /// ingest).
    fn image_packet_from(&self, store: &Store, subject: &NamedNode) -> Result<xmp::ImagePacket> {
        let facts = Self::facts_of(store, subject)?;
        let pan_field = |local: &str| -> Option<String> {
            facts.iter().find(|(p, _)| p == &format!("{PAN_NS}{local}")).and_then(|(_, v)| v.first().cloned())
        };
        let node_fields = |node_iri: &str| -> Result<HashMap<String, String>> {
            let node = NamedNode::new(node_iri).map_err(|e| anyhow!("node IRI: {e}"))?;
            let mut m = HashMap::new();
            for q in store.quads_for_pattern(Some((&node).into()), None, None, Some(GraphName::DefaultGraph.as_ref())) {
                let q = q.context("read node")?;
                if let Some(l) = q.predicate.as_str().strip_prefix(PAN_NS) {
                    m.insert(l.to_string(), term_str(&q.object));
                }
            }
            Ok(m)
        };

        let mut enrichment: Vec<(String, Vec<enrich::EnrichmentRef>)> = Vec::new();
        for ref_local in ["regionData", "poseData", "captionData", "vectorData"] {
            let mut refs: Vec<enrich::EnrichmentRef> = Vec::new();
            for (pred, values) in &facts {
                if pred != &format!("{PAN_NS}{ref_local}") {
                    continue;
                }
                for node_iri in values {
                    let f = node_fields(node_iri)?;
                    if let Some(path) = f.get("path") {
                        refs.push(enrich::EnrichmentRef {
                            id: bare_id(node_iri),
                            model: f.get("model").cloned().unwrap_or_default(),
                            path: path.clone(),
                            count: f.get("count").and_then(|c| c.parse().ok()).unwrap_or(0),
                            produced_date: f.get("producedDate").cloned().unwrap_or_default(),
                        });
                    }
                }
            }
            refs.sort_by(|a, b| a.path.cmp(&b.path));
            if !refs.is_empty() {
                enrichment.push((ref_local.to_string(), refs));
            }
        }
        let thumbnail = match pan_field("thumbnail") {
            Some(t) => {
                let f = node_fields(&t)?;
                match (f.get("path"), f.get("width").and_then(|w| w.parse().ok()), f.get("height").and_then(|h| h.parse().ok())) {
                    (Some(p), Some(w), Some(h)) => Some((p.clone(), w, h)),
                    _ => None,
                }
            }
            None => None,
        };
        Ok(xmp::ImagePacket {
            iri: subject.as_str().to_string(),
            media_path: pan_field("mediaPath").unwrap_or_default(),
            created_date: pan_field("createdDate").unwrap_or_default(),
            media_type: pan_field("mediaType").unwrap_or_default(),
            width: pan_field("width").and_then(|v| v.parse().ok()),
            height: pan_field("height").and_then(|v| v.parse().ok()),
            caption: pan_field("caption"),
            ready_date: pan_field("readyDate"),
            thumbnail,
            enrichment,
        })
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
        Quad::new(subject.clone(), pan_iri(local), Literal::new_simple_literal(value), GraphName::DefaultGraph)
    }
}

impl Drop for Pan {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
