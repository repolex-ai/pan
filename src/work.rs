//! The work list pand reads instead of asking the graph on every pass
//! (issue #71, goodlux 2026-09-27).
//!
//! With 200,000 images in a store, "which images still need this stage"
//! asked as a SPARQL query costs 12.8 s per stage per pass, and "which
//! images are ready to mark" costs 1.2 s every five seconds while nothing
//! changes. The graph stays the record; this is a copy of the few facts the
//! ladder reads, built once from the graph when first needed and kept
//! current by every write that goes through the store: an ingest adds a
//! row, a stage's record marks its image done, a ready mark marks it
//! complete, a delete removes it. The build takes seconds once; a pass then
//! costs microseconds.

use oxigraph::model::{Quad, Term};
use std::collections::{HashMap, HashSet};

use crate::config::PAN_NS;
use crate::PendingItem;

/// The reference predicates a stage writes, one per stage.
pub const REF_LOCALS: [&str; 5] = [
    "regionData",
    "poseData",
    "captionData",
    "vectorData",
    crate::depth::REF_LOCAL,
];

/// One image as the ladder sees it.
#[derive(Clone, Debug)]
pub struct ImageRow {
    pub created_date: String,
    pub iri: String,
    pub id: String,
    pub media_path: String,
    pub media_type: String,
}

#[derive(Default, Debug)]
pub struct WorkIndex {
    /// Every image, newest first: `created_date` descending, then IRI
    /// ascending, the order the graph query used (`ORDER BY DESC(?d) ?s`).
    images: Vec<ImageRow>,
    known: HashSet<String>,
    /// (reference predicate, model) → the IRIs of images that have a record.
    done: HashMap<(String, String), HashSet<String>>,
    /// Images carrying `pan:enrichmentCompleteDate`.
    complete: HashSet<String>,
    /// What the gated stages wait for: segmentation needs scene objects,
    /// the embedding needs the long caption (goodlux, 2026-09-08).
    has_scene_objects: HashSet<String>,
    has_long_caption: HashSet<String>,
}

fn key(created_date: &str, iri: &str) -> (std::cmp::Reverse<String>, String) {
    (std::cmp::Reverse(created_date.to_string()), iri.to_string())
}

impl WorkIndex {
    pub fn images(&self) -> usize {
        self.images.len()
    }

    /// Add an image row, keeping the order. A known IRI is left as it is.
    pub fn add_image(&mut self, row: ImageRow) {
        if !self.known.insert(row.iri.clone()) {
            return;
        }
        let k = key(&row.created_date, &row.iri);
        let at = self
            .images
            .binary_search_by(|r| key(&r.created_date, &r.iri).cmp(&k))
            .unwrap_or_else(|i| i);
        self.images.insert(at, row);
    }

    pub fn mark_done(&mut self, ref_local: &str, model: &str, iri: &str) {
        self.done
            .entry((ref_local.to_string(), model.to_string()))
            .or_default()
            .insert(iri.to_string());
    }

    pub fn mark_complete(&mut self, iri: &str) {
        self.complete.insert(iri.to_string());
    }

    pub fn set_needs(&mut self, iri: &str, scene_objects: bool, long_caption: bool) {
        for (set, on) in [
            (&mut self.has_scene_objects, scene_objects),
            (&mut self.has_long_caption, long_caption),
        ] {
            if on {
                set.insert(iri.to_string());
            } else {
                set.remove(iri);
            }
        }
    }

    pub fn remove_image(&mut self, iri: &str) {
        if !self.known.remove(iri) {
            return;
        }
        self.images.retain(|r| r.iri != iri);
        for s in self.done.values_mut() {
            s.remove(iri);
        }
        self.complete.remove(iri);
        self.has_scene_objects.remove(iri);
        self.has_long_caption.remove(iri);
    }

    /// Learn from a batch of quads about to be, or just, inserted: a new
    /// image (its type, path, type and date arrive together at ingest), a
    /// stage's reference and the model on it, a ready mark, a caption's
    /// gate fields.
    pub fn apply_insert(&mut self, quads: &[Quad]) {
        let p = |q: &Quad| {
            q.predicate
                .as_str()
                .strip_prefix(PAN_NS)
                .map(str::to_string)
        };
        let subj = |q: &Quad| q.subject.to_string().trim_matches(['<', '>']).to_string();
        let lit = |q: &Quad| match &q.object {
            Term::Literal(l) => Some(l.value().to_string()),
            _ => None,
        };
        let iri_obj = |q: &Quad| match &q.object {
            Term::NamedNode(n) => Some(n.as_str().to_string()),
            _ => None,
        };
        // Pass one: what each subject says about itself.
        let mut is_image: HashSet<String> = HashSet::new();
        let mut fields: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut refs: Vec<(String, String, String)> = Vec::new();
        let mut models: HashMap<String, String> = HashMap::new();
        for q in quads {
            let s = subj(q);
            if q.predicate.as_str() == crate::RDF_TYPE
                && iri_obj(q).as_deref() == Some(&format!("{PAN_NS}Image"))
            {
                is_image.insert(s.clone());
                continue;
            }
            let Some(local) = p(q) else { continue };
            match local.as_str() {
                "mediaPath" | "mediaType" | "createdDate" => {
                    if let Some(v) = lit(q) {
                        fields.entry(s).or_default().insert(local, v);
                    }
                }
                "model" => {
                    if let Some(v) = lit(q) {
                        models.insert(s, v);
                    }
                }
                "enrichmentCompleteDate" => self.mark_complete(&s),
                "sceneObjects" => {
                    self.has_scene_objects.insert(s);
                }
                "longCaption" => {
                    self.has_long_caption.insert(s);
                }
                l if REF_LOCALS.contains(&l) => {
                    if let Some(node) = iri_obj(q) {
                        refs.push((s, local, node));
                    }
                }
                _ => {}
            }
        }
        for s in is_image {
            let Some(f) = fields.get(&s) else { continue };
            if let (Some(path), Some(ty), Some(d)) =
                (f.get("mediaPath"), f.get("mediaType"), f.get("createdDate"))
            {
                self.add_image(ImageRow {
                    created_date: d.clone(),
                    id: crate::bare_id(&s),
                    iri: s.clone(),
                    media_path: path.clone(),
                    media_type: ty.clone(),
                });
            }
        }
        for (s, local, node) in refs {
            match models.get(&node) {
                Some(m) => self.mark_done(&local, m, &s),
                None => tracing::debug!(
                    image = %s,
                    reference = %local,
                    "a stage reference arrived without its model in the same batch; the next build catches it"
                ),
            }
        }
    }

    /// The images a stage still has to do: newest first, at or after the
    /// floor, without a record from this model, past the stage's gate.
    pub fn pending(
        &self,
        ref_local: &str,
        model: &str,
        limit: usize,
        since: Option<&str>,
    ) -> Vec<PendingItem> {
        let done = self.done.get(&(ref_local.to_string(), model.to_string()));
        let mut out = Vec::new();
        for r in &self.images {
            if out.len() >= limit {
                break;
            }
            if let Some(floor) = since {
                if r.created_date.as_str() < floor {
                    // Newest first: everything after this is older still.
                    break;
                }
            }
            if done.is_some_and(|d| d.contains(&r.iri)) {
                continue;
            }
            let gated = match ref_local {
                "regionData" => !self.has_scene_objects.contains(&r.iri),
                "vectorData" => !self.has_long_caption.contains(&r.iri),
                _ => false,
            };
            if gated {
                continue;
            }
            out.push(PendingItem {
                id: r.id.clone(),
                iri: r.iri.clone(),
                media_path: r.media_path.clone(),
                media_type: r.media_type.clone(),
            });
        }
        out
    }

    /// Images with a record for every required (reference, model) pair and
    /// no ready mark yet.
    pub fn complete_candidates(&self, required: &[(String, String)], limit: usize) -> Vec<String> {
        let sets: Vec<Option<&HashSet<String>>> = required
            .iter()
            .map(|(l, m)| self.done.get(&(l.clone(), m.clone())))
            .collect();
        let mut out = Vec::new();
        for r in &self.images {
            if out.len() >= limit {
                break;
            }
            if self.complete.contains(&r.iri) {
                continue;
            }
            if sets.iter().all(|s| s.is_some_and(|s| s.contains(&r.iri))) {
                out.push(r.id.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(d: &str, id: &str) -> ImageRow {
        ImageRow {
            created_date: d.into(),
            iri: format!("https://repolex.ai/pan/Image/{id}"),
            id: id.into(),
            media_path: format!("image/img/source/{id}.png"),
            media_type: "image/png".into(),
        }
    }

    #[test]
    fn newest_first_then_iri_and_the_floor_stops_the_walk() {
        let mut w = WorkIndex::default();
        w.add_image(row("2026-09-01T00:00:00-07:00", "aaa"));
        w.add_image(row("2026-09-03T00:00:00-07:00", "ccc"));
        w.add_image(row("2026-09-02T00:00:00-07:00", "bbb"));
        w.add_image(row("2026-09-02T00:00:00-07:00", "abb"));
        w.add_image(row("2026-09-03T00:00:00-07:00", "ccc")); // twice: once
        let ids: Vec<String> = w
            .pending("poseData", "m", 10, None)
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["ccc", "abb", "bbb", "aaa"]);
        let ids: Vec<String> = w
            .pending("poseData", "m", 10, Some("2026-09-02T00:00:00-07:00"))
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["ccc", "abb", "bbb"], "the floor is inclusive");
        w.mark_done("poseData", "m", "https://repolex.ai/pan/Image/ccc");
        let ids: Vec<String> = w
            .pending("poseData", "m", 2, None)
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["abb", "bbb"], "done is skipped, limit holds");
        assert_eq!(w.pending("poseData", "other-model", 1, None)[0].id, "ccc");
    }

    #[test]
    fn gates_ready_marks_and_removal() {
        let mut w = WorkIndex::default();
        w.add_image(row("2026-09-01T00:00:00-07:00", "aaa"));
        assert!(
            w.pending("regionData", "sam3", 5, None).is_empty(),
            "no scene objects yet"
        );
        assert!(
            w.pending("vectorData", "e", 5, None).is_empty(),
            "no long caption yet"
        );
        w.set_needs("https://repolex.ai/pan/Image/aaa", true, false);
        assert_eq!(w.pending("regionData", "sam3", 5, None).len(), 1);
        assert!(w.pending("vectorData", "e", 5, None).is_empty());
        let req = vec![("poseData".to_string(), "m".to_string())];
        assert!(w.complete_candidates(&req, 5).is_empty());
        w.mark_done("poseData", "m", "https://repolex.ai/pan/Image/aaa");
        assert_eq!(w.complete_candidates(&req, 5), ["aaa"]);
        assert_eq!(
            w.complete_candidates(&[], 5),
            ["aaa"],
            "no stages: ingest is ready"
        );
        w.mark_complete("https://repolex.ai/pan/Image/aaa");
        assert!(w.complete_candidates(&req, 5).is_empty());
        w.remove_image("https://repolex.ai/pan/Image/aaa");
        assert_eq!(w.images(), 0);
        assert!(w.pending("regionData", "sam3", 5, None).is_empty());
    }
}
