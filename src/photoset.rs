//! Photosets — a set of media a person curates (pan.ttl 0.4.2, goodlux
//! 2026-09-16; issue #4).
//!
//! A Photoset is a `subtexture:Set` with exactly three facts: its id, its
//! description, and when it was created. It has NO member list. Membership
//! is the universal edge from the image: `pan:relatedToId <pan/Photoset/id>`
//! in the image's XMP and in the graph. Ask "which images are in set X" and
//! the answer is a graph pattern, never a list kept on the set.
//!
//! Every set has its own file, `<store>/photosets/<id>.xml`, an XMP-style
//! RDF/XML packet with the same conventions as the image XMP: a root
//! Description about the set itself, pan: vocabulary only, identities in
//! git-lex's angle-bracket form. The file is the source of truth: on every
//! open the store reads `photosets/*.xml` and rewrites the set nodes in the
//! graph from them, so the graph is rebuilt from files alone. The store root
//! is committed (only `_ignore/` is not), so a soul's sets travel with it.

use anyhow::{anyhow, Context, Result};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

use crate::config::{GIT_LEX_NS, PAN_MEDIA_NS, PAN_NS};
use crate::{bare_id, enrich, gen_pan_id, git_lex_iri, now_local, pan_iri, validate_pan_id, write_atomic, xmp, Pan, PanLayout};

/// The ontology class and the IRI path segment: `<pan/Photoset/id>`.
pub const PHOTOSET_CLASS: &str = "Photoset";

/// One set as the graph and its file hold it. Nothing else is on a set.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Photoset {
    /// The bare id, the same token the file name and the IRI carry.
    pub id: String,
    /// `https://repolex.ai/pan/Photoset/<id>` — the identity (`git-lex:id`).
    pub iri: String,
    /// `pan:description` in the file, `git-lex:description` in the graph.
    /// At most one.
    pub description: Option<String>,
    /// `pan:createdDate` in the file, `git-lex:createdDate` in the graph.
    pub created_date: String,
}

pub fn photoset_iri(id: &str) -> Result<NamedNode> {
    validate_pan_id(id)?;
    NamedNode::new(format!("{PAN_MEDIA_NS}{PHOTOSET_CLASS}/{id}")).map_err(|e| anyhow!("photoset IRI: {e}"))
}

/// The set's file, `photosets/<id>.xml`: one root Description about the set,
/// the three declared fields, nothing else. Same packet wrapping as the
/// image XMP so the same reader parses both.
pub fn build_photoset_file(p: &Photoset) -> String {
    let mut desc = String::with_capacity(512);
    desc.push_str("    <rdf:Description rdf:about=\"\"");
    desc.push_str(&format!(" xmlns:pan=\"{PAN_NS}\">\n"));
    desc.push_str(&format!("      <pan:id>{}</pan:id>\n", xml_escape(&xmp::bracket_of_iri(&p.iri))));
    desc.push_str(&format!("      <pan:createdDate>{}</pan:createdDate>\n", xml_escape(&p.created_date)));
    if let Some(d) = &p.description {
        desc.push_str(&format!("      <pan:description>{}</pan:description>\n", xml_escape(d)));
    }
    desc.push_str("    </rdf:Description>\n");
    xmp::compose_packet(None, &desc)
}

/// Read a set back from its file text. Strict: the root Description must
/// carry a `pan:id` of the form `<pan/Photoset/id>` and a `pan:createdDate`;
/// a file that says less is an error, never a half-set.
pub fn read_photoset_file(text: &str) -> Result<Photoset> {
    let subjects = xmp::parse_packet(text).context("parse photoset file")?;
    let root = subjects.iter().find(|s| s.subject.is_none()).ok_or_else(|| anyhow!("photoset file has no root Description"))?;
    let one = |local: &str| -> Option<String> {
        root.facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}{local}"))
            .and_then(|(_, v)| v.first())
            .map(|t| t.value().to_string())
    };
    let id_text = one("id").ok_or_else(|| anyhow!("photoset file has no pan:id"))?;
    let iri = crate::iri_from_bracket(&id_text).ok_or_else(|| anyhow!("photoset pan:id is not <pan/Photoset/id>: {id_text}"))?;
    let id = iri
        .strip_prefix(&format!("{PAN_MEDIA_NS}{PHOTOSET_CLASS}/"))
        .ok_or_else(|| anyhow!("photoset pan:id is not <pan/Photoset/id>: {id_text}"))?
        .to_string();
    validate_pan_id(&id)?;
    let created_date = one("createdDate").ok_or_else(|| anyhow!("photoset file has no pan:createdDate"))?;
    Ok(Photoset { id, iri, description: one("description"), created_date })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

impl Pan {
    /// `<root>/photosets` — committed with the store, one file per set.
    pub fn photosets_root(&self) -> PathBuf {
        self.layout.photosets_root()
    }

    fn photoset_file(&self, id: &str) -> PathBuf {
        self.photosets_root().join(format!("{id}.xml"))
    }

    /// Resolve a bare id to the set's IRI, if the graph holds a set by it.
    pub fn photoset_subject(&self, id: &str) -> Result<Option<NamedNode>> {
        if validate_pan_id(id).is_err() {
            return Ok(None);
        }
        let node = photoset_iri(id)?;
        let exists = self
            .store
            .quads_for_pattern(Some((&node).into()), Some(crate::rdf_type().as_ref()), Some(pan_iri(PHOTOSET_CLASS).as_ref().into()), Some(GraphName::DefaultGraph.as_ref()))
            .next()
            .is_some();
        Ok(exists.then_some(node))
    }

    /// The three facts of a set as graph quads: type, identity, creation
    /// time, description. In the graph the universals wear their git-lex
    /// names; the file spells them pan: (the same boundary the image keeps).
    fn photoset_quads(p: &Photoset) -> Result<Vec<Quad>> {
        let node = photoset_iri(&p.id)?;
        let mut quads = vec![
            Quad::new(node.clone(), crate::rdf_type(), pan_iri(PHOTOSET_CLASS), GraphName::DefaultGraph),
            enrich::self_id_quad(&node)?,
            Quad::new(node.clone(), git_lex_iri("createdDate"), Literal::new_simple_literal(&p.created_date), GraphName::DefaultGraph),
        ];
        if let Some(d) = &p.description {
            quads.push(Quad::new(node, git_lex_iri("description"), Literal::new_simple_literal(d), GraphName::DefaultGraph));
        }
        Ok(quads)
    }

    /// Put the set's node in the graph exactly as `p` says: every statement
    /// the node had is removed first, so the file always wins.
    fn write_photoset_node(&self, p: &Photoset) -> Result<()> {
        let node = photoset_iri(&p.id)?;
        let old: Vec<Quad> = self
            .store
            .quads_for_pattern(Some((&node).into()), None, None, Some(GraphName::DefaultGraph.as_ref()))
            .collect::<std::result::Result<_, _>>()
            .context("read photoset node")?;
        let mut t = self.store.start_transaction().context("start transaction")?;
        for q in &old {
            t.remove(q.as_ref());
        }
        for q in Self::photoset_quads(p)? {
            t.insert(q.as_ref());
        }
        t.commit().context("commit photoset")?;
        Ok(())
    }

    /// Make a new set: a fresh id (never one an image or a set already has),
    /// its file written first, then its node in the graph. Fails before
    /// anything is written if the file cannot be; removes the file if the
    /// graph refuses.
    pub fn photoset_create(&self, description: Option<&str>) -> Result<Photoset> {
        let description = description.map(str::trim).filter(|d| !d.is_empty()).map(String::from);
        let id = loop {
            let cand = gen_pan_id();
            if self.subject_for(&cand)?.is_none() && self.photoset_subject(&cand)?.is_none() && !self.photoset_file(&cand).exists() {
                break cand;
            }
        };
        let p = Photoset { iri: photoset_iri(&id)?.into_string(), id, description, created_date: now_local() };
        let path = self.photoset_file(&p.id);
        fs::create_dir_all(self.photosets_root()).with_context(|| format!("create {}", self.photosets_root().display()))?;
        write_atomic(&path, build_photoset_file(&p).as_bytes())?;
        if let Err(e) = self.write_photoset_node(&p) {
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        Ok(p)
    }

    /// Every set in the graph, oldest first.
    pub fn photoset_list(&self) -> Result<Vec<Photoset>> {
        let mut out = Vec::new();
        for q in self.store.quads_for_pattern(None, Some(crate::rdf_type().as_ref()), Some(pan_iri(PHOTOSET_CLASS).as_ref().into()), Some(GraphName::DefaultGraph.as_ref())) {
            let q = q.context("list photosets")?;
            let oxigraph::model::NamedOrBlankNode::NamedNode(node) = &q.subject else { continue };
            if let Some(p) = self.photoset_of(node)? {
                out.push(p);
            }
        }
        out.sort_by(|a, b| a.created_date.cmp(&b.created_date).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// One set by id, from the graph. None = no such set.
    pub fn photoset_get(&self, id: &str) -> Result<Option<Photoset>> {
        match self.photoset_subject(id)? {
            Some(node) => self.photoset_of(&node),
            None => Ok(None),
        }
    }

    fn photoset_of(&self, node: &NamedNode) -> Result<Option<Photoset>> {
        let mut created_date = None;
        let mut description = None;
        for q in self.store.quads_for_pattern(Some(node.into()), None, None, Some(GraphName::DefaultGraph.as_ref())) {
            let q = q.context("read photoset")?;
            let value = match &q.object {
                Term::Literal(l) => l.value().to_string(),
                _ => continue,
            };
            match q.predicate.as_str().strip_prefix(GIT_LEX_NS) {
                Some("createdDate") => created_date = Some(value),
                Some("description") => description = Some(value),
                _ => {}
            }
        }
        let Some(created_date) = created_date else { return Ok(None) };
        Ok(Some(Photoset { id: bare_id(node.as_str()), iri: node.as_str().to_string(), description, created_date }))
    }

    /// The IRIs of every media object whose `pan:relatedToId` names the set.
    pub fn photoset_members(&self, id: &str) -> Result<Vec<String>> {
        let Some(node) = self.photoset_subject(id)? else { return Err(anyhow!("photoset not found: {id}")) };
        let mut out = Vec::new();
        for q in self.store.quads_for_pattern(None, Some(pan_iri("relatedToId").as_ref()), Some((&node).into()), Some(GraphName::DefaultGraph.as_ref())) {
            let q = q.context("read members")?;
            if let oxigraph::model::NamedOrBlankNode::NamedNode(s) = &q.subject {
                out.push(s.as_str().to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Put a media object in a set: `pan:relatedToId <pan/Photoset/id>` on
    /// the image, in the graph and then in its XMP. Already a member = no
    /// change. Both the set and the image must exist.
    pub fn photoset_add(&self, set_id: &str, media_id: &str) -> Result<()> {
        let Some(set) = self.photoset_subject(set_id)? else { return Err(anyhow!("photoset not found: {set_id}")) };
        let Some(media) = self.subject_for(media_id)? else { return Err(anyhow!("id not found: {media_id}")) };
        let edge = Quad::new(media, pan_iri("relatedToId"), set, GraphName::DefaultGraph);
        if self.store.contains(edge.as_ref()).context("check membership")? {
            return Ok(());
        }
        self.insert_quads(&[edge])?;
        self.restamp(media_id)
    }

    /// Take a media object out of a set. Not a member = no change.
    pub fn photoset_remove(&self, set_id: &str, media_id: &str) -> Result<()> {
        let Some(set) = self.photoset_subject(set_id)? else { return Err(anyhow!("photoset not found: {set_id}")) };
        let Some(media) = self.subject_for(media_id)? else { return Err(anyhow!("id not found: {media_id}")) };
        let edge = Quad::new(media, pan_iri("relatedToId"), set, GraphName::DefaultGraph);
        if !self.store.contains(edge.as_ref()).context("check membership")? {
            return Ok(());
        }
        let mut t = self.store.start_transaction().context("start transaction")?;
        t.remove(edge.as_ref());
        t.commit().context("commit remove")?;
        self.restamp(media_id)
    }

    /// Rebuild every set node from `photosets/*.xml`. Called at every open,
    /// so the graph never says a set the files do not. A file whose id does
    /// not match its name, or that is not a set at all, is an error: the
    /// store does not open over a set it cannot read. Returns how many sets
    /// were loaded.
    pub(crate) fn load_photosets(&self) -> Result<usize> {
        let root = self.photosets_root();
        if !root.is_dir() {
            return Ok(0);
        }
        let mut n = 0;
        let mut entries: Vec<PathBuf> = fs::read_dir(&root)
            .with_context(|| format!("read {}", root.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("xml") && !p.file_name().and_then(|f| f.to_str()).unwrap_or("").starts_with('.'))
            .collect();
        entries.sort();
        for path in entries {
            let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            let p = read_photoset_file(&text).with_context(|| format!("photoset file {}", path.display()))?;
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem != p.id {
                return Err(anyhow!("photoset file {} carries pan:id <pan/Photoset/{}>; the file name and the id must agree", path.display(), p.id));
            }
            self.write_photoset_node(&p)?;
            n += 1;
        }
        Ok(n)
    }
}

impl PanLayout {
    /// `photosets/` — one file per curated set, at the store root, committed.
    pub const PHOTOSETS_SUBDIR: &'static str = "photosets";

    /// `<root>/photosets`.
    pub fn photosets_root(&self) -> PathBuf {
        self.root.join(Self::PHOTOSETS_SUBDIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_file_round_trips_its_three_facts_and_nothing_else() {
        let p = Photoset {
            id: "abcd2345".into(),
            iri: format!("{PAN_MEDIA_NS}Photoset/abcd2345"),
            description: Some("portraits & <tests>".into()),
            created_date: "2026-09-16T12:00:00-07:00".into(),
        };
        let text = build_photoset_file(&p);
        assert!(text.contains("<pan:id>&lt;pan/Photoset/abcd2345&gt;</pan:id>"), "{text}");
        assert!(text.contains("<pan:createdDate>2026-09-16T12:00:00-07:00</pan:createdDate>"), "{text}");
        assert!(text.contains("<pan:description>portraits &amp; &lt;tests&gt;</pan:description>"), "{text}");
        assert!(!text.contains("git-lex"), "the file carries pan: only");
        assert!(!text.contains("member") && !text.contains("inPhotoset"), "no member list on a set");
        assert_eq!(read_photoset_file(&text).unwrap(), p);
    }

    #[test]
    fn a_set_file_without_identity_is_refused() {
        let text = xmp::compose_packet(None, &format!("    <rdf:Description rdf:about=\"\" xmlns:pan=\"{PAN_NS}\">\n      <pan:description>x</pan:description>\n    </rdf:Description>\n"));
        let err = read_photoset_file(&text).unwrap_err().to_string();
        assert!(err.contains("pan:id"), "{err}");
        let text = xmp::compose_packet(None, &format!("    <rdf:Description rdf:about=\"\" xmlns:pan=\"{PAN_NS}\">\n      <pan:id>&lt;pan/Image/abcd2345&gt;</pan:id>\n      <pan:createdDate>2026-09-16T12:00:00-07:00</pan:createdDate>\n    </rdf:Description>\n"));
        let err = read_photoset_file(&text).unwrap_err().to_string();
        assert!(err.contains("not <pan/Photoset/id>"), "{err}");
    }
}
