//! The Instance node: one `pan:Instance` per store, written by pand at open.
//!
//! pan.ttl 0.4.1 declared the class ("one pand deployment ... written by pand
//! at start; never authored") and nothing wrote it, so a reader asking "which
//! mode is this store in" got silence and could not tell "not written yet"
//! from "not in the vocabulary" (pan issue #28). Ruled by goodlux,
//! 2026-09-17: the Instance is whatever pand is running over, so every store
//! the daemon opens carries the daemon's Instance node, and the store answers
//! on its own.
//!
//! IDENTITY. The Instance's id is the filepath to the instance: the absolute
//! storage root the daemon runs over (the configured `media_volume`, e.g.
//! `/Volumes/f00/_pan`), percent-encoded into one IRI segment —
//! `<pan/Instance/%2FVolumes%2Ff00%2F_pan>`. Ruled by goodlux, 2026-09-17.
//! One daemon, one root, one id, the same in every store it opens; and
//! `pan:fsRoot` is that same path in the clear. The encoding is lossless and
//! never decoded on read: the segment IS the id, the path is the fact. (The
//! first landing derived the id from the machine's hostname; that was the
//! fork's guess and was wrong.) Not the port — a port is a setting on the
//! instance (`pan:listenPort`), not what the instance IS — and not the store
//! id, which would say a store has an instance rather than an instance has
//! stores.
//!
//! The bare `pan` command opens stores without a daemon and writes no
//! Instance: there is none to describe.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};

use crate::config::{now_local, PAN_MEDIA_NS};
use crate::{enrich, pan_iri, rdf_type, term_str, Pan, QueryResults};

/// What the daemon knows about itself that the store cannot derive.
#[derive(Debug, Clone)]
pub struct InstanceFacts {
    /// The absolute storage root the daemon runs over: the instance's
    /// identity (`instance_id_from_root`) and its `pan:fsRoot`.
    pub root: PathBuf,
    /// The daemon's HTTP base, `http://<bind>:<port>`; the store's SPARQL
    /// endpoint under it is what `pan:primaryGraph` names.
    pub base_url: String,
    /// `pan:listenPort`.
    pub listen_port: u16,
}

/// `pan:instanceMode` for a pand that copies bytes in: the only mode built.
pub const INSTANCE_MODE_MANAGED: &str = "managed";
/// `pan:sourceFormat` of a managed store: PNG only (pan.ttl 0.4.3).
pub const SOURCE_FORMAT_PNG: &str = "image/png";

/// The storage root as one IRI segment: every byte outside the RFC 3986
/// unreserved set `[A-Za-z0-9._~-]` is percent-encoded, uppercase hex, so
/// `/Volumes/f00/_pan` becomes `%2FVolumes%2Ff00%2F_pan`. Lossless; never
/// decoded on read.
pub fn instance_id_from_root(root: &Path) -> String {
    let bytes = root.as_os_str().as_encoded_bytes();
    let mut id = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-') {
            id.push(b as char);
        } else {
            id.push_str(&format!("%{b:02X}"));
        }
    }
    id
}

/// The root this daemon runs over: the configured media volume, else the
/// daemon's own directory (the config file's parent).
pub fn instance_root(cfg: &crate::daemon::config::DaemonConfig) -> PathBuf {
    match &cfg.media_volume {
        Some(v) => v.clone(),
        None => cfg
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cfg.path.clone()),
    }
}

impl InstanceFacts {
    /// `<pan/Instance/<encoded root>>`'s last segment.
    pub fn id(&self) -> String {
        instance_id_from_root(&self.root)
    }
}

pub fn instance_iri(id: &str) -> Result<NamedNode> {
    NamedNode::new(format!("{PAN_MEDIA_NS}Instance/{id}")).map_err(|e| anyhow!("instance IRI: {e}"))
}

impl Pan {
    /// Put the daemon's Instance node in this store: type, `pan:id`,
    /// `pan:createdDate`, and the declared fields — `pan:fsRoot` (the
    /// instance's storage root, absolute, the same in every store),
    /// `pan:instanceMode`, `pan:sourceFormat`,
    /// `pan:listenPort`, `pan:primaryGraph` (the store's SPARQL endpoint).
    /// `pan:localGraph` is not written: pand keeps no cache graph.
    ///
    /// Exactly one Instance per store, always: every node typed
    /// `pan:Instance` is removed first, so a root moved or a port moved
    /// leaves no second node behind. The creation date survives a rewrite of
    /// the same id, since the record is the same record.
    pub fn declare_instance(&self, facts: &InstanceFacts) -> Result<()> {
        let node = instance_iri(&facts.id())?;
        let existing: Vec<NamedNode> = match self.query("SELECT ?s WHERE { ?s a pan:Instance }")? {
            QueryResults::Solutions(sols) => sols
                .filter_map(|r| r.ok())
                .filter_map(|r| r.get("s").cloned())
                .filter_map(|t| match t {
                    Term::NamedNode(n) => Some(n),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut old: Vec<Quad> = Vec::new();
        for s in &existing {
            let quads: Vec<Quad> = self
                .store
                .quads_for_pattern(
                    Some(s.into()),
                    None,
                    None,
                    Some(GraphName::DefaultGraph.as_ref()),
                )
                .collect::<std::result::Result<_, _>>()
                .context("read instance node")?;
            old.extend(quads);
        }
        let created_date = old
            .iter()
            .find(|q| q.subject == node.clone().into() && q.predicate == pan_iri("createdDate"))
            .map(|q| term_str(&q.object.clone()))
            .unwrap_or_else(now_local);

        let fs_root = facts.root.to_string_lossy().to_string();
        let endpoint = format!(
            "{}/stores/{}/sparql",
            facts.base_url.trim_end_matches('/'),
            self.store_id
        );
        let mut t = self
            .store
            .start_transaction()
            .context("start transaction")?;
        for q in &old {
            t.remove(q.as_ref());
        }
        t.insert(
            Quad::new(
                node.clone(),
                rdf_type(),
                pan_iri("Instance"),
                GraphName::DefaultGraph,
            )
            .as_ref(),
        );
        t.insert(enrich::self_id_quad(&node)?.as_ref());
        t.insert(self.quad(&node, "createdDate", &created_date).as_ref());
        t.insert(self.quad(&node, "fsRoot", &fs_root).as_ref());
        t.insert(
            self.quad(&node, "instanceMode", INSTANCE_MODE_MANAGED)
                .as_ref(),
        );
        t.insert(self.quad(&node, "sourceFormat", SOURCE_FORMAT_PNG).as_ref());
        t.insert(self.quad(&node, "primaryGraph", &endpoint).as_ref());
        t.insert(
            Quad::new(
                node.clone(),
                pan_iri("listenPort"),
                Literal::new_typed_literal(
                    facts.listen_port.to_string(),
                    NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
                ),
                GraphName::DefaultGraph,
            )
            .as_ref(),
        );
        t.commit().context("commit instance node")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::instance_id_from_root;
    use std::path::Path;

    #[test]
    fn a_root_becomes_one_percent_encoded_segment() {
        assert_eq!(
            instance_id_from_root(Path::new("/Volumes/f00/_pan")),
            "%2FVolumes%2Ff00%2F_pan"
        );
        // A space and a non-ASCII character: every byte outside the
        // unreserved set is encoded, nothing is lost.
        assert_eq!(
            instance_id_from_root(Path::new("/Volumes/my disk/pän")),
            "%2FVolumes%2Fmy%20disk%2Fp%C3%A4n"
        );
        assert_eq!(instance_id_from_root(Path::new("a.b~c-d_e")), "a.b~c-d_e");
    }

    #[test]
    fn the_encoded_segment_survives_the_bracket_and_iri_forms_unchanged() {
        let id = instance_id_from_root(Path::new("/Volumes/my disk/pän"));
        let iri = format!("https://repolex.ai/pan/Instance/{id}");
        let bracket = crate::xmp::bracket_of_iri(&iri);
        assert_eq!(bracket, format!("<pan/Instance/{id}>"));
        assert_eq!(
            crate::iri_from_bracket(&bracket).as_deref(),
            Some(iri.as_str())
        );
    }
}
