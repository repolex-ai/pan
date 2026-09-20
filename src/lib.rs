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
//! - Identity is `pan:id`, the Thing's IRI, spelled pan: in the graph as in
//!   the file (the universal id by owl:equivalentProperty; goodlux, 2026-09-17)
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
pub use oxigraph::model::Term;
use oxigraph::model::{GraphName, Literal, NamedNode, NamedOrBlankNode, Quad};
use oxigraph::sparql::SparqlEvaluator;
pub use oxigraph::sparql::{QueryResults, QuerySolution};
use oxigraph::store::Store;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

pub mod config;
pub mod convert;
pub mod daemon;
pub mod depth;
pub mod enrich;
pub mod facts;
pub mod imageset;
pub mod instance;
pub mod layout;
pub mod npy;
pub mod pngchunk;
pub mod thumbnail;
pub mod wire;
pub mod xmp;

/// Take a mutex, recovering if a previous holder panicked.
///
/// A poisoned mutex means some thread panicked while holding it. The default
/// `lock().unwrap()` turns that one panic into a panic on every later lock, so
/// one bad stage answer would take the whole daemon down. Every mutex in pand
/// guards state that stays usable after a panic — in-memory vector-index
/// handles, the per-stage attempt map, a hold map, an open log file — and
/// pand is the single writer, so nothing else can have half-applied a change.
/// Recover the guard, say so once per call site in the log, and carry on.
/// (m4rq's rlex audit of pan, 2026-09-17.)
pub fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| {
        tracing::warn!(
            "a mutex was poisoned by an earlier panic; recovering its guard and continuing"
        );
        poisoned.into_inner()
    })
}

pub use config::{now_local, PanConfig, GIT_LEX_NS, PAN_MEDIA_NS, PAN_NS};
pub use facts::Facts;
pub use imageset::ImageSet;
pub use layout::PanLayout;

/// The Pan base ontology, shipped with the binary; NOT loaded into the media graph.
pub const PAN_ONTOLOGY_TTL: &str = include_str!("../ontology/pan.ttl");

/// The `owl:versionInfo` of the compiled-in ontology ("0.3.5"), or "?" if the
/// header ever loses it.
pub fn ontology_version() -> &'static str {
    PAN_ONTOLOGY_TTL
        .split("owl:versionInfo")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .unwrap_or("?")
}

/// Write the ontology this binary was built with to `<dir>/pan.ttl` so other
/// systems on the machine can read Pan's vocabulary (goodlux, 2026-09-16: the
/// machine-wide config directory, not a store — one machine holds many stores
/// and none of them is the ontology's home). Rewritten at every start, so the
/// file always matches the running pand. Returns the path written.
pub fn write_ontology_copy(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("pan.ttl");
    write_atomic(&path, PAN_ONTOLOGY_TTL.as_bytes())?;
    Ok(path)
}

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
/// Every spelling of the universal identity a Pan node may have been given
/// by an earlier binary. Pan writes only `pan:id` (goodlux, 2026-09-17); the
/// other two are `owl:equivalentProperty` bridges in pan.ttl and are what a
/// store from before that ruling still carries.
const IDENTITY_PREDICATES: [&str; 3] = [
    "https://repolex.ai/ontology/pan/id",
    "https://repolex.ai/ontology/git-lex/id",
    "https://repolex.ai/ontology/subtexture/id",
];

/// The angle-bracket form of a pan identity, as it appears everywhere a
/// person or another tool sees it: `<pan/Image/k7m2p9x4>` — the same notation
/// git-lex uses for every other Thing.
pub fn bracket_iri(iri: &str) -> String {
    match iri.strip_prefix("https://repolex.ai/") {
        Some(rest) => format!("<{rest}>"),
        None => iri.to_string(),
    }
}

/// Write a file so that no reader ever sees it half done: bytes go to
/// `.<name>.partial` beside the target, then one `rename` puts it in place
/// (atomic on the same volume). A viewer watching the folder as images land
/// (Xee³, 2026-09-04) was reading a 3 MB PNG mid-write and showing Horae's
/// block, which comes first in the packet, without Pan's, which comes last.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("write_atomic: no file name in {}", path.display()))?;
    let tmp = path.with_file_name(format!(".{name}.partial"));
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename {} → {}", tmp.display(), path.display()));
    }
    Ok(())
}

/// The inverse of [`bracket_iri`]: the git-lex reference form `<ns/Class/id>`
/// (how frontmatter — and a producer's XMP field — writes a reference) to the
/// full IRI `https://repolex.ai/ns/Class/id`. None if the text is not that
/// form: no brackets, fewer than three segments, or whitespace inside.
pub fn iri_from_bracket(text: &str) -> Option<String> {
    let inner = text.trim().strip_prefix('<')?.strip_suffix('>')?;
    if inner.is_empty() || inner.chars().any(char::is_whitespace) || inner.starts_with('/') {
        return None;
    }
    if inner.split('/').filter(|s| !s.is_empty()).count() < 3 {
        return None;
    }
    Some(format!("https://repolex.ai/{inner}"))
}

/// Accept an identity in any form a caller hands over — `<pan/Image/x>`, the
/// full IRI, or the bare id — and return the bare id.
pub fn bare_id(given: &str) -> String {
    let s = given.trim();
    let s = s
        .strip_prefix('<')
        .and_then(|r| r.strip_suffix('>'))
        .unwrap_or(s);
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
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
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
    NamedNode::new(format!("{PAN_MEDIA_NS}{}/{id}", media_class(media_type)))
        .map_err(|e| anyhow!("invalid media IRI: {e}"))
}

pub(crate) fn pan_iri(local: &str) -> NamedNode {
    NamedNode::new(format!("{PAN_NS}{local}")).expect("valid pan IRI")
}

pub(crate) fn rdf_type() -> NamedNode {
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
            let path_text = path.to_str().ok_or_else(|| {
                anyhow!(
                    "vector index path {} is not valid UTF-8; the index library needs a UTF-8 path",
                    path.display()
                )
            })?;
            index.load(path_text)?;
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
        let path_text = self.path.to_str().ok_or_else(|| {
            anyhow!(
                "vector index path {} is not valid UTF-8; the index library needs a UTF-8 path",
                self.path.display()
            )
        })?;
        self.index.save(path_text)?;
        let dir = self.path.parent().ok_or_else(|| {
            anyhow!(
                "vector index path {} has no parent directory to hold keymap.json",
                self.path.display()
            )
        })?;
        let map_path = dir.join("keymap.json");
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
    /// The full IRI written for this object (`pan:id`).
    pub iri: String,
    pub media_path: String,
    /// Where the bytes as delivered were kept, when the arrival was not PNG
    /// and was converted (`img/original/…`). None = the arrival was stored as is.
    pub original_path: Option<String>,
    pub created_date: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// False = the bytes could not be decoded as an image (still stored).
    pub thumbnail: bool,
    /// Statements read from the XMP the file arrived with (a producer's copia
    /// block, an Adobe block, …) and loaded into the graph.
    pub statements: usize,
}

/// How much of each kind a store holds, read from the graph alone: the
/// number of images, and for each derived kind the number of images that
/// have at least one record of it. `pending_*` is images minus that.
/// The scene fields of pan.ttl 0.3.4, in the order the file writes them.
/// A JSON key from the caption model must be one of these, a caption, or
/// sceneObjects; anything else is refused (the ontology is the whole of what
/// Pan may say). The test below checks every name here against pan.ttl.
pub const SCENE_FIELDS: [&str; 13] = [
    "sceneCamera",
    "sceneFraming",
    "scenePosture",
    "sceneGaze",
    "sceneExpression",
    "sceneAction",
    "sceneEnergy",
    "sceneMood",
    "sceneLighting",
    "sceneStyle",
    "sceneMedium",
    "sceneLocation",
    "sceneSubjectOrientation",
];

/// Every property the caption stage writes on the object.
pub const PERCEPTION_FIELDS: [&str; 17] = [
    "shortCaption",
    "longCaption",
    "sceneObjects",
    "sceneCamera",
    "sceneFraming",
    "scenePosture",
    "sceneGaze",
    "sceneExpression",
    "sceneAction",
    "sceneEnergy",
    "sceneMood",
    "sceneLighting",
    "sceneStyle",
    "sceneMedium",
    "sceneLocation",
    "sceneSubjectOrientation",
    // The prompt that produced the captions riding on this object
    // (goodlux, 2026-09-19). Written by the caption stage, not by the model.
    "modelPromptPath",
];

/// Fields Pan itself writes about a media object at ingest or at stage
/// completion. A person may never set these by hand.
pub const STRUCTURAL_FIELDS: [&str; 7] = [
    "mediaPath",
    "mediaType",
    "sourceFile",
    "width",
    "height",
    "createdDate",
    "readyDate",
];

/// One property a person may set on a media object: its local name and the
/// datatype the ontology declares for it (`xsd:integer`, `xsd:boolean`,
/// `xsd:string`, `xsd:dateTime`, `xsd:decimal`, or a bounded datatype such
/// as `pan:RatingValue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettableField {
    pub local: String,
    pub range: String,
}

/// The properties a person may set with `pan set`, read from the compiled
/// ontology: every `owl:DatatypeProperty` whose domain is pan:Media or
/// pan:Image, minus the fields the caption stage owns (PERCEPTION_FIELDS)
/// and the fields Pan itself writes (STRUCTURAL_FIELDS). Today that is
/// rating, isPicked, isRejected (pan.ttl 0.3.8, goodlux 2026-09-16). A new
/// settable field is declared in pan.ttl, never added here.
pub fn settable_fields() -> Vec<SettableField> {
    let mut out = Vec::new();
    for chunk in PAN_ONTOLOGY_TTL.split("\npan:").skip(1) {
        let Some(name_end) = chunk.find(' ') else {
            continue;
        };
        let local = &chunk[..name_end];
        let rest = &chunk[name_end..];
        if !rest.starts_with(" a owl:DatatypeProperty") {
            continue;
        }
        let block = match rest.find(" .\n") {
            Some(e) => &rest[..e],
            None => rest,
        };
        let token_after = |key: &str| -> Option<&str> {
            let k = block.find(key)? + key.len();
            block[k..]
                .split(|c: char| c.is_whitespace() || c == ';')
                .next()
                .filter(|t| !t.is_empty())
        };
        let domain = token_after("rdfs:domain ").unwrap_or("");
        if domain != "pan:Media" && domain != "pan:Image" {
            continue;
        }
        if PERCEPTION_FIELDS.contains(&local) || STRUCTURAL_FIELDS.contains(&local) {
            continue;
        }
        out.push(SettableField {
            local: local.to_string(),
            range: token_after("rdfs:range ")
                .unwrap_or("xsd:string")
                .to_string(),
        });
    }
    out.sort_by(|a, b| a.local.cmp(&b.local));
    out
}

/// The inclusive bounds a bounded integer datatype declares in pan.ttl
/// (`owl:withRestrictions ( [ xsd:minInclusive 0 ] [ xsd:maxInclusive 5 ] )`).
fn integer_bounds(datatype_local: &str) -> Option<(i64, i64)> {
    let start = PAN_ONTOLOGY_TTL.find(&format!("\npan:{datatype_local} a rdfs:Datatype"))?;
    let block = &PAN_ONTOLOGY_TTL[start..];
    let block = &block[..block.find(" .\n").unwrap_or(block.len())];
    let num = |key: &str| -> Option<i64> {
        let k = block.find(key)? + key.len();
        block[k..]
            .split(|c: char| !c.is_ascii_digit() && c != '-')
            .find(|t| !t.is_empty())?
            .parse()
            .ok()
    };
    Some((num("xsd:minInclusive ")?, num("xsd:maxInclusive ")?))
}

const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";

/// Turn a JSON value into the RDF literal the declared range asks for, or say
/// plainly what was expected. A JSON string holding a number or `true`/`false`
/// is accepted, so the command line can pass everything as text.
fn literal_for(
    field: &SettableField,
    value: &serde_json::Value,
) -> std::result::Result<Literal, String> {
    let as_text = || match value {
        serde_json::Value::String(s) => s.trim().to_string(),
        other => other.to_string(),
    };
    let typed = |v: String, dt: &str| {
        Literal::new_typed_literal(v, NamedNode::new_unchecked(format!("{XSD_NS}{dt}")))
    };
    match field.range.as_str() {
        "xsd:integer" => match as_text().parse::<i64>() {
            Ok(n) => Ok(typed(n.to_string(), "integer")),
            Err(_) => Err(format!(
                "{} expects a whole number, got {value}",
                field.local
            )),
        },
        "xsd:boolean" => match as_text().as_str() {
            "true" => Ok(typed("true".into(), "boolean")),
            "false" => Ok(typed("false".into(), "boolean")),
            _ => Err(format!(
                "{} expects true or false, got {value}",
                field.local
            )),
        },
        "xsd:decimal" => match as_text().parse::<f64>() {
            Ok(_) => Ok(typed(as_text(), "decimal")),
            Err(_) => Err(format!("{} expects a number, got {value}", field.local)),
        },
        "xsd:dateTime" => match value {
            serde_json::Value::String(s) if !s.trim().is_empty() => {
                Ok(typed(s.trim().to_string(), "dateTime"))
            }
            _ => Err(format!(
                "{} expects an RFC3339 date-time string, got {value}",
                field.local
            )),
        },
        "xsd:string" => match value {
            serde_json::Value::String(s) => Ok(Literal::new_simple_literal(s.as_str())),
            _ => Err(format!("{} expects text, got {value}", field.local)),
        },
        other => {
            // A pan-declared bounded datatype, e.g. pan:RatingValue.
            let local = other.strip_prefix("pan:").unwrap_or(other);
            match integer_bounds(local) {
                Some((lo, hi)) => match as_text().parse::<i64>() {
                    Ok(n) if (lo..=hi).contains(&n) => Ok(typed(n.to_string(), "integer")),
                    Ok(n) => Err(format!(
                        "{} expects a whole number from {lo} to {hi}, got {n}",
                        field.local
                    )),
                    Err(_) => Err(format!(
                        "{} expects a whole number from {lo} to {hi}, got {value}",
                        field.local
                    )),
                },
                None => Err(format!(
                    "{} has range {other}, which pan set does not know how to write",
                    field.local
                )),
            }
        }
    }
}

fn not_settable(local: &str) -> anyhow::Error {
    let names: Vec<String> = settable_fields().into_iter().map(|f| f.local).collect();
    anyhow!(
        "{local} is not a property a person may set; settable: {}",
        names.join(", ")
    )
}

/// What the caption stage learned about one object: the model's JSON answer,
/// checked against the vocabulary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Perception {
    pub short_caption: String,
    pub long_caption: String,
    pub scene_objects: Vec<String>,
    pub scene: Vec<(String, String)>,
    /// The prompt file that asked for this answer, relative to the prompts
    /// directory (goodlux, 2026-09-19). pand fills it in; the model never
    /// sends it. Rides on the object so an image says which prompt described
    /// it, and on the Caption record beside it.
    pub prompt_path: String,
}

impl Perception {
    /// Parse the caption model's answer: one JSON object (a ```json fence
    /// around it is tolerated) whose keys are property names. Unknown keys
    /// are an error naming the key — the prompt is the schema and the
    /// ontology is the law; nothing undeclared is stored.
    pub fn parse(answer: &str) -> std::result::Result<Self, String> {
        let s = answer.trim();
        let start = s.find('{').ok_or("answer has no JSON object")?;
        let end = s.rfind('}').ok_or("answer has no JSON object")?;
        if end < start {
            return Err("answer has no JSON object".into());
        }
        let v: serde_json::Value = serde_json::from_str(&s[start..=end])
            .map_err(|e| format!("answer is not valid JSON: {e}"))?;
        let serde_json::Value::Object(m) = v else {
            return Err("answer is not a JSON object".into());
        };
        let mut out = Perception::default();
        for (k, v) in &m {
            match k.as_str() {
                "shortCaption" => {
                    out.short_caption = v.as_str().unwrap_or_default().trim().to_string()
                }
                "longCaption" => {
                    out.long_caption = v.as_str().unwrap_or_default().trim().to_string()
                }
                "sceneObjects" => {
                    let items: Vec<String> = match v {
                        serde_json::Value::Array(a) => a
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(str::to_string)
                            .collect(),
                        serde_json::Value::String(s) => s.split(',').map(str::to_string).collect(),
                        _ => return Err("sceneObjects must be a list of strings".into()),
                    };
                    for raw in items {
                        let n = raw
                            .trim()
                            .trim_matches(|c: char| c == '.' || c == ';')
                            .trim()
                            .to_lowercase();
                        if !n.is_empty() && n.len() <= 40 && !out.scene_objects.contains(&n) {
                            out.scene_objects.push(n);
                        }
                    }
                }
                other if SCENE_FIELDS.contains(&other) => {
                    let val = match v {
                        serde_json::Value::String(s) => s.trim().to_string(),
                        serde_json::Value::Null => String::new(),
                        x => x.to_string(),
                    };
                    if !val.is_empty() && !val.eq_ignore_ascii_case("N/A") {
                        out.scene.push((other.to_string(), val));
                    }
                }
                other => {
                    return Err(format!(
                        "answer has a key the Pan ontology does not declare: {other}"
                    ))
                }
            }
        }
        if out.short_caption.is_empty() || out.long_caption.is_empty() {
            return Err("answer is missing shortCaption or longCaption".into());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod ontology_copy_tests {
    use super::*;

    #[test]
    fn writes_the_compiled_ontology_verbatim() {
        let dir = std::env::temp_dir().join(format!("pan-ontology-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = write_ontology_copy(&dir.join("ontology")).unwrap();
        assert_eq!(path, dir.join("ontology").join("pan.ttl"));
        assert_eq!(fs::read_to_string(&path).unwrap(), PAN_ONTOLOGY_TTL);
        assert!(!ontology_version().is_empty() && ontology_version() != "?");
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod perception_tests {
    use super::*;

    #[test]
    fn every_perception_field_is_declared_in_the_ontology() {
        for f in PERCEPTION_FIELDS {
            assert!(
                PAN_ONTOLOGY_TTL.contains(&format!("\npan:{f} a owl:DatatypeProperty")),
                "pan:{f} is not declared in pan.ttl"
            );
        }
    }

    #[test]
    fn imageset_vocabulary_is_declared_in_the_ontology() {
        // pan.ttl 0.4.2 (goodlux, 2026-09-16): the set class under
        // subtexture:Set and its description. pan:member and pan:inPhotoset
        // are gone: membership is pan:relatedToId from the image to the set.
        // pan.ttl 0.4.7 (goodlux, 2026-09-17): Photoset is renamed ImageSet,
        // under a pan:MediaSet parent that has no instances yet.
        for decl in [
            "\npan:MediaSet a owl:Class",
            "\npan:ImageSet a owl:Class",
            "\npan:description a owl:DatatypeProperty",
        ] {
            assert!(
                PAN_ONTOLOGY_TTL.contains(decl),
                "missing in pan.ttl: {decl}"
            );
        }
    }

    #[test]
    fn instance_vocabulary_is_declared_in_the_ontology() {
        // pan.ttl 0.4.1 (goodlux, 2026-09-16): the deployment node and its
        // six properties, declared ahead of the code that will write them.
        for decl in [
            "\npan:Instance a owl:Class ;\n    rdfs:subClassOf pan:Node",
            "\npan:primaryGraph a owl:DatatypeProperty",
            "\npan:localGraph a owl:DatatypeProperty",
            "\npan:fsRoot a owl:DatatypeProperty",
            "\npan:sourceFormat a owl:DatatypeProperty",
            "\npan:instanceMode a owl:DatatypeProperty",
            "\npan:listenPort a owl:DatatypeProperty",
        ] {
            assert!(
                PAN_ONTOLOGY_TTL.contains(decl),
                "missing in pan.ttl: {decl}"
            );
        }
    }

    #[test]
    fn settable_fields_are_exactly_the_curation_fields() {
        let names: Vec<String> = settable_fields().into_iter().map(|f| f.local).collect();
        assert_eq!(
            names,
            ["isPicked", "isRejected", "rating"],
            "pan.ttl declares a new person-settable field: extend pan set's docs and this test"
        );
        let rating = settable_fields()
            .into_iter()
            .find(|f| f.local == "rating")
            .unwrap();
        assert_eq!(rating.range, "pan:RatingValue");
        assert_eq!(integer_bounds("RatingValue"), Some((0, 5)));
    }

    #[test]
    fn values_are_checked_against_the_declared_range() {
        let rating = SettableField {
            local: "rating".into(),
            range: "pan:RatingValue".into(),
        };
        assert_eq!(
            literal_for(&rating, &serde_json::json!(4)).unwrap().value(),
            "4"
        );
        assert_eq!(
            literal_for(&rating, &serde_json::json!("3"))
                .unwrap()
                .value(),
            "3"
        );
        assert!(literal_for(&rating, &serde_json::json!(6))
            .unwrap_err()
            .contains("0 to 5"));
        let picked = SettableField {
            local: "isPicked".into(),
            range: "xsd:boolean".into(),
        };
        assert_eq!(
            literal_for(&picked, &serde_json::json!(true))
                .unwrap()
                .value(),
            "true"
        );
        assert!(literal_for(&picked, &serde_json::json!("yes"))
            .unwrap_err()
            .contains("true or false"));
    }

    #[test]
    fn parses_the_answer_and_refuses_undeclared_keys() {
        let p = Perception::parse("```json\n{\"shortCaption\": \"A wolf.\", \"longCaption\": \"A grey wolf on a ridge.\", \"sceneObjects\": [\"Wolf\", \"rock\", \"wolf\", \"\"], \"sceneMood\": \"still\", \"sceneGaze\": null}\n```").unwrap();
        assert_eq!(p.scene_objects, ["wolf", "rock"]);
        assert_eq!(p.scene, [("sceneMood".to_string(), "still".to_string())]);
        let e =
            Perception::parse("{\"shortCaption\": \"x\", \"longCaption\": \"y\", \"vibe\": \"z\"}")
                .unwrap_err();
        assert!(e.contains("vibe"), "{e}");
        assert!(Perception::parse("{\"shortCaption\": \"x\"}").is_err());
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StoreCounts {
    pub images: u64,
    pub thumbnails: u64,
    pub captions: u64,
    pub embeddings: u64,
    pub poses: u64,
    pub regions: u64,
    pub depths: u64,
}

/// The facts of one node as `pan info` shows them: one entry per predicate
/// IRI with every value it carries, sorted by predicate.
pub type NodeFacts = Vec<(String, Vec<String>)>;

/// What exists for one media object, read from the graph alone.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaState {
    pub id: String,
    pub iri: String,
    pub media_type: String,
    pub created_date: String,
    pub ready_date: Option<String>,
    pub thumbnail: bool,
    /// enrichment reference (vectorData / captionData / regionData / poseData / depthData)
    /// → models that have run on this object (a run with nothing found counts).
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
    /// The store's own identity: six characters, the start of a soul's genesis
    /// SHA or of a bare store id (goodlux, 2026-09-18).
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
        fs::create_dir_all(root)
            .with_context(|| format!("create store root {}", root.display()))?;
        let cfg = PanConfig::load(root)?;
        let layout = PanLayout::resolve(root, media_root);
        fs::create_dir_all(&layout.oxigraph_root).context("create oxigraph root")?;
        fs::create_dir_all(&layout.hnsw_root).context("create hnsw root")?;
        fs::create_dir_all(&layout.media_root)
            .with_context(|| format!("create media root {}", layout.media_root.display()))?;
        let store = Store::open(&layout.oxigraph_root)
            .with_context(|| format!("open oxigraph at {}", layout.oxigraph_root.display()))?;
        let pan = Pan {
            cfg,
            layout,
            store_id: store_id.to_string(),
            store,
            indexes: Mutex::new(HashMap::new()),
        };
        pan.declare_store()?;
        // The sets a person curated live in imagesets/*.xml; the graph is
        // rebuilt from them on every open, so the files are the truth.
        let sets = pan.load_imagesets()?;
        if sets > 0 {
            tracing::info!(store = %store_id, imagesets = sets, "imagesets loaded from files");
        }
        Ok(pan)
    }

    /// Forget every embedding in this store — the records, the reference
    /// bags, the vector sidecars, the search index — and rewrite the XMP of
    /// each image that had one. Pending means absent, so the embed stage
    /// refills them on its next pass. Used when the vectors are to be remade
    /// (Rob, 2026-09-07: the ones so far are test data; staying on the 2B).
    pub fn wipe_embeddings(&self) -> Result<usize> {
        let ids: Vec<String> =
            match self.query("SELECT DISTINCT ?s WHERE { ?s pan:vectorData ?v }")? {
                QueryResults::Solutions(sols) => sols
                    .filter_map(|r| r.ok())
                    .filter_map(|r| r.get("s").map(term_str))
                    .map(|iri| bare_id(&iri))
                    .collect(),
                _ => Vec::new(),
            };
        let up = format!(
            "PREFIX pan: <{PAN_NS}>\n\
             DELETE {{ ?v pan:item ?e . ?e ?p ?o }} WHERE {{ ?s pan:vectorData ?v . ?v pan:item ?e . ?e ?p ?o }} ;\n\
             DELETE {{ ?s pan:vectorData ?v . ?v ?p ?o }} WHERE {{ ?s pan:vectorData ?v . ?v ?p ?o }}"
        );
        self.store
            .update(&up)
            .map_err(|e| anyhow!("wipe embeddings: {e}"))?;
        locked(&self.indexes).clear();
        if self.layout.hnsw_root.exists() {
            fs::remove_dir_all(&self.layout.hnsw_root)
                .with_context(|| format!("remove {}", self.layout.hnsw_root.display()))?;
        }
        if let Ok(kinds) = fs::read_dir(&self.layout.media_root) {
            for k in kinds.filter_map(|e| e.ok()) {
                let v = k
                    .path()
                    .join(PanLayout::DATA_SUBDIR)
                    .join(PanLayout::VECTORS_SUBDIR);
                if v.is_dir() {
                    fs::remove_dir_all(&v).with_context(|| format!("remove {}", v.display()))?;
                }
            }
        }
        for id in &ids {
            if let Err(e) = self.restamp(id) {
                tracing::warn!(store = %self.store_id, id = %id, "restamp after embedding wipe: {e:#}");
            }
        }
        tracing::info!(store = %self.store_id, images = ids.len(), "embeddings wiped; the embed stage refills them");
        Ok(ids.len())
    }

    /// The store node `<pan/Store/<id>>`: type, identity, media root. Replaces
    /// a stale media root (the volume moved) rather than adding a second one.
    fn declare_store(&self) -> Result<()> {
        let node = NamedNode::new(format!("{PAN_MEDIA_NS}Store/{}", self.store_id))
            .map_err(|e| anyhow!("store IRI: {e}"))?;
        let media_root = self.layout.media_root.to_string_lossy().to_string();
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        // Rewrite, never accumulate: the media root from this config, and
        // the identity in this binary's spelling. A store opened by an older
        // binary carries `git-lex:id` on this node (pan issue #33); every
        // identity spelling — pan, git-lex, subtexture — is removed before
        // `pan:id` goes in, so the node has exactly one.
        let old: Vec<Quad> = self
            .store
            .quads_for_pattern(
                Some((&node).into()),
                None,
                None,
                Some(GraphName::DefaultGraph.as_ref()),
            )
            .filter_map(|q| q.ok())
            .filter(|q| {
                let p = q.predicate.as_str();
                p == pan_iri("mediaRoot").as_str() || IDENTITY_PREDICATES.contains(&p)
            })
            .collect();
        for q in &old {
            t.remove(q.as_ref());
        }
        // Exactly one store node, always. A store opened by a binary that used
        // the whole forty-character genesis hash left
        // `<pan/Store/700c5bd4a9…>` behind; the id is six characters now
        // (goodlux, 2026-09-18) and every store node that is not this one is
        // removed whole, not left as a second answer to "where is the media".
        let stale: Vec<Quad> = self
            .store
            .quads_for_pattern(None, None, None, Some(GraphName::DefaultGraph.as_ref()))
            .filter_map(|q| q.ok())
            .filter(|q| match &q.subject {
                NamedOrBlankNode::NamedNode(n) => {
                    n.as_str().starts_with(&format!("{PAN_MEDIA_NS}Store/"))
                        && n.as_str() != node.as_str()
                }
                _ => false,
            })
            .collect();
        for q in &stale {
            tracing::warn!(store = %self.store_id, subject = %q.subject, "store node from an older id removed");
            t.remove(q.as_ref());
        }
        t.insert(
            Quad::new(
                node.clone(),
                rdf_type(),
                pan_iri("Store"),
                GraphName::DefaultGraph,
            )
            .as_ref(),
        );
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
    /// itself (`pan:id`), so the lookup is: does `<pan/Image/id>` (or
    /// `<pan/Media/id>`) have a type in this store.
    pub fn subject_for(&self, id: &str) -> Result<Option<NamedNode>> {
        if validate_pan_id(id).is_err() {
            return Ok(None);
        }
        for class in ["Image", "Media"] {
            let cand = NamedNode::new(format!("{PAN_MEDIA_NS}{class}/{id}"))
                .map_err(|e| anyhow!("candidate IRI: {e}"))?;
            let exists = self
                .store
                .quads_for_pattern(
                    Some((&cand).into()),
                    Some(rdf_type().as_ref()),
                    None,
                    Some(GraphName::DefaultGraph.as_ref()),
                )
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
    pub fn put(&self, arrived: &[u8], content_type: Option<&str>) -> Result<PutResult> {
        let arrived_png = xmp::is_png(arrived);
        let arrived_type = content_type.map(|s| s.to_string()).unwrap_or_else(|| {
            if arrived_png {
                "image/png".to_string()
            } else {
                "application/octet-stream".to_string()
            }
        });
        let arrived_ext = match arrived_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/tiff" => "tiff",
            _ => "bin",
        };

        // The managed store keeps ONE working format for images: PNG
        // (goodlux, 2026-09-16). A non-PNG image is decoded once and written
        // as PNG under img/source/; the bytes as delivered are kept under
        // img/original/ and never read again. Other media kinds are stored as
        // delivered. Metadata carry-over from the arrival is #23.
        let convert = arrived_type.starts_with("image/") && !arrived_png;
        let converted: Vec<u8>;
        let (bytes, media_type, ext): (&[u8], String, &str) = if convert {
            converted = convert::to_png(arrived)
                .with_context(|| format!("convert {arrived_type} arrival to PNG"))?;
            (&converted, "image/png".to_string(), "png")
        } else {
            (arrived, arrived_type.clone(), arrived_ext)
        };
        let png = xmp::is_png(bytes);

        let id = self.mint_pan_id()?;
        let subject = media_subject_iri(&media_type, &id)?;
        let created_date = now_local();
        let shard = created_date
            .get(0..10)
            .unwrap_or("0000-00-00")
            .replace('-', "/");
        let stem = PanLayout::file_stem(&created_date, &id);
        let kind = PanLayout::media_kind(&media_type);
        let rel_path = PanLayout::media_rel_path(kind, &shard, &stem, ext);
        let abs_path = self.layout.abs(&rel_path);
        // The arrival, kept beside the source when it was converted.
        let original_rel =
            convert.then(|| PanLayout::original_rel_path(kind, &shard, &stem, arrived_ext));
        // pan:sourceFile (pan.ttl 0.4.3, goodlux 2026-09-16): the file this
        // source was made from — the original when converted, the source
        // itself when it arrived as PNG. Always present; one rule.
        let source_file = original_rel.clone().unwrap_or_else(|| rel_path.clone());

        let mut quads = vec![
            Quad::new(
                subject.clone(),
                rdf_type(),
                pan_iri(media_class(&media_type)),
                GraphName::DefaultGraph,
            ),
            enrich::self_id_quad(&subject)?,
            self.quad(&subject, "mediaPath", &rel_path),
            // When it came to be: pan:createdDate, the same spelling the file
            // carries (goodlux, 2026-09-17: the graph stores pan:, never git-lex:).
            self.quad(&subject, "createdDate", &created_date),
            self.quad(&subject, "mediaType", &media_type),
            self.quad(&subject, "sourceFile", &source_file),
        ];

        // Whatever XMP the file arrived with is THE metadata (Rob, 2026-09-04:
        // a producer writes its block into the image before handing it over;
        // Pan receives a media file and nothing else). Its facts about the
        // image (rdf:about="") attach to this object; named subjects stay as
        // they are; datatypes survive. Read with the real RDF/XML parser, and
        // a packet that is not valid RDF/XML refuses the file — nothing is
        // stored that the graph cannot say.
        let existing_packet = if png {
            xmp::read_xmp_packet_from_bytes(bytes).context("read the XMP chunk in the file")?
        } else {
            None
        };
        let arrived_statements = match &existing_packet {
            Some(packet) => xmp::load_packet_statements(packet, subject.as_str())?,
            None => Vec::new(),
        };
        quads.extend(arrived_statements.iter().cloned());

        // Thumbnail — declared as its own node; not decodable = no thumbnail,
        // still stored, `pan state` says so.
        let mut thumb: Option<xmp::ThumbRef> = None;
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
                    let rel = PanLayout::thumbnail_rel_path(
                        kind,
                        &shard,
                        &stem,
                        thumbnail::THUMB_MAX_EDGE,
                    );
                    let tid = gen_pan_id();
                    let tnode = NamedNode::new(format!("{PAN_MEDIA_NS}Thumbnail/{tid}"))
                        .map_err(|e| anyhow!("thumbnail IRI: {e}"))?;
                    quads.push(Quad::new(
                        subject.clone(),
                        pan_iri("thumbnail"),
                        tnode.clone(),
                        GraphName::DefaultGraph,
                    ));
                    quads.push(Quad::new(
                        tnode.clone(),
                        rdf_type(),
                        pan_iri("Thumbnail"),
                        GraphName::DefaultGraph,
                    ));
                    quads.push(enrich::self_id_quad(&tnode)?);
                    quads.push(self.quad(&tnode, "path", &rel));
                    quads.push(self.quad(&tnode, "width", &t.width.to_string()));
                    quads.push(self.quad(&tnode, "height", &t.height.to_string()));
                    quads.push(self.quad(&tnode, "producedDate", &created_date));
                    thumb = Some(xmp::ThumbRef {
                        id: tid,
                        path: rel,
                        width: t.width,
                        height: t.height,
                        produced_date: created_date.clone(),
                    });
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
                let pan_desc =
                    xmp::build_pan_description(&self.image_packet_from(&scratch, &subject)?);
                let packet = xmp::compose_packet(existing_packet.as_deref(), &pan_desc);
                let written = xmp::write_packet_into_png_bytes(bytes, &packet)?;
                write_atomic(&abs_path, &written)
                    .with_context(|| format!("write media {}", abs_path.display()))?;
            } else {
                write_atomic(&abs_path, bytes)
                    .with_context(|| format!("write media {}", abs_path.display()))?;
            }
            if let Some(xmp::ThumbRef { path: rel, .. }) = &thumb {
                let tabs = self.layout.abs(rel);
                if let Some(parent) = tabs.parent() {
                    fs::create_dir_all(parent).context("create thumbnail shard dir")?;
                }
                write_atomic(&tabs, &thumb_jpeg)
                    .with_context(|| format!("write thumbnail {}", tabs.display()))?;
            }
            if let Some(rel) = &original_rel {
                let oabs = self.layout.abs(rel);
                if let Some(parent) = oabs.parent() {
                    fs::create_dir_all(parent).context("create original shard dir")?;
                }
                write_atomic(&oabs, arrived)
                    .with_context(|| format!("write original {}", oabs.display()))?;
            }
            self.insert_quads(&quads)?;
            Ok(())
        };
        if let Err(e) = land() {
            let _ = fs::remove_file(&abs_path);
            if let Some(xmp::ThumbRef { path: rel, .. }) = &thumb {
                let _ = fs::remove_file(self.layout.abs(rel));
            }
            if let Some(rel) = &original_rel {
                let _ = fs::remove_file(self.layout.abs(rel));
            }
            return Err(e);
        }

        Ok(PutResult {
            id,
            iri: subject.into_string(),
            media_path: rel_path,
            original_path: original_rel,
            created_date,
            width,
            height,
            thumbnail: thumb.is_some(),
            statements: arrived_statements.len(),
        })
    }

    /// Insert quads as ONE transaction: all land or none do.
    pub fn insert_quads(&self, quads: &[Quad]) -> Result<()> {
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        for q in quads {
            t.insert(q.as_ref());
        }
        t.commit().context("commit transaction")?;
        Ok(())
    }

    // ── read ──────────────────────────────────────────────────────────────────

    /// Media bytes + facts by id.
    pub fn get(&self, id: &str) -> Result<(Vec<u8>, NodeFacts)> {
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
    pub fn facts_for(&self, id: &str) -> Result<NodeFacts> {
        let Some(subject) = self.subject_for(id)? else {
            return Ok(vec![]);
        };
        Self::facts_of(&self.store, &subject)
    }

    fn facts_of(store: &Store, subject: &NamedNode) -> Result<NodeFacts> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for quad in store.quads_for_pattern(
            Some(subject.into()),
            None,
            None,
            Some(GraphName::DefaultGraph.as_ref()),
        ) {
            let quad = quad.context("read facts")?;
            map.entry(quad.predicate.as_str().to_string())
                .or_default()
                .push(term_str(&quad.object));
        }
        let mut out: Vec<_> = map.into_iter().collect();
        out.sort();
        Ok(out)
    }

    /// One `pan:` field of an arbitrary node by its IRI.
    pub fn node_field(&self, node_iri: &str, local: &str) -> Result<Option<String>> {
        let node =
            NamedNode::new(node_iri).map_err(|e| anyhow!("invalid node IRI {node_iri}: {e}"))?;
        let first = self
            .store
            .quads_for_pattern(
                Some((&node).into()),
                Some(pan_iri(local).as_ref()),
                None,
                Some(GraphName::DefaultGraph.as_ref()),
            )
            .next();
        match first {
            Some(q) => Ok(Some(term_str(&q.context("read node field")?.object))),
            None => Ok(None),
        }
    }

    /// What exists for one object, from the graph alone.
    pub fn state_for(&self, id: &str) -> Result<Option<MediaState>> {
        let Some(subject) = self.subject_for(id)? else {
            return Ok(None);
        };
        let facts = self.facts_for(id)?;
        let one = |local: &str| -> Option<String> {
            facts
                .iter()
                .find(|(p, _)| p == &format!("{PAN_NS}{local}"))
                .and_then(|(_, v)| v.first().cloned())
        };
        let mut enrichment = Vec::new();
        for link in [
            "vectorData",
            "captionData",
            "regionData",
            "poseData",
            depth::REF_LOCAL,
        ] {
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
            created_date: facts
                .iter()
                .find(|(p, _)| p == &format!("{PAN_NS}createdDate"))
                .and_then(|(_, v)| v.first().cloned())
                .unwrap_or_default(),
            ready_date: one("readyDate"),
            thumbnail: facts
                .iter()
                .any(|(p, _)| p == &format!("{PAN_NS}thumbnail")),
            enrichment,
        }))
    }

    /// Images with NO reference from `model` under `ref_local` (`regionData`,
    /// `poseData`, `captionData`, `vectorData`) — the stage engine's work
    /// list. The graph is the queue: pending means absent. The reference is
    /// written on every run, records or none, so a run that found nothing
    /// still retires the image.
    ///
    /// Newest first: new files take priority, and older images missing data
    /// are filled in whenever there is slack (Rob, 2026-09-05). Because
    /// pending means absent, the same query IS the backfill:
    /// once nothing new is waiting, the next batch is simply the newest of
    /// the old. No second queue, no second process.
    ///
    /// `since` is the backfill floor: an RFC 3339 local-offset date-time, the
    /// same shape `pan:createdDate` is written in, so a plain string compare
    /// is a time compare. Images created before it are not pending.
    pub fn pending_for(
        &self,
        ref_local: &str,
        model: &str,
        limit: usize,
        since: Option<&str>,
    ) -> Result<Vec<PendingItem>> {
        // What a stage needs before it can run (goodlux, 2026-09-08):
        // segmentation is prompted with the scene objects, and the embedding
        // is built from the image AND its XMP, so both wait for the caption
        // stage to have written its fields.
        // Each is an EXISTS test, never a join: joined, an image with N
        // scene objects came back N times and was handed to sam3 N times
        // before the first result landed (issue #29, 2026-09-17: four
        // references and 240 regions for 60). DISTINCT below is the second
        // lock on the same door.
        let needs = match ref_local {
            "regionData" => "FILTER EXISTS { ?s pan:sceneObjects ?obj }",
            "vectorData" => "FILTER EXISTS { ?s pan:longCaption ?ld }",
            _ => "",
        };
        let model_lit = model.replace('\\', "\\\\").replace('"', "\\\"");
        let floor = match since {
            Some(s) => format!(
                "FILTER(STR(?d) >= \"{}\")",
                s.replace('\\', "\\\\").replace('"', "\\\"")
            ),
            None => String::new(),
        };
        let q = format!(
            "SELECT DISTINCT ?s ?path ?type ?d WHERE {{
               ?s a pan:Image ; pan:mediaPath ?path ; pan:mediaType ?type ; pan:createdDate ?d .
               {needs}
               FILTER NOT EXISTS {{ ?s pan:{ref_local} ?e . ?e pan:model \"{model_lit}\" }}
               {floor}
             }} ORDER BY DESC(?d) ?s LIMIT {limit}"
        );
        let mut out = Vec::new();
        if let QueryResults::Solutions(sols) = self.query(&q)? {
            for s in sols {
                let s = s?;
                let get = |v: &str| s.get(v).map(term_str).unwrap_or_default();
                let iri = get("s");
                out.push(PendingItem {
                    id: bare_id(&iri),
                    iri,
                    media_path: get("path"),
                    media_type: get("type"),
                });
            }
        }
        Ok(out)
    }

    /// Images that have every listed (reference, model) pair recorded but no
    /// `pan:readyDate` yet — the ones the ladder can now mark ready.
    pub fn ready_candidates(
        &self,
        required: &[(String, String)],
        limit: usize,
    ) -> Result<Vec<String>> {
        let mut q = String::from(
            "SELECT ?s WHERE { ?s a pan:Image . FILTER NOT EXISTS { ?s pan:readyDate ?r } ",
        );
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
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        let already = self
            .store
            .quads_for_pattern(
                Some((&subject).into()),
                Some(pan_iri("readyDate").as_ref()),
                None,
                Some(GraphName::DefaultGraph.as_ref()),
            )
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
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        let quads = facts.into_quads(&subject, &self.cfg.prefixes, &self.cfg.default_prefix)?;
        self.insert_quads(&quads)?;
        self.restamp(id)
    }

    /// The media kind (`image`, `video`, …) an object's files live under —
    /// the first segment of its stored path.
    pub fn media_kind_of(&self, id: &str) -> Result<String> {
        let path = self
            .facts_for(id)?
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}mediaPath"))
            .and_then(|(_, v)| v.first().cloned())
            .unwrap_or_default();
        Ok(PanLayout::kind_of_path(&path).to_string())
    }

    /// Per-kind counts for `pand status` (Rob, 2026-09-07): how many images,
    /// and how many of them have each derived record.
    pub fn counts(&self) -> Result<StoreCounts> {
        let count = |pattern: &str| -> Result<u64> {
            let q =
                format!("SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ ?s a pan:Image . {pattern} }}");
            Ok(match self.query(&q)? {
                QueryResults::Solutions(mut sols) => sols
                    .next()
                    .and_then(|r| r.ok())
                    .and_then(|r| r.get("n").map(term_str))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                _ => 0,
            })
        };
        Ok(StoreCounts {
            images: count("")?,
            thumbnails: count("?s pan:thumbnail ?t .")?,
            // Records hang off their reference (pan:item), never off the
            // image directly (goodlux, 2026-09-16).
            captions: count("?s pan:captionData ?d . ?d pan:item ?c .")?,
            embeddings: count("?s pan:vectorData ?d . ?d pan:item ?e .")?,
            poses: count("?s pan:poseData ?d . ?d pan:item ?p .")?,
            regions: count("?s pan:regionData ?d . ?d pan:item ?r .")?,
            depths: count(&format!(
                "?s pan:{} ?d . ?d pan:item ?m .",
                depth::REF_LOCAL
            ))?,
        })
    }

    /// The image's scene objects (pan:sceneObjects), one per value — the
    /// segmentation prompts. Empty when the caption stage has not run.
    pub fn scene_objects_of(&self, id: &str) -> Result<Vec<String>> {
        Ok(self
            .facts_for(id)?
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}sceneObjects"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default())
    }

    fn created_date_of(&self, id: &str) -> Result<String> {
        Ok(self
            .facts_for(id)?
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}createdDate"))
            .and_then(|(_, v)| v.first().cloned())
            .unwrap_or_default())
    }
}

/// The two things that vary about the file a stage writes: the variant that
/// makes one model's record sit beside another's, and the server's own answer
/// saved next to it.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecordFile<'a> {
    /// Part of the record's file name, so two captioning models do not
    /// overwrite each other. None for a stage that runs once per image.
    pub variant: Option<&'a str>,
    /// Media-root-relative path of the server's own answer, when the stage
    /// saved one. Becomes pan:modelReplyPath on the reference.
    pub model_reply: Option<&'a str>,
}

impl<'a> RecordFile<'a> {
    pub fn variant(variant: &'a str) -> Self {
        Self {
            variant: Some(variant),
            model_reply: None,
        }
    }

    pub fn with_model_reply(mut self, rel: &'a str) -> Self {
        self.model_reply = Some(rel);
        self
    }
}

impl Pan {
    /// Where this stage's record file for `id` will land, relative to the
    /// store's media root. Deterministic: `write_enrichment` derives the same
    /// path. A caller needs it to name the server's answer file beside the
    /// record before the record exists.
    pub fn enrichment_rel(&self, id: &str, kind: &str, variant: Option<&str>) -> Result<String> {
        let created = self.created_date_of(id)?;
        let shard = created.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
        let media_kind = self.media_kind_of(id)?;
        Ok(PanLayout::enrichment_rel_path(
            &media_kind,
            kind,
            &shard,
            id,
            variant,
        ))
    }

    /// Record one model's output for an object as a data file beside the
    /// media plus the graph statements that describe it, then refresh the
    /// XMP so the image's own packet lists the new file. `kind` = data-file
    /// directory (caption / sam3 / pose); `ref_local` = reference predicate.
    /// The image links to the reference only; the reference links each record
    /// with pan:item (goodlux, 2026-09-16).
    pub fn write_enrichment(
        &self,
        id: &str,
        kind: &str,
        ref_local: &str,
        model: &str,
        records: &[enrich::EnrichmentRecord],
        file: RecordFile<'_>,
    ) -> Result<String> {
        let RecordFile {
            variant,
            model_reply,
        } = file;
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        let created = self.created_date_of(id)?;
        let shard = created.get(0..10).unwrap_or("0000-00-00").replace('-', "/");
        let media_kind = self.media_kind_of(id)?;
        let rel = PanLayout::enrichment_rel_path(&media_kind, kind, &shard, id, variant);
        let abs = self.layout.abs(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).context("create enrichment dir")?;
        }
        // pan:count only on the segmentation reference (pan:RegionData), the
        // one file that holds many records; caption, vector and pose
        // references carry none (goodlux, 2026-09-16).
        let count = (ref_local == "regionData").then_some(records.len());
        // The reference comes first: its IRI is the subject the data file
        // opens with and the node the records hang off.
        let mut r = enrich::EnrichmentRef::new(model, &rel, count);
        if let Some(answer) = model_reply {
            r = r.with_model_reply(answer);
        }
        write_atomic(&abs, enrich::build_data_file(&r.iri(), records).as_bytes())
            .with_context(|| format!("write {}", abs.display()))?;
        let mut quads = enrich::ref_quads(subject.as_str(), ref_local, &r)?;
        quads.extend(enrich::record_quads(&r.iri(), records)?);
        if let Err(e) = self.insert_quads(&quads) {
            let _ = fs::remove_file(&abs);
            return Err(e);
        }
        self.restamp(id)?;
        Ok(rel)
    }

    /// Record an embedding: vector into the index + `.npy` sidecar, the
    /// Embedding record in its own data file beside the `.npy`, a vectorData
    /// reference on the image, XMP refreshed.
    /// `details` is everything the server said besides the vector (its own
    /// model id, precision, provider, …). Two things happen with it, per Rob
    /// 2026-09-05: `precision` and `provider` become data on the Embedding
    /// record (declared, pan.ttl 0.3.2), and the WHOLE of it is written
    /// verbatim to `<vector>.json` beside the `.npy` — "save all the data".
    /// `model` stays the functional label = the index name; the server's own
    /// model id has no declared property yet and stays in the `.json` only.
    ///
    /// The record lives in `vectors/<index>/<id>.xml`, the same RDF/XML shape
    /// caption, pose, sam3 and depth write (reference node, `pan:item`, the
    /// record in full), so a rebuild from disk recovers its id, dim and
    /// producedDate (issue #31). The reference's `pan:path` names that file;
    /// the record's `pan:vectorPath` names the `.npy`.
    pub fn write_embedding(
        &self,
        id: &str,
        model: &str,
        index_name: &str,
        vec: &[f32],
        details: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<String> {
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        self.add_vector(id, index_name, vec)?;
        self.flush()?;
        let media_kind = self.media_kind_of(id)?;
        let npy_rel = PanLayout::vector_rel_path(&media_kind, index_name, id);
        // Everything the server said besides the vector, whole, beside the
        // .npy, and named on the reference as pan:modelReplyPath (goodlux,
        // 2026-09-19).
        let mut answer_rel: Option<String> = None;
        if !details.is_empty() {
            let rel = format!(
                "{}.json",
                npy_rel.strip_suffix(".npy").unwrap_or(npy_rel.as_str())
            );
            let side = self.layout.abs(&rel);
            write_atomic(&side, serde_json::to_string_pretty(details)?.as_bytes())
                .with_context(|| format!("write {}", side.display()))?;
            answer_rel = Some(rel);
        }
        let mut rec = enrich::EnrichmentRecord::new(gen_pan_id(), "Embedding", model)
            .field("dim", vec.len().to_string())
            .field("vectorPath", &npy_rel);
        for key in ["precision", "provider"] {
            if let Some(v) = details
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            {
                rec = rec.field(key, v);
            }
        }
        let rel = PanLayout::vector_record_rel_path(&media_kind, index_name, id);
        let abs = self.layout.abs(&rel);
        let mut r = enrich::EnrichmentRef::new(model, &rel, None);
        if let Some(reply) = &answer_rel {
            r = r.with_model_reply(reply);
        }
        write_atomic(
            &abs,
            enrich::build_data_file(&r.iri(), std::slice::from_ref(&rec)).as_bytes(),
        )
        .with_context(|| format!("write {}", abs.display()))?;
        let mut quads = enrich::ref_quads(subject.as_str(), "vectorData", &r)?;
        quads.extend(enrich::record_quads(&r.iri(), std::slice::from_ref(&rec))?);
        if let Err(e) = self.insert_quads(&quads) {
            let _ = fs::remove_file(&abs);
            return Err(e);
        }
        self.restamp(id)?;
        Ok(rel)
    }

    /// Write what the caption stage learned onto the object (pan.ttl 0.3.4):
    /// the two descriptions, the scene objects (one value each) and the scene
    /// fields. Every previous value of those properties goes first, so a
    /// re-caption replaces rather than accumulates. XMP refreshed.
    pub fn set_perception(&self, id: &str, p: &Perception) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        for local in PERCEPTION_FIELDS {
            let old: Vec<Quad> = self
                .store
                .quads_for_pattern(
                    Some((&subject).into()),
                    Some(pan_iri(local).as_ref()),
                    None,
                    Some(GraphName::DefaultGraph.as_ref()),
                )
                .collect::<std::result::Result<_, _>>()
                .with_context(|| format!("read {local}"))?;
            for q in &old {
                t.remove(q.as_ref());
            }
        }
        t.insert(
            self.quad(&subject, "shortCaption", &p.short_caption)
                .as_ref(),
        );
        t.insert(self.quad(&subject, "longCaption", &p.long_caption).as_ref());
        if !p.prompt_path.trim().is_empty() {
            t.insert(
                self.quad(&subject, "modelPromptPath", &p.prompt_path)
                    .as_ref(),
            );
        }
        for o in &p.scene_objects {
            t.insert(self.quad(&subject, "sceneObjects", o).as_ref());
        }
        for (local, value) in &p.scene {
            t.insert(self.quad(&subject, local, value).as_ref());
        }
        t.commit().context("commit perception")?;
        self.restamp(id)
    }

    /// Set facts a person owns on a media object — rating, isPicked,
    /// isRejected (pan.ttl 0.3.8; goodlux 2026-09-16: a rating lives on the
    /// image, in its XMP, not in a second database). Every key must be a
    /// settable field (see `settable_fields`) and every value must fit the
    /// declared range; one bad key refuses the whole request before anything
    /// is written. Old values of each key are deleted first, so setting
    /// overwrites. Graph and XMP change together: the restamp rewrites the
    /// image's packet.
    pub fn set_fields(&self, id: &str, fields: &[(String, serde_json::Value)]) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        if fields.is_empty() {
            return Err(anyhow!("nothing to set; give at least one key=value"));
        }
        let settable = settable_fields();
        let mut literals: Vec<(String, Literal)> = Vec::with_capacity(fields.len());
        for (local, value) in fields {
            let Some(f) = settable.iter().find(|f| &f.local == local) else {
                return Err(not_settable(local));
            };
            let lit = literal_for(f, value).map_err(|m| anyhow!("invalid value: {m}"))?;
            literals.push((local.clone(), lit));
        }
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        for (local, lit) in &literals {
            let old: Vec<Quad> = self
                .store
                .quads_for_pattern(
                    Some((&subject).into()),
                    Some(pan_iri(local).as_ref()),
                    None,
                    Some(GraphName::DefaultGraph.as_ref()),
                )
                .collect::<std::result::Result<_, _>>()
                .with_context(|| format!("read {local}"))?;
            for q in &old {
                t.remove(q.as_ref());
            }
            t.insert(
                Quad::new(
                    subject.clone(),
                    pan_iri(local),
                    lit.clone(),
                    GraphName::DefaultGraph,
                )
                .as_ref(),
            );
        }
        t.commit().context("commit set")?;
        self.restamp(id)
    }

    /// Remove facts a person set. Only settable fields may be unset; the
    /// caption stage's fields and Pan's own are refused the same way `set`
    /// refuses them. Unsetting a field that has no value is not an error.
    pub fn unset_fields(&self, id: &str, locals: &[String]) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        if locals.is_empty() {
            return Err(anyhow!("nothing to unset; give at least one property name"));
        }
        let settable = settable_fields();
        for local in locals {
            if !settable.iter().any(|f| &f.local == local) {
                return Err(not_settable(local));
            }
        }
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        for local in locals {
            let old: Vec<Quad> = self
                .store
                .quads_for_pattern(
                    Some((&subject).into()),
                    Some(pan_iri(local).as_ref()),
                    None,
                    Some(GraphName::DefaultGraph.as_ref()),
                )
                .collect::<std::result::Result<_, _>>()
                .with_context(|| format!("read {local}"))?;
            for q in &old {
                t.remove(q.as_ref());
            }
        }
        t.commit().context("commit unset")?;
        self.restamp(id)
    }

    /// Delete an object: media, thumbnail, data files, vector sidecars +
    /// index entries, and every statement about it or its records.
    pub fn delete(&self, id: &str) -> Result<()> {
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        let facts = self.facts_for(id)?;
        let pan_val = |local: &str| -> Option<String> {
            facts
                .iter()
                .find(|(p, _)| p == &format!("{PAN_NS}{local}"))
                .and_then(|(_, v)| v.first().cloned())
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
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        let mut targets = vec![subject.clone()];
        targets.extend(linked);
        for s in &targets {
            let qs: Vec<Quad> = self
                .store
                .quads_for_pattern(
                    Some(s.into()),
                    None,
                    None,
                    Some(GraphName::DefaultGraph.as_ref()),
                )
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
        let mut indexes = locked(&self.indexes);
        for name in index_names {
            if !indexes.contains_key(&name) {
                let known =
                    fs::read_to_string(self.layout.hnsw_root.join(&name).join("keymap.json"))
                        .ok()
                        .and_then(|raw| serde_json::from_str::<HashMap<String, u64>>(&raw).ok())
                        .map(|m| m.contains_key(id))
                        .unwrap_or(false);
                if !known {
                    continue;
                }
                indexes.insert(
                    name.clone(),
                    VectorIndex::create(&self.layout.hnsw_root, &name, 0)?,
                );
            }
            if let Some(vi) = indexes.get_mut(&name) {
                if let Some(key) = vi.id_to_key.remove(id) {
                    vi.key_to_id.remove(&key);
                    vi.index.remove(key).ok();
                    vi.dirty = true;
                }
            }
            let sidecar = self.layout.vector_sidecar_path(
                &self.media_kind_of(id).unwrap_or_else(|_| "image".into()),
                &name,
                id,
            );
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
        let mut indexes = locked(&self.indexes);
        if !indexes.contains_key(index_name) {
            indexes.insert(
                index_name.to_string(),
                VectorIndex::create(&self.layout.hnsw_root, index_name, vec.len())?,
            );
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
        if vi.id_to_key.contains_key(id) {
            return Ok(false);
        }
        npy::write_f32_1d(
            &self
                .layout
                .vector_sidecar_path(&self.media_kind_of(id)?, index_name, id),
            vec,
        )?;
        let key = vi.next_key;
        vi.next_key += 1;
        vi.id_to_key.insert(id.to_string(), key);
        vi.key_to_id.insert(key, id.to_string());
        let needed = vi.id_to_key.len();
        if vi.index.capacity() < needed {
            vi.index.reserve(needed.max(1024))?;
        }
        vi.index
            .add(key, vec)
            .map_err(|e| anyhow!("usearch add (id {}, index {}): {}", id, index_name, e))?;
        vi.dirty = true;
        Ok(true)
    }

    pub fn contains_id(&self, id: &str, index_name: &str) -> bool {
        let indexes = locked(&self.indexes);
        indexes
            .get(index_name)
            .map(|vi| vi.id_to_key.contains_key(id))
            .unwrap_or(false)
    }

    /// `(dim, count)` for every index visible on disk or in memory.
    pub fn index_stats(&self) -> Vec<(String, IndexStats)> {
        let indexes = locked(&self.indexes);
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
        if let Ok(rd) = fs::read_dir(&self.layout.hnsw_root) {
            for e in rd.filter_map(|e| e.ok()) {
                let Some(name) = e.file_name().to_str().map(String::from) else {
                    continue;
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

    /// Hybrid query — THE reason Pan exists. The SPARQL `where` (constraining
    /// `?s`, the media subject) gates the candidate set; usearch kNN ranks by
    /// cosine similarity to `like`. Pre-filter then search, joined at the
    /// application layer by the id↔key map. Empty `where` = pure kNN.
    pub fn search(
        &self,
        where_clause: &str,
        like: &[f32],
        k: usize,
        index_name: &str,
    ) -> Result<Vec<SearchHit>> {
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
        if let QueryResults::Solutions(sols) = SparqlEvaluator::new()
            .parse_query(&q)
            .map_err(|e| anyhow!("search where-clause: {e}"))?
            .on_store(&self.store)
            .execute()
            .map_err(|e| anyhow!("search where-clause: {e}"))?
        {
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
        let mut indexes = locked(&self.indexes);
        if !indexes.contains_key(index_name)
            && self
                .layout
                .hnsw_root
                .join(index_name)
                .join("index.usearch")
                .exists()
        {
            indexes.insert(
                index_name.to_string(),
                VectorIndex::create(&self.layout.hnsw_root, index_name, 0)?,
            );
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
        let total = vi.id_to_key.len() as f32;
        let selectivity = (candidate_keys.len() as f32 / total).max(0.001);
        let ef = ((k as f32 / selectivity).clamp(64.0, 4096.0)) as usize;
        vi.index.change_expansion_search(ef);
        let matches = vi
            .index
            .filtered_search(like, k, |key| candidate_keys.contains(&key))?;
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

    // ── SPARQL ────────────────────────────────────────────────────────────────

    /// Run a SPARQL query with the store's prefixes pre-declared (pan, git-lex,
    /// copia, pan.yml extras, rdf/rdfs/owl/xsd).
    pub fn query(&self, sparql: &str) -> Result<QueryResults<'_>> {
        let prologue = self.prefix_prologue();
        SparqlEvaluator::new()
            .parse_query(&format!("{prologue}{sparql}"))
            .map_err(|e| anyhow!("SPARQL error: {e}"))?
            .on_store(&self.store)
            .execute()
            .map_err(|e| anyhow!("SPARQL error: {e}"))
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
        let Some(subject) = self.subject_for(id)? else {
            return Err(anyhow!("id not found: {id}"));
        };
        let facts = self.facts_for(id)?;
        let Some(media_path) = facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}mediaPath"))
            .and_then(|(_, v)| v.first())
        else {
            return Err(anyhow!("id not found: {id}"));
        };
        let abs = self.layout.abs(media_path);
        let bytes = fs::read(&abs).with_context(|| format!("read media {}", abs.display()))?;
        if !xmp::is_png(&bytes) {
            return Ok(()); // non-PNG media carries no XMP (v1)
        }
        let existing = xmp::read_xmp_packet_from_bytes(&bytes).unwrap_or(None);
        let pan_desc = xmp::build_pan_description(&self.image_packet_from(&self.store, &subject)?);
        let packet = xmp::compose_packet(existing.as_deref(), &pan_desc);
        let written = xmp::write_packet_into_png_bytes(&bytes, &packet)?;
        write_atomic(&abs, &written).with_context(|| format!("write media {}", abs.display()))?;
        Ok(())
    }

    /// Pan's own block for one object, read from `store` (the live store on
    /// restamp; a scratch store holding the about-to-be-committed quads at
    /// ingest).
    fn image_packet_from(&self, store: &Store, subject: &NamedNode) -> Result<xmp::ImagePacket> {
        let facts = Self::facts_of(store, subject)?;
        let pan_field = |local: &str| -> Option<String> {
            facts
                .iter()
                .find(|(p, _)| p == &format!("{PAN_NS}{local}"))
                .and_then(|(_, v)| v.first().cloned())
        };
        let node_fields = |node_iri: &str| -> Result<HashMap<String, String>> {
            let node = NamedNode::new(node_iri).map_err(|e| anyhow!("node IRI: {e}"))?;
            let mut m = HashMap::new();
            for q in store.quads_for_pattern(
                Some((&node).into()),
                None,
                None,
                Some(GraphName::DefaultGraph.as_ref()),
            ) {
                let q = q.context("read node")?;
                if let Some(l) = q.predicate.as_str().strip_prefix(PAN_NS) {
                    m.insert(l.to_string(), term_str(&q.object));
                }
            }
            Ok(m)
        };

        let mut enrichment: Vec<(String, Vec<enrich::EnrichmentRef>)> = Vec::new();
        for ref_local in [
            "regionData",
            "poseData",
            "captionData",
            "vectorData",
            depth::REF_LOCAL,
        ] {
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
                            count: f.get("count").and_then(|c| c.parse().ok()),
                            produced_date: f.get("producedDate").cloned().unwrap_or_default(),
                            model_reply_path: f.get("modelReplyPath").cloned(),
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
                match (
                    f.get("path"),
                    f.get("width").and_then(|w| w.parse().ok()),
                    f.get("height").and_then(|h| h.parse().ok()),
                ) {
                    // producedDate rides into the file's thumbnail struct from the
                    // Thumbnail node, so file and graph say the same (pan issue #27).
                    (Some(p), Some(w), Some(h)) => Some(xmp::ThumbRef {
                        id: bare_id(&t),
                        path: p.clone(),
                        width: w,
                        height: h,
                        produced_date: f.get("producedDate").cloned().unwrap_or_default(),
                    }),
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
            source_file: pan_field("sourceFile").unwrap_or_default(),
            width: pan_field("width").and_then(|v| v.parse().ok()),
            height: pan_field("height").and_then(|v| v.parse().ok()),
            short_caption: pan_field("shortCaption"),
            long_caption: pan_field("longCaption"),
            scene_objects: facts
                .iter()
                .find(|(p, _)| p == &format!("{PAN_NS}sceneObjects"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default(),
            scene: SCENE_FIELDS
                .iter()
                .filter_map(|l| pan_field(l).map(|v| (l.to_string(), v)))
                .collect(),
            curation: settable_fields()
                .iter()
                .filter_map(|f| pan_field(&f.local).map(|v| (f.local.clone(), v)))
                .collect(),
            // The references Pan itself put on the image — imageset
            // membership, `<pan/ImageSet/id>` (goodlux, 2026-09-16). A
            // producer's relatedToId (Horae's `<copia/Moment/id>`) stays in
            // the producer's own block, so only pan Things are written here.
            related_to: {
                let mut v: Vec<String> = facts
                    .iter()
                    .filter(|(p, _)| p == &format!("{PAN_NS}relatedToId"))
                    .flat_map(|(_, vals)| vals.iter())
                    .filter(|iri| iri.starts_with(PAN_MEDIA_NS))
                    .map(|iri| xmp::bracket_of_iri(iri))
                    .collect();
                v.sort();
                v.dedup();
                v
            },
            ready_date: pan_field("readyDate"),
            thumbnail,
            enrichment,
        })
    }

    /// Persist dirty vector indexes. Called on Drop too.
    pub fn flush(&self) -> Result<()> {
        let mut indexes = locked(&self.indexes);
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

#[cfg(test)]
mod declare_store_tests {
    use super::*;

    /// pan issue #33: a store opened by the pre-ruling binary carries
    /// `git-lex:id` on its Store node; reopening must leave exactly one
    /// identity, spelled `pan:id`.
    #[test]
    fn reopening_a_store_rewrites_a_stale_identity_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let pan = Pan::open(dir.path()).unwrap();
        let node = NamedNode::new(format!("{PAN_MEDIA_NS}Store/{}", pan.store_id)).unwrap();
        let stale = Quad::new(
            node.clone(),
            NamedNode::new(format!("{GIT_LEX_NS}id")).unwrap(),
            node.clone(),
            GraphName::DefaultGraph,
        );
        pan.store.insert(stale.as_ref()).unwrap();

        pan.declare_store().unwrap();

        let ids: Vec<String> = pan
            .store
            .quads_for_pattern(
                Some((&node).into()),
                None,
                None,
                Some(GraphName::DefaultGraph.as_ref()),
            )
            .filter_map(|q| q.ok())
            .map(|q| q.predicate.as_str().to_string())
            .filter(|p| p.ends_with("/id"))
            .collect();
        assert_eq!(
            ids,
            vec![format!("{PAN_NS}id")],
            "exactly one identity predicate, spelled pan:id: {ids:?}"
        );
    }
}

#[cfg(test)]
mod locked_tests {
    use super::locked;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_poisoned_mutex_is_recovered_not_repanicked() {
        let m = Arc::new(Mutex::new(vec![1u8]));
        let poisoner = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let mut g = poisoner.lock().unwrap();
            g.push(2);
            panic!("holder dies while holding the lock");
        })
        .join();
        assert!(
            m.is_poisoned(),
            "the thread's panic must have poisoned the mutex"
        );
        assert!(m.lock().is_err(), "plain lock() reports the poison");

        let mut g = locked(&m);
        assert_eq!(*g, vec![1, 2], "the state the holder wrote is still there");
        g.push(3);
        drop(g);
        assert_eq!(*locked(&m), vec![1, 2, 3]);
    }
}
