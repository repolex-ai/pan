//! End-to-end round-trip over the Pan core: the acceptance test for Task 1's
//! charter — store media, describe it with a graph, search it by graph pattern
//! AND vector similarity. Runs with ZERO detectors configured (graph-only is a
//! complete product) and exercises graph+vector via the two-call flow.

use pan::{Facts, Pan};
use std::collections::HashMap;

/// Tiny deterministic RGB PNG.
fn make_png(w: u32, h: u32, seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        let px: Vec<u8> = (0..w * h * 3)
            .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed))
            .collect();
        writer.write_image_data(&px).unwrap();
        writer.finish().unwrap();
    }
    out
}

/// A crude unit vector pointing mostly along one axis — distinguishable under
/// cosine similarity.
fn unit_vec(dim: usize, axis: usize) -> Vec<f32> {
    let mut v = vec![0.01f32; dim];
    v[axis] = 1.0;
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / norm).collect()
}

#[test]
fn full_store_describe_query_search_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pan.yml"),
        "storage_id: test-store\nindex_id: test-idx\nprefixes:\n  dc: http://purl.org/dc/elements/1.1/\n",
    )
    .unwrap();
    let store = Pan::open(dir.path()).unwrap();
    assert_eq!(store.cfg.storage_id, "test-store");

    // ── put two images with facts ──
    let wolf_png = make_png(12, 12, 1);
    let sea_png = make_png(12, 12, 99);
    let wolf = store
        .put(
            &wolf_png,
            Some("image/png"),
            None,
            Facts::new().with("dc:subject", "wolf").with("dc:title", "wolf in forest"),
        )
        .unwrap();
    let sea = store
        .put(
            &sea_png,
            Some("image/png"),
            None,
            Facts::new().with("dc:subject", "sea"),
        )
        .unwrap();
    assert_ne!(wolf.id, sea.id);
    assert_eq!(wolf.id.len(), 8, "panId is a short assigned id");
    assert_eq!(
        wolf.iri,
        format!("https://repolex.ai/pan/Image/{}", wolf.id),
        "subject is a standard full https IRI"
    );

    // The identity model: putting the SAME bytes again is a NEW media object —
    // panIds are assigned, never content-derived, and there is no dedup.
    let wolf2 = store.put(&wolf_png, Some("image/png"), None, Facts::new()).unwrap();
    assert_ne!(wolf2.id, wolf.id, "same bytes, different object, different panId");
    store.delete(&wolf2.id).unwrap();

    // ── get: bytes come back, pixels stable across the stamp ──
    let (bytes, facts) = store.get(&wolf.id).unwrap();
    assert_ne!(bytes, wolf_png, "stored PNG is stamped (file bytes differ)");
    assert_eq!(
        pan::xmp::pixel_hash(&bytes).unwrap(),
        pan::xmp::pixel_hash(&wolf_png).unwrap(),
        "the stamp never touches the pixels"
    );
    let facts_map: HashMap<String, Vec<String>> = facts.into_iter().collect();
    assert_eq!(facts_map["http://purl.org/dc/elements/1.1/subject"], vec!["wolf"]);
    assert_eq!(
        facts_map["https://repolex.ai/ontology/git-lex/id"],
        vec![wolf.iri.clone()],
        "git-lex:id identity fact present (the IRI itself)"
    );
    assert_eq!(
        facts_map["http://www.w3.org/1999/02/22-rdf-syntax-ns#type"],
        vec!["https://repolex.ai/ontology/pan/Image".to_string()],
        "instance is typed against the kit ontology class"
    );

    // ── Pan's block is in the image XMP ──
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes).unwrap().expect("XMP written");
    assert!(packet.contains(&wolf.iri), "pan: identity block present");
    assert!(packet.contains("pan:createdDate"), "createdDate in the packet");

    // ── describe: merge facts, loud failure on unknown prefix ──
    store
        .describe(&wolf.id, Facts::new().with("dc:creator", "w4r3z"))
        .unwrap();
    let err = store
        .describe(&wolf.id, Facts::new().with("nope:field", "x"))
        .unwrap_err();
    assert!(err.to_string().contains("unknown prefix"), "loud, not silent: {err}");

    // Re-stamp rewrote the XMP without touching pixels.
    let (bytes_after, _) = store.get(&wolf.id).unwrap();
    let _packet_after = pan::xmp::read_xmp_packet_from_bytes(&bytes_after).unwrap().unwrap();
    assert_eq!(
        pan::xmp::pixel_hash(&bytes_after).unwrap(),
        pan::xmp::pixel_hash(&wolf_png).unwrap(),
        "restamp preserves the pixels"
    );

    // ── graph-only query mode (no vectors anywhere yet) ──
    {
        let results = store
            .query("SELECT ?id WHERE { ?s dc:subject \"wolf\" ; git-lex:id ?id }")
            .unwrap();
        let ids: Vec<String> = match results {
            pan::QueryResults::Solutions(sols) => sols
                .map(|s| {
                    let s = s.unwrap();
                    match s.get("id").unwrap() {
                        pan::Term::NamedNode(n) => n.as_str().to_string(),
                        other => panic!("expected IRI, got {other}"),
                    }
                })
                .collect(),
            _ => panic!("expected solutions"),
        };
        assert_eq!(ids, vec![wolf.iri.clone()], "graph-only mode is a complete product");
    }

    // ── attach vectors (the two-call flow) + fusion search ──
    let dim = 64;
    let wolf_vec = unit_vec(dim, 3);
    let sea_vec = unit_vec(dim, 40);
    assert!(store.add_vector(&wolf.id, "test-idx", &wolf_vec).unwrap());
    assert!(store.add_vector(&sea.id, "test-idx", &sea_vec).unwrap());
    assert!(
        !store.add_vector(&wolf.id, "test-idx", &wolf_vec).unwrap(),
        "idempotent re-add is a no-op"
    );

    // Raw sidecars landed (reembed source of truth).
    let sidecar = store.layout.vector_sidecar_path("test-idx", &wolf.id);
    assert!(sidecar.exists(), "npy sidecar written at {}", sidecar.display());
    assert_eq!(pan::npy::read_f32_1d(&sidecar).unwrap().len(), dim);

    // Ungated search: nearest to wolf_vec is wolf.
    let hits = store.search("", &wolf_vec, 2, "test-idx").unwrap();
    assert_eq!(hits[0].id, wolf.id);
    assert!(hits[0].score > 0.99, "self-similarity ~1.0, got {}", hits[0].score);
    assert_eq!(hits.len(), 2);

    // THE crown jewel: graph pattern gates the candidate set, kNN ranks.
    // Query vector is wolf-like, but the gate only admits dc:subject "sea" —
    // so the sea image is the only possible hit.
    let hits = store
        .search("?s dc:subject \"sea\" .", &wolf_vec, 5, "test-idx")
        .unwrap();
    assert_eq!(hits.len(), 1, "graph gate admits exactly the sea image");
    assert_eq!(hits[0].id, sea.id);

    // Dim mismatch is loud.
    let err = store.search("", &unit_vec(32, 1), 5, "test-idx").unwrap_err();
    assert!(err.to_string().contains("does not match index"), "{err}");

    // ── persistence: reopen from disk, search still works (lazy index load) ──
    store.flush().unwrap();
    drop(store);
    let reopened = Pan::open(dir.path()).unwrap();
    let hits = reopened.search("", &sea_vec, 1, "test-idx").unwrap();
    assert_eq!(hits[0].id, sea.id, "index + keymap reload from disk");

    // Graph survived too.
    let (_, facts) = reopened.get(&wolf.id).unwrap();
    let facts_map: HashMap<String, Vec<String>> = facts.into_iter().collect();
    assert_eq!(facts_map["http://purl.org/dc/elements/1.1/creator"], vec!["w4r3z"]);

    // ── delete: everything about the sea image goes ──
    reopened.delete(&sea.id).unwrap();
    assert!(reopened.facts_for(&sea.id).unwrap().is_empty(), "triples gone");
    assert!(reopened.get(&sea.id).is_err(), "blob gone");
    let hits = reopened.search("", &sea_vec, 5, "test-idx").unwrap();
    assert!(
        hits.iter().all(|h| h.id != sea.id),
        "deleted panId never surfaces in search"
    );
    // Wolf unaffected.
    assert!(!reopened.facts_for(&wolf.id).unwrap().is_empty());
}

#[test]
fn travel_copy_ingests_on_put_into_fresh_store() {
    // The portability property: a PNG stamped by one store carries its FACTS
    // into a brand-new store via XMP (the walker-lite ingest on put). Facts
    // travel; IDENTITY does not — the receiving store assigns its own panId.
    let dir_a = tempfile::tempdir().unwrap();
    std::fs::write(
        dir_a.path().join("pan.yml"),
        "prefixes:\n  dc: http://purl.org/dc/elements/1.1/\n",
    )
    .unwrap();
    let store_a = Pan::open(dir_a.path()).unwrap();
    let png = make_png(10, 10, 42);
    let block = "<rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:subject>lighthouse</dc:subject></rdf:Description>";
    let put_a = store_a
        .put(&png, Some("image/png"), Some(block), Facts::new())
        .unwrap();
    let (stamped, _) = store_a.get(&put_a.id).unwrap();

    // New store, no shared config beyond defaults — the fact rides the file.
    let dir_b = tempfile::tempdir().unwrap();
    let store_b = Pan::open(dir_b.path()).unwrap();
    let put_b = store_b.put(&stamped, Some("image/png"), None, Facts::new()).unwrap();
    assert_ne!(
        put_b.id, put_a.id,
        "identity never travels — the receiving store assigns its own panId"
    );

    let facts: HashMap<String, Vec<String>> =
        store_b.facts_for(&put_b.id).unwrap().into_iter().collect();
    assert_eq!(
        facts["http://purl.org/dc/elements/1.1/subject"],
        vec!["lighthouse"],
        "facts traveled inside the file and ingested on put"
    );
    // The receiving store's identity is its OWN, not the source's.
    assert_eq!(facts["https://repolex.ai/ontology/git-lex/id"], vec![put_b.iri.clone()]);
}
