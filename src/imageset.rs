//! ImageSets — a set of images a person curates (pan.ttl 0.4.2, goodlux
//! 2026-09-16; issue #4; renamed from Photoset with pan.ttl 0.4.7, goodlux
//! 2026-09-17: pan:MediaSet is the parent, a set of any media kind, and
//! pan:ImageSet is the one the code writes — images only. A VideoSet or
//! AudioSet gets its own class beside it when that media kind exists, so a
//! UI can pick its handler from the class alone).
//!
//! An ImageSet is a `subtexture:Set` with exactly three facts: its id, its
//! description, and when it was created. It has NO member list. Membership
//! is the universal edge from the image: `pan:relatedToId <pan/ImageSet/id>`
//! in the image's XMP and in the graph. Ask "which images are in set X" and
//! the answer is a graph pattern, never a list kept on the set. Only a
//! `pan:Image` may be added; the class name is the promise.
//!
//! Every set has its own file, `<store>/imagesets/<id>.xml`, an XMP-style
//! RDF/XML packet with the same conventions as the image XMP: a root
//! Description about the set itself, pan: vocabulary only, identities in
//! git-lex's angle-bracket form. The file is the source of truth: on every
//! open the store reads `imagesets/*.xml` and rewrites the set nodes in the
//! graph from them, so the graph is rebuilt from files alone. The store root
//! is committed (only `_ignore/` is not), so a soul's sets travel with it.

use anyhow::{anyhow, Context, Result};
use oxigraph::model::{Literal, NamedNode, Quad, Term};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

use crate::config::{PAN_MEDIA_NS, PAN_NS};
use crate::{
    bare_id, enrich, gen_pan_id, now_local, pan_iri, validate_pan_id, write_atomic, xmp, Pan,
    PanLayout,
};

/// The ontology class and the IRI path segment: `<pan/ImageSet/id>`.
pub const IMAGESET_CLASS: &str = "ImageSet";

/// One set as the graph and its file hold it. Nothing else is on a set.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ImageSet {
    /// The bare id, the same token the file name and the IRI carry.
    pub id: String,
    /// `https://repolex.ai/pan/ImageSet/<id>` — the identity (`pan:id`).
    pub iri: String,
    /// `pan:description`, in the file and in the graph alike.
    /// At most one.
    pub description: Option<String>,
    /// `pan:createdDate`, in the file and in the graph alike.
    pub created_date: String,
}

pub fn imageset_iri(id: &str) -> Result<NamedNode> {
    validate_pan_id(id)?;
    NamedNode::new(format!("{PAN_MEDIA_NS}{IMAGESET_CLASS}/{id}"))
        .map_err(|e| anyhow!("imageset IRI: {e}"))
}

/// The set's file, `imagesets/<id>.xml`: one root Description about the set,
/// the three declared fields, nothing else. Same packet wrapping as the
/// image XMP so the same reader parses both.
pub fn build_imageset_file(p: &ImageSet) -> String {
    let mut desc = String::with_capacity(512);
    desc.push_str("    <rdf:Description rdf:about=\"\"");
    desc.push_str(&format!(" xmlns:pan=\"{PAN_NS}\">\n"));
    desc.push_str(&format!(
        "      <pan:id>{}</pan:id>\n",
        xml_escape(&xmp::bracket_of_iri(&p.iri))
    ));
    desc.push_str(&format!(
        "      <pan:createdDate>{}</pan:createdDate>\n",
        xml_escape(&p.created_date)
    ));
    if let Some(d) = &p.description {
        desc.push_str(&format!(
            "      <pan:description>{}</pan:description>\n",
            xml_escape(d)
        ));
    }
    desc.push_str("    </rdf:Description>\n");
    xmp::compose_packet(None, &desc)
}

/// Read a set back from its file text. Strict: the root Description must
/// carry a `pan:id` of the form `<pan/ImageSet/id>` and a `pan:createdDate`;
/// a file that says less is an error, never a half-set.
pub fn read_imageset_file(text: &str) -> Result<ImageSet> {
    let subjects = xmp::parse_packet(text).context("parse imageset file")?;
    let root = subjects
        .iter()
        .find(|s| s.subject.is_none())
        .ok_or_else(|| anyhow!("imageset file has no root Description"))?;
    let one = |local: &str| -> Option<String> {
        root.facts
            .iter()
            .find(|(p, _)| p == &format!("{PAN_NS}{local}"))
            .and_then(|(_, v)| v.first())
            .map(|t| t.value().to_string())
    };
    let id_text = one("id").ok_or_else(|| anyhow!("imageset file has no pan:id"))?;
    let iri = crate::iri_from_bracket(&id_text)
        .ok_or_else(|| anyhow!("imageset pan:id is not <pan/ImageSet/id>: {id_text}"))?;
    let id = iri
        .strip_prefix(&format!("{PAN_MEDIA_NS}{IMAGESET_CLASS}/"))
        .ok_or_else(|| anyhow!("imageset pan:id is not <pan/ImageSet/id>: {id_text}"))?
        .to_string();
    validate_pan_id(&id)?;
    let created_date =
        one("createdDate").ok_or_else(|| anyhow!("imageset file has no pan:createdDate"))?;
    Ok(ImageSet {
        id,
        iri,
        description: one("description"),
        created_date,
    })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl Pan {
    /// `<root>/imagesets` — committed with the store, one file per set.
    pub fn imagesets_root(&self) -> PathBuf {
        self.layout.imagesets_root()
    }

    fn imageset_file(&self, id: &str) -> PathBuf {
        self.imagesets_root().join(format!("{id}.xml"))
    }

    /// Resolve a bare id to the set's IRI, if the graph holds a set by it.
    pub fn imageset_subject(&self, id: &str) -> Result<Option<NamedNode>> {
        if validate_pan_id(id).is_err() {
            return Ok(None);
        }
        let node = imageset_iri(id)?;
        let exists = self
            .store
            .quads_for_pattern(
                Some((&node).into()),
                Some(crate::rdf_type().as_ref()),
                Some(pan_iri(IMAGESET_CLASS).as_ref().into()),
                Some(crate::config::pan_graph().as_ref()),
            )
            .next()
            .is_some();
        Ok(exists.then_some(node))
    }

    /// The three facts of a set as graph quads: type, identity, creation
    /// time, description — spelled pan: in the graph exactly as in the file
    /// (goodlux, 2026-09-17).
    fn imageset_quads(p: &ImageSet) -> Result<Vec<Quad>> {
        let node = imageset_iri(&p.id)?;
        let mut quads = vec![
            Quad::new(
                node.clone(),
                crate::rdf_type(),
                pan_iri(IMAGESET_CLASS),
                crate::config::pan_graph(),
            ),
            enrich::self_id_quad(&node)?,
            Quad::new(
                node.clone(),
                pan_iri("createdDate"),
                Literal::new_simple_literal(&p.created_date),
                crate::config::pan_graph(),
            ),
        ];
        if let Some(d) = &p.description {
            quads.push(Quad::new(
                node,
                pan_iri("description"),
                Literal::new_simple_literal(d),
                crate::config::pan_graph(),
            ));
        }
        Ok(quads)
    }

    /// Put the set's node in the graph exactly as `p` says: every statement
    /// the node had is removed first, so the file always wins.
    fn write_imageset_node(&self, p: &ImageSet) -> Result<()> {
        let node = imageset_iri(&p.id)?;
        let old: Vec<Quad> = self
            .store
            .quads_for_pattern(
                Some((&node).into()),
                None,
                None,
                Some(crate::config::pan_graph().as_ref()),
            )
            .collect::<std::result::Result<_, _>>()
            .context("read imageset node")?;
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        for q in &old {
            t.remove(q.as_ref());
        }
        for q in Self::imageset_quads(p)? {
            t.insert(q.as_ref());
        }
        t.commit().context("commit imageset")?;
        Ok(())
    }

    /// Make a new set: a fresh id (never one an image or a set already has),
    /// its file written first, then its node in the graph. Fails before
    /// anything is written if the file cannot be; removes the file if the
    /// graph refuses.
    pub fn imageset_create(&self, description: Option<&str>) -> Result<ImageSet> {
        let description = description
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(String::from);
        let id = loop {
            let cand = gen_pan_id();
            if self.subject_for(&cand)?.is_none()
                && self.imageset_subject(&cand)?.is_none()
                && !self.imageset_file(&cand).exists()
            {
                break cand;
            }
        };
        let p = ImageSet {
            iri: imageset_iri(&id)?.into_string(),
            id,
            description,
            created_date: now_local(),
        };
        let path = self.imageset_file(&p.id);
        fs::create_dir_all(self.imagesets_root())
            .with_context(|| format!("create {}", self.imagesets_root().display()))?;
        write_atomic(&path, build_imageset_file(&p).as_bytes())?;
        if let Err(e) = self.write_imageset_node(&p) {
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        Ok(p)
    }

    /// Every set in the graph, oldest first.
    pub fn imageset_list(&self) -> Result<Vec<ImageSet>> {
        let mut out = Vec::new();
        for q in self.store.quads_for_pattern(
            None,
            Some(crate::rdf_type().as_ref()),
            Some(pan_iri(IMAGESET_CLASS).as_ref().into()),
            Some(crate::config::pan_graph().as_ref()),
        ) {
            let q = q.context("list imagesets")?;
            let oxigraph::model::NamedOrBlankNode::NamedNode(node) = &q.subject else {
                continue;
            };
            if let Some(p) = self.imageset_of(node)? {
                out.push(p);
            }
        }
        out.sort_by(|a, b| a.created_date.cmp(&b.created_date).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// One set by id, from the graph. None = no such set.
    pub fn imageset_get(&self, id: &str) -> Result<Option<ImageSet>> {
        match self.imageset_subject(id)? {
            Some(node) => self.imageset_of(&node),
            None => Ok(None),
        }
    }

    fn imageset_of(&self, node: &NamedNode) -> Result<Option<ImageSet>> {
        let mut created_date = None;
        let mut description = None;
        for q in self.store.quads_for_pattern(
            Some(node.into()),
            None,
            None,
            Some(crate::config::pan_graph().as_ref()),
        ) {
            let q = q.context("read imageset")?;
            let value = match &q.object {
                Term::Literal(l) => l.value().to_string(),
                _ => continue,
            };
            match q.predicate.as_str().strip_prefix(PAN_NS) {
                Some("createdDate") => created_date = Some(value),
                Some("description") => description = Some(value),
                _ => {}
            }
        }
        let Some(created_date) = created_date else {
            return Ok(None);
        };
        Ok(Some(ImageSet {
            id: bare_id(node.as_str()),
            iri: node.as_str().to_string(),
            description,
            created_date,
        }))
    }

    /// The IRIs of every media object whose `pan:relatedToId` names the set.
    pub fn imageset_members(&self, id: &str) -> Result<Vec<String>> {
        let Some(node) = self.imageset_subject(id)? else {
            return Err(anyhow!("imageset not found: {id}"));
        };
        let mut out = Vec::new();
        for q in self.store.quads_for_pattern(
            None,
            Some(pan_iri("relatedToId").as_ref()),
            Some((&node).into()),
            Some(crate::config::pan_graph().as_ref()),
        ) {
            let q = q.context("read members")?;
            if let oxigraph::model::NamedOrBlankNode::NamedNode(s) = &q.subject {
                out.push(s.as_str().to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Put an image in a set: `pan:relatedToId <pan/ImageSet/id>` on the
    /// image, in the graph and then in its XMP. Already a member = no
    /// change. Both the set and the image must exist, and the media must be
    /// a `pan:Image`: an ImageSet holds images only, so anything else is
    /// refused before anything is written.
    pub fn imageset_add(&self, set_id: &str, media_id: &str) -> Result<()> {
        let Some(set) = self.imageset_subject(set_id)? else {
            return Err(anyhow!("imageset not found: {set_id}"));
        };
        let Some(media) = self.subject_for(media_id)? else {
            return Err(anyhow!("id not found: {media_id}"));
        };
        let is_image = self
            .store
            .contains(
                Quad::new(
                    media.clone(),
                    crate::rdf_type(),
                    pan_iri("Image"),
                    crate::config::pan_graph(),
                )
                .as_ref(),
            )
            .context("check media class")?;
        if !is_image {
            let class = self
                .store
                .quads_for_pattern(
                    Some((&media).into()),
                    Some(crate::rdf_type().as_ref()),
                    None,
                    Some(crate::config::pan_graph().as_ref()),
                )
                .filter_map(|q| q.ok())
                .find_map(|q| match q.object {
                    Term::NamedNode(n) => n.as_str().strip_prefix(PAN_NS).map(str::to_string),
                    _ => None,
                })
                .unwrap_or_else(|| "untyped".to_string());
            return Err(anyhow!(
                "{media_id} is a pan:{class}, not a pan:Image; an ImageSet takes images only. A set for that media kind is not declared yet."
            ));
        }
        let edge = Quad::new(
            media,
            pan_iri("relatedToId"),
            set,
            crate::config::pan_graph(),
        );
        if self
            .store
            .contains(edge.as_ref())
            .context("check membership")?
        {
            return Ok(());
        }
        self.insert_quads(&[edge])?;
        self.restamp(media_id)
    }

    /// Take a media object out of a set. Not a member = no change.
    pub fn imageset_remove(&self, set_id: &str, media_id: &str) -> Result<()> {
        let Some(set) = self.imageset_subject(set_id)? else {
            return Err(anyhow!("imageset not found: {set_id}"));
        };
        let Some(media) = self.subject_for(media_id)? else {
            return Err(anyhow!("id not found: {media_id}"));
        };
        let edge = Quad::new(
            media,
            pan_iri("relatedToId"),
            set,
            crate::config::pan_graph(),
        );
        if !self
            .store
            .contains(edge.as_ref())
            .context("check membership")?
        {
            return Ok(());
        }
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        t.remove(edge.as_ref());
        t.commit().context("commit remove")?;
        self.restamp(media_id)
    }

    /// Rebuild every set node from `imagesets/*.xml`. Called at every open,
    /// so the graph never says a set the files do not. A file whose id does
    /// not match its name, or that is not a set at all, is an error: the
    /// store does not open over a set it cannot read. Returns how many sets
    /// were loaded.
    pub(crate) fn load_imagesets(&self) -> Result<usize> {
        let root = self.imagesets_root();
        if !root.is_dir() {
            return Ok(0);
        }
        let mut n = 0;
        let mut entries: Vec<PathBuf> = fs::read_dir(&root)
            .with_context(|| format!("read {}", root.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("xml")
                    && !p
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or("")
                        .starts_with('.')
            })
            .collect();
        entries.sort();
        for path in entries {
            let text =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            let p = read_imageset_file(&text)
                .with_context(|| format!("imageset file {}", path.display()))?;
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem != p.id {
                return Err(anyhow!("imageset file {} carries pan:id <pan/ImageSet/{}>; the file name and the id must agree", path.display(), p.id));
            }
            self.write_imageset_node(&p)?;
            n += 1;
        }
        Ok(n)
    }
}

impl PanLayout {
    /// `imagesets/` — one file per curated set, at the store root, committed.
    pub const IMAGESETS_SUBDIR: &'static str = "imagesets";

    /// `<root>/imagesets`.
    pub fn imagesets_root(&self) -> PathBuf {
        self.root.join(Self::IMAGESETS_SUBDIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_file_round_trips_its_three_facts_and_nothing_else() {
        let p = ImageSet {
            id: "abcd2345".into(),
            iri: format!("{PAN_MEDIA_NS}ImageSet/abcd2345"),
            description: Some("portraits & <tests>".into()),
            created_date: "2026-09-16T12:00:00-07:00".into(),
        };
        let text = build_imageset_file(&p);
        assert!(
            text.contains("<pan:id>&lt;pan/ImageSet/abcd2345&gt;</pan:id>"),
            "{text}"
        );
        assert!(
            text.contains("<pan:createdDate>2026-09-16T12:00:00-07:00</pan:createdDate>"),
            "{text}"
        );
        assert!(
            text.contains("<pan:description>portraits &amp; &lt;tests&gt;</pan:description>"),
            "{text}"
        );
        assert!(!text.contains("git-lex"), "the file carries pan: only");
        assert!(
            !text.contains("member") && !text.contains("inPhotoset"),
            "no member list on a set"
        );
        assert_eq!(read_imageset_file(&text).unwrap(), p);
    }

    #[test]
    fn a_set_file_without_identity_is_refused() {
        let text = xmp::compose_packet(None, &format!("    <rdf:Description rdf:about=\"\" xmlns:pan=\"{PAN_NS}\">\n      <pan:description>x</pan:description>\n    </rdf:Description>\n"));
        let err = read_imageset_file(&text).unwrap_err().to_string();
        assert!(err.contains("pan:id"), "{err}");
        let text = xmp::compose_packet(None, &format!("    <rdf:Description rdf:about=\"\" xmlns:pan=\"{PAN_NS}\">\n      <pan:id>&lt;pan/Image/abcd2345&gt;</pan:id>\n      <pan:createdDate>2026-09-16T12:00:00-07:00</pan:createdDate>\n    </rdf:Description>\n"));
        let err = read_imageset_file(&text).unwrap_err().to_string();
        assert!(err.contains("not <pan/ImageSet/id>"), "{err}");
    }
}
