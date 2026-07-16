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
            Facts::new().with("dc:subject", "wolf").with("dc:title", "wolf in forest"),
        )
        .unwrap();
    let sea = store
        .put(
            &sea_png,
            Some("image/png"),
            Facts::new().with("dc:subject", "sea"),
        )
        .unwrap();
    assert!(wolf.created && sea.created);
    assert_ne!(wolf.cid, sea.cid);
    assert!(wolf.cid.starts_with("sha256:"), "bare cid form on the wire");

    // Idempotent re-put: same cid, created=false, createdAt preserved.
    let wolf2 = store.put(&wolf_png, Some("image/png"), Facts::new()).unwrap();
    assert_eq!(wolf2.cid, wolf.cid);
    assert!(!wolf2.created);
    assert_eq!(wolf2.created_at, wolf.created_at);

    // ── get: bytes come back, pixel identity stable across the stamp ──
    let (bytes, facts) = store.get(&wolf.cid).unwrap();
    assert_ne!(bytes, wolf_png, "stored PNG is stamped (file bytes differ)");
    assert_eq!(
        pan::xmp::compute_pixel_cid(&bytes).unwrap(),
        wolf.cid,
        "pixel cid survives the stamp — identity never rotates"
    );
    let facts_map: HashMap<String, Vec<String>> = facts.into_iter().collect();
    assert_eq!(facts_map["http://purl.org/dc/elements/1.1/subject"], vec!["wolf"]);

    // get accepts the urn: form too (Pool's double-urn lesson).
    let (bytes_urn, _) = store.get(&format!("urn:{}", wolf.cid)).unwrap();
    assert_eq!(bytes, bytes_urn);

    // ── the stamped XMP mirror carries the app facts (travel copy) ──
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes).unwrap().expect("stamped");
    assert!(packet.contains("wolf in forest"), "app facts mirrored into XMP");
    assert!(packet.contains(&wolf.cid), "pan: identity block present");

    // ── describe: merge facts, loud failure on unknown prefix ──
    store
        .describe(&wolf.cid, Facts::new().with("dc:creator", "w4r3z"))
        .unwrap();
    let err = store
        .describe(&wolf.cid, Facts::new().with("nope:field", "x"))
        .unwrap_err();
    assert!(err.to_string().contains("unknown prefix"), "loud, not silent: {err}");

    // Re-stamp followed the graph: the new fact is in the file's XMP now.
    let (bytes_after, _) = store.get(&wolf.cid).unwrap();
    let packet_after = pan::xmp::read_xmp_packet_from_bytes(&bytes_after).unwrap().unwrap();
    assert!(packet_after.contains("w4r3z"), "describe re-stamps the travel copy");
    assert_eq!(
        pan::xmp::compute_pixel_cid(&bytes_after).unwrap(),
        wolf.cid,
        "restamp preserves identity"
    );

    // ── graph-only query mode (no vectors anywhere yet) ──
    {
        let results = store
            .query("SELECT ?cid WHERE { ?s dc:subject \"wolf\" ; pan:cid ?cid }")
            .unwrap();
        let cids: Vec<String> = match results {
            pan::QueryResults::Solutions(sols) => sols
                .map(|s| {
                    let s = s.unwrap();
                    match s.get("cid").unwrap() {
                        pan::Term::Literal(l) => l.value().to_string(),
                        other => panic!("expected literal, got {other}"),
                    }
                })
                .collect(),
            _ => panic!("expected solutions"),
        };
        assert_eq!(cids, vec![wolf.cid.clone()], "graph-only mode is a complete product");
    }

    // ── attach vectors (the two-call flow) + fusion search ──
    let dim = 64;
    let wolf_vec = unit_vec(dim, 3);
    let sea_vec = unit_vec(dim, 40);
    assert!(store.add_vector(&wolf.cid, "test-idx", &wolf_vec).unwrap());
    assert!(store.add_vector(&sea.cid, "test-idx", &sea_vec).unwrap());
    assert!(
        !store.add_vector(&wolf.cid, "test-idx", &wolf_vec).unwrap(),
        "idempotent re-add is a no-op"
    );

    // Raw sidecars landed (reembed source of truth).
    let sidecar = store.layout.vector_sidecar_path("test-idx", &wolf.cid);
    assert!(sidecar.exists(), "npy sidecar written at {}", sidecar.display());
    assert_eq!(pan::npy::read_f32_1d(&sidecar).unwrap().len(), dim);

    // Ungated search: nearest to wolf_vec is wolf.
    let hits = store.search("", &wolf_vec, 2, "test-idx").unwrap();
    assert_eq!(hits[0].cid, wolf.cid);
    assert!(hits[0].score > 0.99, "self-similarity ~1.0, got {}", hits[0].score);
    assert_eq!(hits.len(), 2);

    // THE crown jewel: graph pattern gates the candidate set, kNN ranks.
    // Query vector is wolf-like, but the gate only admits dc:subject "sea" —
    // so the sea image is the only possible hit.
    let hits = store
        .search("?s dc:subject \"sea\" .", &wolf_vec, 5, "test-idx")
        .unwrap();
    assert_eq!(hits.len(), 1, "graph gate admits exactly the sea image");
    assert_eq!(hits[0].cid, sea.cid);

    // Dim mismatch is loud.
    let err = store.search("", &unit_vec(32, 1), 5, "test-idx").unwrap_err();
    assert!(err.to_string().contains("does not match index"), "{err}");

    // ── persistence: reopen from disk, search still works (lazy index load) ──
    store.flush().unwrap();
    drop(store);
    let reopened = Pan::open(dir.path()).unwrap();
    let hits = reopened.search("", &sea_vec, 1, "test-idx").unwrap();
    assert_eq!(hits[0].cid, sea.cid, "index + keymap reload from disk");

    // Graph survived too.
    let (_, facts) = reopened.get(&wolf.cid).unwrap();
    let facts_map: HashMap<String, Vec<String>> = facts.into_iter().collect();
    assert_eq!(facts_map["http://purl.org/dc/elements/1.1/creator"], vec!["w4r3z"]);

    // ── delete: everything about the sea image goes ──
    reopened.delete(&sea.cid).unwrap();
    assert!(reopened.facts_for(&sea.cid).unwrap().is_empty(), "triples gone");
    assert!(reopened.get(&sea.cid).is_err(), "blob gone");
    let hits = reopened.search("", &sea_vec, 5, "test-idx").unwrap();
    assert!(
        hits.iter().all(|h| h.cid != sea.cid),
        "deleted cid never surfaces in search"
    );
    // Wolf unaffected.
    assert!(!reopened.facts_for(&wolf.cid).unwrap().is_empty());
}

#[test]
fn travel_copy_ingests_on_put_into_fresh_store() {
    // The portability property: a PNG stamped by one store carries its facts
    // into a brand-new store via XMP (the walker-lite ingest on put).
    let dir_a = tempfile::tempdir().unwrap();
    std::fs::write(
        dir_a.path().join("pan.yml"),
        "prefixes:\n  dc: http://purl.org/dc/elements/1.1/\n",
    )
    .unwrap();
    let store_a = Pan::open(dir_a.path()).unwrap();
    let png = make_png(10, 10, 42);
    let put_a = store_a
        .put(&png, Some("image/png"), Facts::new().with("dc:subject", "lighthouse"))
        .unwrap();
    let (stamped, _) = store_a.get(&put_a.cid).unwrap();

    // New store, no shared config beyond defaults — the fact rides the file.
    let dir_b = tempfile::tempdir().unwrap();
    let store_b = Pan::open(dir_b.path()).unwrap();
    let put_b = store_b.put(&stamped, Some("image/png"), Facts::new()).unwrap();
    assert_eq!(put_b.cid, put_a.cid, "pixel cid is the cross-store identity");

    let facts: HashMap<String, Vec<String>> =
        store_b.facts_for(&put_b.cid).unwrap().into_iter().collect();
    assert_eq!(
        facts["http://purl.org/dc/elements/1.1/subject"],
        vec!["lighthouse"],
        "facts traveled inside the file and ingested on put"
    );
}
