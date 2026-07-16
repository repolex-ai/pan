//! Facts — the flat `predicate → values` contract surface for describing media.
//!
//! Keys are predicate names — either:
//!   - full IRIs (http://, https://, urn:)        → used verbatim
//!   - prefixed short forms ("copia:sceneMood")   → expanded via the prefix map
//!   - bare names ("sceneMood")                   → expanded via the default prefix
//!
//! Carried from Pool's facts.rs with ONE deliberate change: resolution failures
//! are LOUD. Pool silently skipped unresolvable predicates (the same
//! silent-drop-on-name-mismatch disease as its allowlist gate — a typo made a
//! fact vanish with zero feedback). Pan errors instead: a predicate that can't
//! resolve fails the whole describe, and the caller hears about it.

use anyhow::{anyhow, Result};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub map: HashMap<String, Vec<String>>,
}

impl Facts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, predicate: impl Into<String>, value: impl Into<String>) {
        self.map
            .entry(predicate.into())
            .or_default()
            .push(value.into());
    }

    /// Replace the value list for a predicate (set, not append).
    pub fn set(&mut self, predicate: impl Into<String>, values: Vec<String>) {
        self.map.insert(predicate.into(), values);
    }

    pub fn get(&self, predicate: &str) -> Option<&Vec<String>> {
        self.map.get(predicate)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn with(mut self, predicate: impl Into<String>, value: impl Into<String>) -> Self {
        self.insert(predicate, value);
        self
    }

    /// Resolve every predicate and emit quads into the DEFAULT graph.
    ///
    /// LOUD: any unresolvable predicate (unknown prefix, invalid IRI) fails the
    /// whole call — nothing is partially written by this fn (the caller inserts
    /// the returned quads only on Ok).
    pub fn into_quads(self, subject: &NamedNode, prefixes: &HashMap<String, String>, default_prefix: &str) -> Result<Vec<Quad>> {
        let mut out = Vec::new();
        for (pred, values) in self.map {
            let p = resolve_predicate(&pred, prefixes, default_prefix)
                .map_err(|e| anyhow!("predicate {pred:?}: {e}"))?;
            for v in values {
                out.push(Quad::new(
                    subject.clone(),
                    p.clone(),
                    Literal::new_simple_literal(&v),
                    GraphName::DefaultGraph,
                ));
            }
        }
        Ok(out)
    }
}

/// Expand a predicate name to a full IRI. Lifted verbatim from Pool.
pub fn resolve_predicate(
    pred: &str,
    prefixes: &HashMap<String, String>,
    default_prefix: &str,
) -> Result<NamedNode> {
    // Case 1: already a full IRI.
    if pred.starts_with("http://") || pred.starts_with("https://") || pred.starts_with("urn:") {
        return NamedNode::new(pred).map_err(|e| anyhow!("invalid full IRI: {e}"));
    }
    // Case 2: prefix:local form.
    if let Some(idx) = pred.find(':') {
        let (short, rest) = pred.split_at(idx);
        let local = &rest[1..];
        let ns = prefixes.get(short).ok_or_else(|| {
            anyhow!(
                "unknown prefix '{short}' (registered: {:?})",
                prefixes.keys().collect::<Vec<_>>()
            )
        })?;
        return NamedNode::new(format!("{ns}{local}"))
            .map_err(|e| anyhow!("invalid IRI {ns}{local}: {e}"));
    }
    // Case 3: bare local name → default prefix.
    let ns = prefixes
        .get(default_prefix)
        .ok_or_else(|| anyhow!("default prefix '{default_prefix}' not registered"))?;
    NamedNode::new(format!("{ns}{pred}")).map_err(|e| anyhow!("invalid IRI {ns}{pred}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("pan".to_string(), "https://repolex.ai/ontology/pan/".to_string());
        m.insert("dc".to_string(), "http://purl.org/dc/elements/1.1/".to_string());
        m
    }

    #[test]
    fn resolves_all_three_forms() {
        let p = prefixes();
        assert_eq!(
            resolve_predicate("https://x.io/p", &p, "pan").unwrap().as_str(),
            "https://x.io/p"
        );
        assert_eq!(
            resolve_predicate("dc:title", &p, "pan").unwrap().as_str(),
            "http://purl.org/dc/elements/1.1/title"
        );
        assert_eq!(
            resolve_predicate("cid", &p, "pan").unwrap().as_str(),
            "https://repolex.ai/ontology/pan/cid"
        );
    }

    #[test]
    fn unknown_prefix_fails_loud() {
        // The anti-silent-drop contract: a typo'd prefix is an ERROR, not a
        // vanished fact.
        let facts = Facts::new().with("copai:sceneMood", "calm");
        let subj = NamedNode::new("urn:sha256:ab").unwrap();
        let err = facts.into_quads(&subj, &prefixes(), "pan").unwrap_err();
        assert!(err.to_string().contains("unknown prefix 'copai'"), "got: {err}");
    }

    #[test]
    fn multi_value_predicate_emits_one_quad_each() {
        let mut facts = Facts::new();
        facts.insert("dc:subject", "wolves");
        facts.insert("dc:subject", "forest");
        let subj = NamedNode::new("urn:sha256:ab").unwrap();
        let quads = facts.into_quads(&subj, &prefixes(), "pan").unwrap();
        assert_eq!(quads.len(), 2);
    }
}
