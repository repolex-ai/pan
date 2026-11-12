//! MediaSets — sets of media a person curates (pan.ttl 0.4.2, goodlux
//! 2026-09-16; issue #4; renamed from Photoset with pan.ttl 0.4.7, goodlux
//! 2026-09-17: pan:MediaSet is the parent, a set of any media kind, and each
//! kind has its own class so a UI can pick its handler from the class alone).
//! Two kinds are written: pan:ImageSet (images only, since 0.4.2) and
//! pan:VideoSet (videos only, since 0.4.26, goodlux 2026-11-12, for the clips
//! of one film). One module, one rule set, the kind a parameter.
//!
//! A set is a `subtexture:Set` with exactly three facts: its id, its
//! description, and when it was created. It has NO member list. Membership
//! is the universal edge from the media: `pan:relatedToId <pan/ImageSet/id>`
//! (or `<pan/VideoSet/id>`) in the file's XMP and in the graph. Ask "which
//! media are in set X" and the answer is a graph pattern, never a list kept
//! on the set. Only the kind's media class may be added; the class name is
//! the promise.
//!
//! Every set has its own file, `<store>/_ignore/<Class>/<id>.nq` — the folder
//! named for the class (goodlux, 2026-09-21), the file N-Quads like every
//! record Pan writes: the set's own quads, pan: vocabulary only, each line
//! naming Pan's graph in its fourth column. The file is the source of truth:
//! on every open the store reads `<Class>/*.nq` and rewrites the set nodes
//! in the graph from them, so the graph is rebuilt from files alone.

use anyhow::{anyhow, Context, Result};
use oxigraph::model::{Literal, NamedNode, Quad, Term};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

use crate::config::{PAN_MEDIA_NS, PAN_NS};
use crate::{
    bare_id, enrich, gen_pan_id, now_local, pan_iri, validate_pan_id, write_atomic, Pan, PanLayout,
};

/// One kind of set: the set class, the one media class it holds, and the
/// word the command line and the routes use for it.
#[derive(Debug, PartialEq, Eq)]
pub struct SetKind {
    /// The ontology class and the IRI path segment: `<pan/ImageSet/id>`.
    pub class: &'static str,
    /// The media class a member must have: `pan:Image`, `pan:Video`.
    pub member: &'static str,
    /// The command word and error noun: `imageset`, `videoset`.
    pub noun: &'static str,
}

pub const IMAGESET: SetKind = SetKind {
    class: "ImageSet",
    member: "Image",
    noun: "imageset",
};
pub const VIDEOSET: SetKind = SetKind {
    class: "VideoSet",
    member: "Video",
    noun: "videoset",
};
/// Every kind of set Pan writes.
pub const SET_KINDS: [&SetKind; 2] = [&IMAGESET, &VIDEOSET];

/// The ontology class and the IRI path segment: `<pan/ImageSet/id>`.
pub const IMAGESET_CLASS: &str = IMAGESET.class;

/// The kind whose class name this is, if any.
pub fn set_kind_of_class(class: &str) -> Option<&'static SetKind> {
    SET_KINDS.iter().copied().find(|k| k.class == class)
}

/// The kind a `<pan/Class/id>` IRI names, with the bare id, if it is a set.
pub fn set_kind_of_iri(iri: &str) -> Option<(&'static SetKind, String)> {
    let tail = iri.strip_prefix(PAN_MEDIA_NS)?;
    for kind in SET_KINDS {
        if let Some(id) = tail.strip_prefix(&format!("{}/", kind.class)) {
            return Some((kind, id.to_string()));
        }
    }
    None
}

/// One set as the graph and its file hold it. Nothing else is on a set.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MediaSet {
    /// The set class: `ImageSet` or `VideoSet`.
    pub class: String,
    /// The bare id, the same token the file name and the IRI carry.
    pub id: String,
    /// `https://repolex.ai/pan/<Class>/<id>` — the identity (`pan:id`).
    pub iri: String,
    /// `pan:description`, in the file and in the graph alike.
    /// At most one.
    pub description: Option<String>,
    /// `pan:createdDate`, in the file and in the graph alike.
    pub created_date: String,
    /// `pan:updatedDate`: when the set's file was last written. Equal to
    /// `created_date` at creation; absent on a file from before 0.4.20.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_date: Option<String>,
}

/// The name the code used while only one kind existed.
pub type ImageSet = MediaSet;

impl MediaSet {
    pub fn kind(&self) -> Result<&'static SetKind> {
        set_kind_of_class(&self.class)
            .ok_or_else(|| anyhow!("{} is not a set class Pan writes", self.class))
    }
}

pub fn mediaset_iri(kind: &SetKind, id: &str) -> Result<NamedNode> {
    validate_pan_id(id)?;
    NamedNode::new(format!("{PAN_MEDIA_NS}{}/{id}", kind.class))
        .map_err(|e| anyhow!("{} IRI: {e}", kind.noun))
}

pub fn imageset_iri(id: &str) -> Result<NamedNode> {
    mediaset_iri(&IMAGESET, id)
}

/// The set's file, `<Class>/<id>.nq`: the set's own quads and nothing
/// else — type, identity, creation time, and the description when it has one.
/// The same quads the graph holds, serialized.
pub fn build_mediaset_file(p: &MediaSet) -> Result<String> {
    enrich::quads_to_nquads(&mediaset_quads(p)?)
}

pub fn build_imageset_file(p: &MediaSet) -> Result<String> {
    build_mediaset_file(p)
}

/// The facts of a set as quads: type, identity, creation time, description —
/// spelled pan: in the graph exactly as in the file (goodlux, 2026-09-17).
fn mediaset_quads(p: &MediaSet) -> Result<Vec<Quad>> {
    let kind = p.kind()?;
    let node = mediaset_iri(kind, &p.id)?;
    let mut quads = vec![
        Quad::new(
            node.clone(),
            crate::rdf_type(),
            pan_iri(kind.class),
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
    if let Some(u) = &p.updated_date {
        quads.push(Quad::new(
            node.clone(),
            pan_iri("updatedDate"),
            Literal::new_simple_literal(u),
            crate::config::pan_graph(),
        ));
    }
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

/// Read a set back from its file text. Strict: the file must describe exactly
/// one node, typed with the kind's class, whose `pan:id` is the node itself
/// at `<pan/<Class>/id>`, with a `pan:createdDate`, every line in Pan's
/// graph. A file that says less, or more, is an error, never a half-set.
pub fn read_mediaset_file(kind: &SetKind, text: &str) -> Result<MediaSet> {
    let noun = kind.noun;
    let quads: Vec<Quad> = oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads)
        .for_reader(text.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parse {noun} file"))?;
    let graph = crate::config::pan_graph();
    if let Some(q) = quads.iter().find(|q| q.graph_name != graph) {
        return Err(anyhow!(
            "{noun} file has a line outside <pan/NamedGraph/pan>: {q}"
        ));
    }
    let id_pred = pan_iri("id");
    let id_quad = quads
        .iter()
        .find(|q| q.predicate == id_pred)
        .ok_or_else(|| anyhow!("{noun} file has no pan:id"))?;
    let (oxigraph::model::NamedOrBlankNode::NamedNode(node), Term::NamedNode(target)) =
        (&id_quad.subject, &id_quad.object)
    else {
        return Err(anyhow!("{noun} pan:id is not <pan/{}/id>", kind.class));
    };
    let id = node
        .as_str()
        .strip_prefix(&format!("{PAN_MEDIA_NS}{}/", kind.class))
        .filter(|_| node == target)
        .ok_or_else(|| anyhow!("{noun} pan:id is not <pan/{}/id>: {node}", kind.class))?
        .to_string();
    validate_pan_id(&id)?;
    let mut created_date = None;
    let mut updated_date = None;
    let mut description = None;
    let mut typed = false;
    for q in &quads {
        if q.subject != id_quad.subject {
            return Err(anyhow!(
                "{noun} file describes a second node, {}; a set's file holds the set and nothing else",
                q.subject
            ));
        }
        let local = q.predicate.as_str().strip_prefix(PAN_NS);
        match (&q.object, local) {
            (Term::NamedNode(o), None)
                if q.predicate == crate::rdf_type() && *o == pan_iri(kind.class) =>
            {
                typed = true
            }
            (_, Some("id")) => {}
            (Term::Literal(l), Some("createdDate")) => created_date = Some(l.value().to_string()),
            (Term::Literal(l), Some("updatedDate")) => updated_date = Some(l.value().to_string()),
            (Term::Literal(l), Some("description")) => description = Some(l.value().to_string()),
            _ => {
                return Err(anyhow!(
                    "{noun} file says something a set does not carry: {q}"
                ))
            }
        }
    }
    if !typed {
        return Err(anyhow!("{noun} file does not type its node pan:{}", kind.class));
    }
    let created_date =
        created_date.ok_or_else(|| anyhow!("{noun} file has no pan:createdDate"))?;
    Ok(MediaSet {
        class: kind.class.to_string(),
        id,
        iri: node.as_str().to_string(),
        description,
        created_date,
        updated_date,
    })
}

pub fn read_imageset_file(text: &str) -> Result<MediaSet> {
    read_mediaset_file(&IMAGESET, text)
}

impl Pan {
    /// `<root>/_ignore/<Class>` — one file per set.
    pub fn mediasets_root(&self, kind: &SetKind) -> PathBuf {
        self.layout.mediasets_root(kind)
    }

    /// `<root>/_ignore/ImageSet` — one file per set.
    pub fn imagesets_root(&self) -> PathBuf {
        self.mediasets_root(&IMAGESET)
    }

    fn mediaset_file(&self, kind: &SetKind, id: &str) -> PathBuf {
        self.mediasets_root(kind).join(format!("{id}.nq"))
    }

    /// Resolve a bare id to the set's IRI, if the graph holds a set of this
    /// kind by it.
    pub fn mediaset_subject(&self, kind: &SetKind, id: &str) -> Result<Option<NamedNode>> {
        if validate_pan_id(id).is_err() {
            return Ok(None);
        }
        let node = mediaset_iri(kind, id)?;
        let exists = self
            .store
            .quads_for_pattern(
                Some((&node).into()),
                Some(crate::rdf_type().as_ref()),
                Some(pan_iri(kind.class).as_ref().into()),
                Some(crate::config::pan_graph().as_ref()),
            )
            .next()
            .is_some();
        Ok(exists.then_some(node))
    }

    pub fn imageset_subject(&self, id: &str) -> Result<Option<NamedNode>> {
        self.mediaset_subject(&IMAGESET, id)
    }

    pub fn videoset_subject(&self, id: &str) -> Result<Option<NamedNode>> {
        self.mediaset_subject(&VIDEOSET, id)
    }

    /// Put the set's node in the graph exactly as `p` says: every statement
    /// the node had is removed first, so the file always wins.
    fn write_mediaset_node(&self, p: &MediaSet) -> Result<()> {
        let node = mediaset_iri(p.kind()?, &p.id)?;
        let old: Vec<Quad> = self
            .store
            .quads_for_pattern(
                Some((&node).into()),
                None,
                None,
                Some(crate::config::pan_graph().as_ref()),
            )
            .collect::<std::result::Result<_, _>>()
            .context("read set node")?;
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        for q in &old {
            t.remove(q.as_ref());
        }
        for q in mediaset_quads(p)? {
            t.insert(q.as_ref());
        }
        t.commit().context("commit set")?;
        Ok(())
    }

    /// Does any set of any kind, or any media object, hold this id?
    fn id_taken(&self, id: &str) -> Result<bool> {
        if self.subject_for(id)?.is_some() {
            return Ok(true);
        }
        for kind in SET_KINDS {
            if self.mediaset_subject(kind, id)?.is_some() || self.mediaset_file(kind, id).exists() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Make a new set: a fresh id (never one a media object or a set of any
    /// kind already has), its file written first, then its node in the
    /// graph. Fails before anything is written if the file cannot be;
    /// removes the file if the graph refuses.
    pub fn mediaset_create(&self, kind: &SetKind, description: Option<&str>) -> Result<MediaSet> {
        let id = loop {
            let cand = gen_pan_id();
            if !self.id_taken(&cand)? {
                break cand;
            }
        };
        self.mediaset_create_with(kind, &id, description)
    }

    pub fn imageset_create(&self, description: Option<&str>) -> Result<MediaSet> {
        self.mediaset_create(&IMAGESET, description)
    }

    pub fn videoset_create(&self, description: Option<&str>) -> Result<MediaSet> {
        self.mediaset_create(&VIDEOSET, description)
    }

    /// Make a set by the id the caller names (goodlux, 2026-09-22, issue
    /// #69: an arriving file that names `<pan/ImageSet/id>` makes the set).
    /// The id is the set's identity and its file name; a producer's long
    /// name is fine. Refused when a set of any kind or a media object
    /// already has it.
    pub fn mediaset_create_with(
        &self,
        kind: &SetKind,
        id: &str,
        description: Option<&str>,
    ) -> Result<MediaSet> {
        validate_pan_id(id)?;
        if self.mediaset_subject(kind, id)?.is_some() || self.mediaset_file(kind, id).exists() {
            return Err(anyhow!("{} {id} already exists", kind.noun));
        }
        if self.id_taken(id)? {
            return Err(anyhow!(
                "invalid {} id {id:?}: a media object or another set has it",
                kind.noun
            ));
        }
        let description = description
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(String::from);
        let created_date = now_local();
        let p = MediaSet {
            class: kind.class.to_string(),
            iri: mediaset_iri(kind, id)?.into_string(),
            id: id.to_string(),
            description,
            updated_date: Some(created_date.clone()),
            created_date,
        };
        let path = self.mediaset_file(kind, &p.id);
        let root = self.mediasets_root(kind);
        fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        write_atomic(&path, build_mediaset_file(&p)?.as_bytes())?;
        if let Err(e) = self.write_mediaset_node(&p) {
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        Ok(p)
    }

    pub fn imageset_create_with(&self, id: &str, description: Option<&str>) -> Result<MediaSet> {
        self.mediaset_create_with(&IMAGESET, id, description)
    }

    /// Take a set's node out of the graph and its file off disk. Members keep
    /// their edge; this exists so a file that failed to land does not leave
    /// the set it named behind.
    pub(crate) fn mediaset_unmake(&self, kind: &SetKind, id: &str) -> Result<()> {
        let node = mediaset_iri(kind, id)?;
        let old: Vec<Quad> = self
            .store
            .quads_for_pattern(
                Some((&node).into()),
                None,
                None,
                Some(crate::config::pan_graph().as_ref()),
            )
            .collect::<std::result::Result<_, _>>()
            .context("read set node")?;
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        for q in &old {
            t.remove(q.as_ref());
        }
        t.commit().context("commit set removal")?;
        let path = self.mediaset_file(kind, id);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        Ok(())
    }

    pub(crate) fn imageset_unmake(&self, id: &str) -> Result<()> {
        self.mediaset_unmake(&IMAGESET, id)
    }

    /// Every set of this kind in the graph, oldest first.
    pub fn mediaset_list(&self, kind: &SetKind) -> Result<Vec<MediaSet>> {
        let mut out = Vec::new();
        for q in self.store.quads_for_pattern(
            None,
            Some(crate::rdf_type().as_ref()),
            Some(pan_iri(kind.class).as_ref().into()),
            Some(crate::config::pan_graph().as_ref()),
        ) {
            let q = q.with_context(|| format!("list {}s", kind.noun))?;
            let oxigraph::model::NamedOrBlankNode::NamedNode(node) = &q.subject else {
                continue;
            };
            if let Some(p) = self.mediaset_of(kind, node)? {
                out.push(p);
            }
        }
        out.sort_by(|a, b| a.created_date.cmp(&b.created_date).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn imageset_list(&self) -> Result<Vec<MediaSet>> {
        self.mediaset_list(&IMAGESET)
    }

    pub fn videoset_list(&self) -> Result<Vec<MediaSet>> {
        self.mediaset_list(&VIDEOSET)
    }

    /// One set by id, from the graph. None = no such set of this kind.
    pub fn mediaset_get(&self, kind: &SetKind, id: &str) -> Result<Option<MediaSet>> {
        match self.mediaset_subject(kind, id)? {
            Some(node) => self.mediaset_of(kind, &node),
            None => Ok(None),
        }
    }

    pub fn imageset_get(&self, id: &str) -> Result<Option<MediaSet>> {
        self.mediaset_get(&IMAGESET, id)
    }

    pub fn videoset_get(&self, id: &str) -> Result<Option<MediaSet>> {
        self.mediaset_get(&VIDEOSET, id)
    }

    fn mediaset_of(&self, kind: &SetKind, node: &NamedNode) -> Result<Option<MediaSet>> {
        let mut created_date = None;
        let mut updated_date = None;
        let mut description = None;
        for q in self.store.quads_for_pattern(
            Some(node.into()),
            None,
            None,
            Some(crate::config::pan_graph().as_ref()),
        ) {
            let q = q.with_context(|| format!("read {}", kind.noun))?;
            let value = match &q.object {
                Term::Literal(l) => l.value().to_string(),
                _ => continue,
            };
            match q.predicate.as_str().strip_prefix(PAN_NS) {
                Some("createdDate") => created_date = Some(value),
                Some("updatedDate") => updated_date = Some(value),
                Some("description") => description = Some(value),
                _ => {}
            }
        }
        let Some(created_date) = created_date else {
            return Ok(None);
        };
        Ok(Some(MediaSet {
            class: kind.class.to_string(),
            id: bare_id(node.as_str()),
            iri: node.as_str().to_string(),
            description,
            created_date,
            updated_date,
        }))
    }

    /// The IRIs of every media object whose `pan:relatedToId` names the set.
    pub fn mediaset_members(&self, kind: &SetKind, id: &str) -> Result<Vec<String>> {
        let Some(node) = self.mediaset_subject(kind, id)? else {
            return Err(anyhow!("{} not found: {id}", kind.noun));
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

    pub fn imageset_members(&self, id: &str) -> Result<Vec<String>> {
        self.mediaset_members(&IMAGESET, id)
    }

    pub fn videoset_members(&self, id: &str) -> Result<Vec<String>> {
        self.mediaset_members(&VIDEOSET, id)
    }

    /// Put a media object in a set: `pan:relatedToId <pan/<Class>/id>` on
    /// the media, in the graph and then in its XMP. Already a member = no
    /// change. Both the set and the media must exist, and the media must be
    /// the kind's class: an ImageSet holds images only, a VideoSet videos
    /// only, so anything else is refused before anything is written.
    pub fn mediaset_add(&self, kind: &SetKind, set_id: &str, media_id: &str) -> Result<()> {
        let edge = self.mediaset_edge(kind, set_id, media_id)?;
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

    pub fn imageset_add(&self, set_id: &str, media_id: &str) -> Result<()> {
        self.mediaset_add(&IMAGESET, set_id, media_id)
    }

    pub fn videoset_add(&self, set_id: &str, media_id: &str) -> Result<()> {
        self.mediaset_add(&VIDEOSET, set_id, media_id)
    }

    /// The membership edge, checked but not written: the set exists, the
    /// media exists, and the media is the kind's class. Shared with
    /// `set relatedToId=<pan/<Class>/id>` and with a file that arrives
    /// naming the set, so every door applies one rule.
    pub(crate) fn mediaset_edge(&self, kind: &SetKind, set_id: &str, media_id: &str) -> Result<Quad> {
        let Some(set) = self.mediaset_subject(kind, set_id)? else {
            return Err(anyhow!("{} not found: {set_id}", kind.noun));
        };
        let Some(media) = self.subject_for(media_id)? else {
            return Err(anyhow!("id not found: {media_id}"));
        };
        self.check_member_class(kind, &media, media_id)?;
        Ok(Quad::new(
            media,
            pan_iri("relatedToId"),
            set,
            crate::config::pan_graph(),
        ))
    }

    /// The media is the class this kind of set holds, or an error naming
    /// what it is instead.
    pub(crate) fn check_member_class(&self, kind: &SetKind, media: &NamedNode, media_id: &str) -> Result<()> {
        let is_member_class = self
            .store
            .contains(
                Quad::new(
                    media.clone(),
                    crate::rdf_type(),
                    pan_iri(kind.member),
                    crate::config::pan_graph(),
                )
                .as_ref(),
            )
            .context("check media class")?;
        if is_member_class {
            return Ok(());
        }
        let class = self
            .store
            .quads_for_pattern(
                Some(media.into()),
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
        let other: Vec<&str> = SET_KINDS
            .iter()
            .filter(|k| k.member == class)
            .map(|k| k.class)
            .collect();
        let hint = match other.first() {
            Some(c) => format!(" A pan:{c} is the set for that media kind."),
            None => " A set for that media kind is not declared yet.".to_string(),
        };
        Err(anyhow!(
            "{media_id} is a pan:{class}, not a pan:{}; a {} takes {}s only.{hint}",
            kind.member,
            kind.class,
            kind.member.to_ascii_lowercase()
        ))
    }

    /// Take a media object out of a set. Not a member = no change.
    pub fn mediaset_remove(&self, kind: &SetKind, set_id: &str, media_id: &str) -> Result<()> {
        let Some(set) = self.mediaset_subject(kind, set_id)? else {
            return Err(anyhow!("{} not found: {set_id}", kind.noun));
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

    pub fn imageset_remove(&self, set_id: &str, media_id: &str) -> Result<()> {
        self.mediaset_remove(&IMAGESET, set_id, media_id)
    }

    pub fn videoset_remove(&self, set_id: &str, media_id: &str) -> Result<()> {
        self.mediaset_remove(&VIDEOSET, set_id, media_id)
    }

    /// Destroy a set: every member's `pan:relatedToId` to it comes off, in
    /// the graph and in each member's file, one member at a time (a member
    /// whose file will not rewrite stops the deletion there, with the set
    /// and the members not yet reached still intact); then the set's node
    /// and file go. Returns the members that were unlinked. Asked by
    /// goodlux, 2026-11-12.
    pub fn mediaset_delete(&self, kind: &SetKind, id: &str) -> Result<Vec<String>> {
        if self.mediaset_subject(kind, id)?.is_none() {
            return Err(anyhow!("{} not found: {id}", kind.noun));
        }
        let members = self.mediaset_members(kind, id)?;
        for m in &members {
            let media_id = crate::bare_id(m);
            self.mediaset_remove(kind, id, &media_id).with_context(|| {
                format!("unlink {m} from <pan/{}/{id}>; the set still exists", kind.class)
            })?;
        }
        self.mediaset_unmake(kind, id)?;
        Ok(members)
    }

    pub fn imageset_delete(&self, id: &str) -> Result<Vec<String>> {
        self.mediaset_delete(&IMAGESET, id)
    }

    pub fn videoset_delete(&self, id: &str) -> Result<Vec<String>> {
        self.mediaset_delete(&VIDEOSET, id)
    }

    /// Rebuild every set node of every kind from `<Class>/*.nq`. Called at
    /// every open, so the graph never says a set the files do not. A file
    /// whose id does not match its name, or that is not a set at all, is an
    /// error: the store does not open over a set it cannot read. Returns
    /// how many sets were loaded.
    pub(crate) fn load_mediasets(&self) -> Result<usize> {
        let mut n = 0;
        for kind in SET_KINDS {
            let root = self.mediasets_root(kind);
            if !root.is_dir() {
                continue;
            }
            let mut entries: Vec<PathBuf> = fs::read_dir(&root)
                .with_context(|| format!("read {}", root.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension().and_then(|e| e.to_str()) == Some("nq")
                        && !p
                            .file_name()
                            .and_then(|f| f.to_str())
                            .unwrap_or("")
                            .starts_with('.')
                })
                .collect();
            entries.sort();
            for path in entries {
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?;
                let p = read_mediaset_file(kind, &text)
                    .with_context(|| format!("{} file {}", kind.noun, path.display()))?;
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if stem != p.id {
                    return Err(anyhow!(
                        "{} file {} carries pan:id <pan/{}/{}>; the file name and the id must agree",
                        kind.noun,
                        path.display(),
                        kind.class,
                        p.id
                    ));
                }
                self.write_mediaset_node(&p)?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub(crate) fn load_imagesets(&self) -> Result<usize> {
        self.load_mediasets()
    }
}

impl PanLayout {
    /// `<root>/_ignore/<Class>`: store data, not soul material (goodlux,
    /// 2026-09-26, after the Pool migration put 1,893 three-line set files
    /// into a soul repo's history). Sets are made and edited through Pan.
    pub fn mediasets_root(&self, kind: &SetKind) -> PathBuf {
        self.pocket.join(kind.class)
    }

    pub fn imagesets_root(&self) -> PathBuf {
        self.mediasets_root(&IMAGESET)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MediaSet {
        MediaSet {
            class: "ImageSet".into(),
            id: "abcd2345".into(),
            iri: format!("{PAN_MEDIA_NS}ImageSet/abcd2345"),
            description: Some("portraits & <tests>".into()),
            created_date: "2026-09-16T12:00:00-07:00".into(),
            updated_date: Some("2026-09-16T12:00:00-07:00".into()),
        }
    }

    #[test]
    fn a_set_file_round_trips_its_facts_and_nothing_else() {
        let p = sample();
        let text = build_imageset_file(&p).unwrap();
        let node = format!("<{}>", p.iri);
        let graph = format!("<{}> .", crate::config::PAN_GRAPH_IRI);
        assert_eq!(
            text.lines().count(),
            5,
            "type, id, created, updated, description: {text}"
        );
        assert!(
            text.contains(&format!(
                "<{PAN_NS}updatedDate> \"2026-09-16T12:00:00-07:00\""
            )),
            "{text}"
        );
        assert!(
            text.lines()
                .all(|l| l.starts_with(&node) && l.ends_with(&graph)),
            "every line is about the set, in Pan's graph: {text}"
        );
        assert!(
            text.contains(&format!("{node} <{PAN_NS}id> {node} ")),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "<{PAN_NS}createdDate> \"2026-09-16T12:00:00-07:00\""
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("<{PAN_NS}description> \"portraits & <tests>\"")),
            "{text}"
        );
        assert!(!text.contains("git-lex"), "the file carries pan: only");
        assert!(!text.contains("relatedToId"), "no member list on a set");
        assert_eq!(read_imageset_file(&text).unwrap(), p);
    }

    #[test]
    fn a_videoset_file_is_the_same_shape_under_its_own_class() {
        let mut p = sample();
        p.class = "VideoSet".into();
        p.iri = format!("{PAN_MEDIA_NS}VideoSet/abcd2345");
        let text = build_mediaset_file(&p).unwrap();
        assert!(text.contains(&format!("<{PAN_NS}VideoSet>")), "{text}");
        assert_eq!(read_mediaset_file(&VIDEOSET, &text).unwrap(), p);
        // The kinds do not read each other's files.
        let err = read_mediaset_file(&IMAGESET, &text).unwrap_err().to_string();
        assert!(err.contains("not <pan/ImageSet/id>"), "{err}");
        assert_eq!(
            set_kind_of_iri(&p.iri).map(|(k, id)| (k.class, id)),
            Some(("VideoSet", "abcd2345".to_string()))
        );
        assert_eq!(set_kind_of_iri(&format!("{PAN_MEDIA_NS}Image/abcd2345")), None);
    }

    #[test]
    fn a_set_file_that_says_too_little_or_too_much_is_refused() {
        let g = crate::config::PAN_GRAPH_IRI;
        let set = format!("<{PAN_MEDIA_NS}ImageSet/abcd2345>");
        let no_id = format!("{set} <{PAN_NS}description> \"x\" <{g}> .\n");
        let err = read_imageset_file(&no_id).unwrap_err().to_string();
        assert!(err.contains("pan:id"), "{err}");

        let image = format!("<{PAN_MEDIA_NS}Image/abcd2345>");
        let wrong_class = format!(
            "{image} <{PAN_NS}id> {image} <{g}> .\n{image} <{PAN_NS}createdDate> \"2026-09-16T12:00:00-07:00\" <{g}> .\n"
        );
        let err = read_imageset_file(&wrong_class).unwrap_err().to_string();
        assert!(err.contains("not <pan/ImageSet/id>"), "{err}");

        let good = build_imageset_file(&sample()).unwrap();
        let extra = format!("{good}{set} <{PAN_NS}rating> \"5\" <{g}> .\n");
        let err = read_imageset_file(&extra).unwrap_err().to_string();
        assert!(err.contains("does not carry"), "{err}");

        let outside = good.replace(&format!(" <{g}> ."), " .");
        let err = read_imageset_file(&outside).unwrap_err().to_string();
        assert!(err.contains("outside"), "{err}");
    }
}
