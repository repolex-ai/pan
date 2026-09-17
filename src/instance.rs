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
//! IDENTITY. One machine runs one daemon (the config is per machine), so the
//! Instance's id is the machine's name: `<pan/Instance/<hostname>>`, the
//! host name lowercased and cut at its first dot. Not the port — a port is a
//! setting on the instance (`pan:listenPort`), not what the instance IS —
//! and not the store id, which would say a store has an instance rather than
//! an instance has stores. The same discipline as the Store node, whose id is
//! the soul's genesis SHA: an identity the world already uses.
//!
//! The bare `pan` command opens stores without a daemon and writes no
//! Instance: there is none to describe.

use anyhow::{anyhow, Context, Result};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};

use crate::config::{now_local, PAN_MEDIA_NS};
use crate::{enrich, pan_iri, rdf_type, term_str, Pan, QueryResults};

/// What the daemon knows about itself that the store cannot derive.
#[derive(Debug, Clone)]
pub struct InstanceFacts {
    /// The instance id (`instance_id_from_hostname`).
    pub id: String,
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

/// The machine's name as an id: lowercased, cut at the first dot, every
/// character outside `[a-z0-9-]` replaced by `-`. `mac-studio.local` and
/// `Mac-Studio` both become `mac-studio`.
pub fn instance_id_from_hostname(hostname: &str) -> String {
    let head = hostname.split('.').next().unwrap_or("").trim();
    let id: String = head
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if id.is_empty() {
        "localhost".to_string()
    } else {
        id
    }
}

/// This machine's Instance id.
pub fn local_instance_id() -> String {
    instance_id_from_hostname(&gethostname::gethostname().to_string_lossy())
}

pub fn instance_iri(id: &str) -> Result<NamedNode> {
    NamedNode::new(format!("{PAN_MEDIA_NS}Instance/{id}")).map_err(|e| anyhow!("instance IRI: {e}"))
}

impl Pan {
    /// Put the daemon's Instance node in this store: type, `pan:id`,
    /// `pan:createdDate`, and the declared fields — `pan:fsRoot` (the media
    /// root, absolute), `pan:instanceMode`, `pan:sourceFormat`,
    /// `pan:listenPort`, `pan:primaryGraph` (the store's SPARQL endpoint).
    /// `pan:localGraph` is not written: pand keeps no cache graph.
    ///
    /// Exactly one Instance per store, always: every node typed
    /// `pan:Instance` is removed first, so a machine renamed or a port moved
    /// leaves no second node behind. The creation date survives a rewrite of
    /// the same id, since the record is the same record.
    pub fn declare_instance(&self, facts: &InstanceFacts) -> Result<()> {
        let node = instance_iri(&facts.id)?;
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

        let media_root = self.layout.media_root.to_string_lossy().to_string();
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
        t.insert(self.quad(&node, "fsRoot", &media_root).as_ref());
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
    use super::instance_id_from_hostname;

    #[test]
    fn a_hostname_becomes_a_lowercase_id_cut_at_the_first_dot() {
        assert_eq!(instance_id_from_hostname("Mac-Studio.local"), "mac-studio");
        assert_eq!(instance_id_from_hostname("robs mac"), "robs-mac");
        assert_eq!(instance_id_from_hostname(""), "localhost");
        assert_eq!(instance_id_from_hostname(".local"), "localhost");
    }
}
