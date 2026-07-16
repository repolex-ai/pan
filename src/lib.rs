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
//! - **cid** is the bare `sha256:<hex>` form everywhere (wire, index, XMP);
//!   the subject IRI form is `urn:sha256:<hex>`. For PNGs the cid is the
//!   PIXEL cid — metadata edits never rotate identity.
//! - **Loud failures.** Unresolvable predicates and broken config are errors,
//!   never silent drops.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use oxigraph::model::{GraphName, Literal, NamedNode, Quad, NamedOrBlankNode};
pub use oxigraph::model::Term;
pub use oxigraph::sparql::{QueryResults, QuerySolution};
use oxigraph::store::Store;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

pub mod config;
pub mod detectors;
pub mod facts;
pub mod layout;
pub mod npy;
pub mod serve;
pub mod xmp;

pub use config::{PanConfig, PAN_NS};
pub use facts::Facts;
pub use layout::PanLayout;

/// The Pan base ontology, shipped with the binary. Written to `<root>/pan.ttl`
/// reference copy at open; NOT loaded into the media graph (facts stay pure —
/// schema is documentation, not data).
pub const PAN_ONTOLOGY_TTL: &str = include_str!("../ontology/pan.ttl");

/// Compute the FILE-byte content id (`sha256:<hex>`) of a byte slice. Used for
/// non-PNG media where no pixel-domain identity exists (yet).
pub fn compute_cid(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{:x}", h.finalize())
}

/// cid → subject IRI. Idempotent on the `urn:` prefix (carried from Pool's
/// Day-94 double-urn lesson): bare → urn:bare, urn:bare → urn:bare.
fn cid_iri(cid: &str) -> Result<NamedNode> {
    let bare = cid.strip_prefix("urn:").unwrap_or(cid);
    NamedNode::new(format!("urn:{bare}")).map_err(|e| anyhow!("invalid cid IRI urn:{bare}: {e}"))
}

fn pan_iri(local: &str) -> NamedNode {
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

/// One vector index: usearch HNSW + the CID↔key bijection sidecar
/// (`keymap.json`). Lifted from Pool. The index for a name lives at
/// `<hnsw_root>/<name>/index.usearch`; dim is fixed by the first insert.
struct VectorIndex {
    dim: usize,
    index: Index,
    cid_to_key: HashMap<String, u64>,
    key_to_cid: HashMap<u64, String>,
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

        let mut cid_to_key = HashMap::new();
        let mut key_to_cid = HashMap::new();
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
                for (cid, key) in &m {
                    key_to_cid.insert(*key, cid.clone());
                }
                cid_to_key = m;
            }
        }

        Ok(Self {
            dim: true_dim,
            index,
            cid_to_key,
            key_to_cid,
            next_key,
            path,
            dirty: false,
        })
    }

    fn save(&self) -> Result<()> {
        self.index.save(self.path.to_str().unwrap())?;
        let map_path = self.path.parent().unwrap().join("keymap.json");
        fs::write(&map_path, serde_json::to_string(&self.cid_to_key)?)?;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub cid: String,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct IndexStats {
    pub dim: usize,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PutResult {
    pub cid: String,
    pub blob_path: String,
    pub created_at: String,
    /// True when this call newly stored the object (false = already present).
    pub created: bool,
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

    /// Store media bytes. Content-addressed: PNGs by PIXEL cid, other bytes by
    /// file cid. For PNGs: any existing XMP app facts are ingested into the
    /// graph (real RDF parser), then Pan re-authors + stamps its own packet
    /// (pan: identity block + the graph's app facts) — pixel cid unchanged.
    ///
    /// `facts` are caller-supplied descriptions (loud on unresolvable
    /// predicates). Idempotent per cid: a re-put refreshes bytes + merges
    /// facts, keeps the original createdAt.
    pub fn put(&self, bytes: &[u8], content_type: Option<&str>, facts: Facts) -> Result<PutResult> {
        let png = xmp::is_png(bytes);
        let cid = if png {
            xmp::compute_pixel_cid(bytes)?
        } else {
            compute_cid(bytes)
        };
        let subject = cid_iri(&cid)?;

        // Re-put keeps the original createdAt AND the original mediaType/blob
        // location — identity, once minted, is stable. A re-put refreshes bytes
        // and merges caller facts; it must not fork a second blobPath by
        // honoring a different Content-Type on the same pixels.
        let existing = self.facts_for(&cid)?;
        let created = existing.is_empty();
        let find_one = |p: &str| -> Option<String> {
            existing
                .iter()
                .find(|(pred, _)| pred == &format!("{PAN_NS}{p}"))
                .and_then(|(_, v)| v.first().cloned())
        };
        let created_at =
            find_one("createdAt").unwrap_or_else(|| Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        // mediaType: existing wins on re-put; otherwise the caller's, else a
        // sensible default. This keeps the blob extension (and thus blobPath)
        // constant across re-puts.
        let media_type = find_one("mediaType").unwrap_or_else(|| {
            content_type.map(|s| s.to_string()).unwrap_or_else(|| {
                if png { "image/png".to_string() } else { "application/octet-stream".to_string() }
            })
        });
        let ext = match media_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "bin",
        };

        // blob/image/YYYY/MM/DD/<hex>.<ext> — date shard from createdAt,
        // filename from the cid (identity IS the name). On re-put, prefer the
        // stored blobPath verbatim so we never orphan a prior copy.
        let hex = cid.rsplit(':').next().unwrap_or(&cid);
        let shard = created_at.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
        let rel_path =
            find_one("blobPath").unwrap_or_else(|| format!("{}/{shard}/{hex}.{ext}", PanLayout::BLOB_SUBPATH));
        let abs_path = self.layout.storage_root.join(&rel_path);

        // Clear the prior identity block before re-authoring it — a re-put must
        // not accumulate stale cid/blobPath/createdAt/mediaType quads (the
        // fork-on-different-Content-Type bug). Caller/app facts merge; identity
        // is replaced.
        if !created {
            for local in ["cid", "blobPath", "createdAt", "mediaType"] {
                for quad in self
                    .store
                    .quads_for_pattern(
                        Some((&subject).into()),
                        Some(pan_iri(local).as_ref()),
                        None,
                        Some(GraphName::DefaultGraph.as_ref()),
                    )
                    .collect::<Result<Vec<_>, _>>()?
                {
                    self.store.remove(quad.as_ref()).context("clear prior identity quad")?;
                }
            }
        }

        // Identity facts — Pan's own block.
        let mut quads = vec![
            self.quad(&subject, "cid", &cid),
            self.quad(&subject, "blobPath", &rel_path),
            self.quad(&subject, "createdAt", &created_at),
            self.quad(&subject, "mediaType", &media_type),
        ];

        // Ingest existing XMP app facts (the walker-lite: media traveling in
        // carries its descriptions with it). pan: identity fields are skipped —
        // Pan re-authors those; a foreign store's blobPath is meaningless here.
        // A malformed foreign packet must NOT fail the store — media-in is the
        // job; a broken travel copy is logged and the bytes still land.
        if png {
            match xmp::read_xmp_packet_from_bytes(bytes) {
                Ok(Some(packet)) => match xmp::parse_packet(&packet) {
                    Ok(blocks) => {
                        for block in blocks {
                            // A sub-subject scoped OUTSIDE this cid's subject is a
                            // foreign region (its scope was the source store's cid);
                            // it can never be found or deleted here, so drop it
                            // rather than orphan it.
                            let subj = match &block.subject {
                                None => subject.clone(),
                                Some(iri) if iri.starts_with(&format!("{}/", subject.as_str())) => {
                                    NamedNode::new(iri.as_str())
                                        .map_err(|e| anyhow!("invalid sub-subject IRI {iri}: {e}"))?
                                }
                                Some(_) => continue,
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
                    Err(e) => tracing::warn!(cid = %cid, "skipping unparseable travel XMP: {e:#}"),
                },
                Ok(None) => {}
                Err(e) => tracing::warn!(cid = %cid, "skipping unreadable travel XMP: {e:#}"),
            }
        }

        // Caller facts — loud resolution, nothing written on error.
        quads.extend(facts.into_quads(&subject, &self.cfg.prefixes, &self.cfg.default_prefix)?);
        for q in &quads {
            self.store.insert(q.as_ref()).context("insert quad")?;
        }

        // Land the blob. PNGs get the authority stamp (graph → packet → pixels
        // preserved); other media lands verbatim.
        if let Some(parent) = abs_path.parent() {
            fs::create_dir_all(parent).context("create blob shard dir")?;
        }
        if png {
            let packet = self.build_packet_from_graph(&cid, &rel_path, &created_at)?;
            let stamped = xmp::write_packet_into_png_bytes(bytes, &packet)?;
            fs::write(&abs_path, &stamped)
                .with_context(|| format!("write blob {}", abs_path.display()))?;
        } else {
            fs::write(&abs_path, bytes)
                .with_context(|| format!("write blob {}", abs_path.display()))?;
        }

        Ok(PutResult {
            cid,
            blob_path: rel_path,
            created_at,
            created,
        })
    }

    /// Assert one triple on an arbitrary subject IRI. `object_is_iri` picks
    /// whether the object is a NamedNode (an IRI, e.g. an `rdf:type`) or a
    /// plain literal. Used to describe sub-subjects (regions etc.) that hang
    /// off a media object's subject. Does NOT re-stamp — call `restamp(cid)`
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

    /// Read media bytes + facts by cid. Works for bare and urn: cid forms.
    pub fn get(&self, cid: &str) -> Result<(Vec<u8>, Vec<(String, Vec<String>)>)> {
        let facts = self.facts_for(cid)?;
        let blob_path = facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}blobPath"))
            .and_then(|(_, v)| v.first().cloned())
            .ok_or_else(|| anyhow!("cid not found: {cid}"))?;
        let abs = self.layout.storage_root.join(&blob_path);
        let bytes = fs::read(&abs).with_context(|| format!("read blob {}", abs.display()))?;
        Ok((bytes, facts))
    }

    /// All facts for a cid's subject: full-IRI predicate → values. Empty vec =
    /// unknown cid.
    pub fn facts_for(&self, cid: &str) -> Result<Vec<(String, Vec<String>)>> {
        let subject = cid_iri(cid)?;
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

    /// Merge additional facts onto an existing cid (LOUD on unresolvable
    /// predicates, 404-style error on unknown cid). PNGs are re-stamped so the
    /// XMP mirror follows the graph.
    pub fn describe(&self, cid: &str, facts: Facts) -> Result<()> {
        let existing = self.facts_for(cid)?;
        if existing.is_empty() {
            return Err(anyhow!("cid not found: {cid}"));
        }
        let subject = cid_iri(cid)?;
        let quads = facts.into_quads(&subject, &self.cfg.prefixes, &self.cfg.default_prefix)?;
        for q in &quads {
            self.store.insert(q.as_ref()).context("insert quad")?;
        }
        self.restamp(cid)?;
        Ok(())
    }

    /// Delete a cid: triples (subject + its sub-subjects), blob file, vector
    /// sidecars, and index entries.
    pub fn delete(&self, cid: &str) -> Result<()> {
        let facts = self.facts_for(cid)?;
        if facts.is_empty() {
            return Err(anyhow!("cid not found: {cid}"));
        }
        let subject = cid_iri(cid)?;

        // Blob file first (facts still know where it is).
        if let Some(blob_path) = facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}blobPath"))
            .and_then(|(_, v)| v.first())
        {
            let abs = self.layout.storage_root.join(blob_path);
            if abs.exists() {
                fs::remove_file(&abs).with_context(|| format!("remove blob {}", abs.display()))?;
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
        let bare = cid.strip_prefix("urn:").unwrap_or(cid);
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
                // knows this cid, using the sidecar's dim.
                let keymap_path = self.layout.hnsw_root.join(&name).join("keymap.json");
                let known = fs::read_to_string(&keymap_path)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<HashMap<String, u64>>(&raw).ok())
                    .map(|m| m.contains_key(bare))
                    .unwrap_or(false);
                if !known {
                    continue;
                }
                let sidecar = self.layout.vector_sidecar_path(&name, bare);
                let dim = npy::read_f32_1d(&sidecar).map(|v| v.len()).unwrap_or(0);
                if dim == 0 {
                    continue; // can't load without a dim; index entry becomes stale but harmless
                }
                indexes.insert(name.clone(), VectorIndex::create(&self.layout.hnsw_root, &name, dim)?);
            }
            if let Some(vi) = indexes.get_mut(&name) {
                if let Some(key) = vi.cid_to_key.remove(bare) {
                    vi.key_to_cid.remove(&key);
                    vi.index.remove(key).ok();
                    vi.dirty = true;
                }
            }
            let sidecar = self.layout.vector_sidecar_path(&name, bare);
            if sidecar.exists() {
                fs::remove_file(&sidecar).ok();
            }
        }
        Ok(())
    }

    // ── Vectors + search (the crown jewel, lifted from Pool) ────────────────

    /// Attach a vector to an existing cid: writes the raw `.npy` sidecar
    /// (media-derived, follows storage) and adds to the named HNSW index.
    ///
    /// **Idempotent**: if `cid` is already indexed in `index_name`, no-op and
    /// `Ok(false)`. `Ok(true)` when a new entry was added.
    pub fn add_vector(&self, cid: &str, index_name: &str, vec: &[f32]) -> Result<bool> {
        let bare = cid.strip_prefix("urn:").unwrap_or(cid);
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

        // Idempotency gate: usearch throws on duplicate-key insert; same CID =
        // same vector, no work to do.
        if vi.cid_to_key.contains_key(bare) {
            return Ok(false);
        }

        // Raw sidecar first (reembed/migration source of truth for the vector).
        npy::write_f32_1d(&self.layout.vector_sidecar_path(index_name, bare), vec)?;

        let key = vi.next_key;
        vi.next_key += 1;
        vi.cid_to_key.insert(bare.to_string(), key);
        vi.key_to_cid.insert(key, bare.to_string());

        let capacity_needed = vi.cid_to_key.len();
        if vi.index.capacity() < capacity_needed {
            vi.index.reserve(capacity_needed.max(1024))?;
        }
        vi.index
            .add(key, vec)
            .map_err(|e| anyhow!("usearch add (cid {}, index {}): {}", bare, index_name, e))?;
        vi.dirty = true;
        Ok(true)
    }

    /// Whether `cid` is already present in `index_name`.
    pub fn contains_cid(&self, cid: &str, index_name: &str) -> bool {
        let bare = cid.strip_prefix("urn:").unwrap_or(cid);
        let indexes = self.indexes.lock().unwrap();
        indexes
            .get(index_name)
            .map(|vi| vi.cid_to_key.contains_key(bare))
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
                        count: vi.cid_to_key.len(),
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
    /// `?s`, which binds `?cid` via `pan:cid`) gates the candidate set; usearch
    /// kNN ranks by cosine similarity to `like`. Strategy A: pre-filter then
    /// search, joined at the application layer by the CID↔key map — no custom
    /// SPARQL UDF. Lifted from Pool with one simplification: facts live in the
    /// default graph, so there is no GRAPH wrapper to splice into.
    ///
    /// An empty `where_clause` means "no graph gate" — pure kNN over the index.
    pub fn search(&self, where_clause: &str, like: &[f32], k: usize, index_name: &str) -> Result<Vec<SearchHit>> {
        validate_index_name(index_name)?;
        // Same prologue as query() so a gate can use rdf:/rdfs:/owl:/xsd: and
        // every configured app prefix — no surprise "unknown prefix" between
        // the two SPARQL paths.
        let q = format!(
            "{}
             SELECT DISTINCT ?cid WHERE {{
               ?s pan:cid ?cid .
               {where_clause}
             }}",
            self.prefix_prologue()
        );
        let mut candidate_cids: HashSet<String> = HashSet::new();
        if let QueryResults::Solutions(sols) = self.store.query(&q).map_err(|e| anyhow!("search where-clause: {e}"))? {
            for s in sols {
                let s = s?;
                if let Some(t) = s.get("cid") {
                    // pan:cid is stored as the bare literal, but guard the urn:
                    // form anyway (Pool's lesson: robust to either wire shape).
                    let raw = term_str(t);
                    let cid = raw.strip_prefix("urn:").unwrap_or(&raw).to_string();
                    candidate_cids.insert(cid);
                }
            }
        }

        if candidate_cids.is_empty() {
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

        let candidate_keys: HashSet<u64> = candidate_cids
            .iter()
            .filter_map(|c| vi.cid_to_key.get(c).copied())
            .collect();

        if candidate_keys.is_empty() {
            return Ok(vec![]);
        }

        // Adaptive search breadth from prefilter selectivity (ported verbatim —
        // non-obvious quality tuning).
        let total = vi.cid_to_key.len() as f32;
        let selectivity = (candidate_keys.len() as f32 / total).max(0.001);
        let ef = ((k as f32 / selectivity).clamp(64.0, 4096.0)) as usize;
        vi.index.change_expansion_search(ef);

        let matches = vi.index.filtered_search(like, k, |key| candidate_keys.contains(&key))?;

        let mut hits = Vec::with_capacity(matches.keys.len());
        for (key, distance) in matches.keys.iter().zip(matches.distances.iter()) {
            if let Some(cid) = vi.key_to_cid.get(key) {
                hits.push(SearchHit {
                    cid: cid.clone(),
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
    pub fn restamp(&self, cid: &str) -> Result<()> {
        let facts = self.facts_for(cid)?;
        let blob_path = match facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}blobPath"))
            .and_then(|(_, v)| v.first())
        {
            Some(p) => p.clone(),
            None => return Err(anyhow!("cid not found: {cid}")),
        };
        let abs = self.layout.storage_root.join(&blob_path);
        let bytes = fs::read(&abs).with_context(|| format!("read blob {}", abs.display()))?;
        if !xmp::is_png(&bytes) {
            return Ok(()); // non-PNG media has no XMP mirror (v1)
        }
        let created_at = facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}createdAt"))
            .and_then(|(_, v)| v.first().cloned())
            .unwrap_or_default();
        let packet = self.build_packet_from_graph(cid, &blob_path, &created_at)?;
        let stamped = xmp::write_packet_into_png_bytes(&bytes, &packet)?;
        fs::write(&abs, &stamped).with_context(|| format!("write blob {}", abs.display()))?;
        Ok(())
    }

    /// Graph facts → XMP packet. Facts group into app blocks by reverse prefix
    /// lookup; multi-value predicates become Bags; sub-subjects (`<subj>/…`)
    /// re-author as their own Descriptions.
    fn build_packet_from_graph(&self, cid: &str, blob_path: &str, created_at: &str) -> Result<String> {
        let subject = cid_iri(cid)?;
        let subj_prefix = format!("{}/", subject.as_str());

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

        // Root facts → app blocks (pan: identity fields re-authored, not copied).
        let mut app_fields: HashMap<(String, String), Vec<(String, xmp::FieldValue)>> = HashMap::new();
        for (pred, values) in self.facts_for(cid)? {
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

        // Sub-subjects: named subjects scoped under this cid's subject IRI.
        // Keep object term-type so rdf:type (and other IRI objects) re-author
        // as `rdf:resource` on the way out, not as string literals.
        let rdf_type_iri = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let mut sub_facts: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
        for quad in self.store.iter() {
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

        Ok(xmp::build_packet(cid, blob_path, created_at, &app_blocks, &sub_blocks))
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
