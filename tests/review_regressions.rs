//! Regression tests for the defects the adversarial review fleet confirmed.
//! Each names the finding it pins so a future change that reopens the hole
//! fails loudly here. (Updated for the Day-50 identity model: assigned panId,
//! standard https subject IRIs, no content-addressing.)

use pan::{Facts, Pan};
use std::collections::HashMap;

fn make_png(seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 10, 10);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        let px: Vec<u8> = (0..300).map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed)).collect();
        w.write_image_data(&px).unwrap();
        w.finish().unwrap();
    }
    out
}

fn facts_map(store: &Pan, pan_id: &str) -> HashMap<String, Vec<String>> {
    store.facts_for(pan_id).unwrap().into_iter().collect()
}

/// #1/#18 (revised for assigned identity) — two puts of the same bytes are two
/// INDEPENDENT objects: distinct panIds, distinct blobPaths, and deleting one
/// never touches the other's blob or facts.
#[test]
fn same_bytes_twice_are_independent_objects() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let png = make_png(1);

    let a = store.put(&png, Some("image/png"), Facts::new()).unwrap();
    let b = store.put(&png, Some("image/png"), Facts::new()).unwrap();
    assert_ne!(a.id, b.id, "assigned ids never collide on same bytes");
    assert_ne!(a.media_path, b.media_path, "each object owns its own media file");

    let fa = facts_map(&store, &a.id);
    assert_eq!(fa["https://repolex.ai/ontology/pan/mediaPath"].len(), 1, "exactly one mediaPath");
    assert_eq!(fa["https://repolex.ai/ontology/pan/mediaType"].len(), 1, "exactly one mediaType");

    // Deleting one object leaves the other fully intact.
    store.delete(&a.id).unwrap();
    assert!(store.facts_for(&a.id).unwrap().is_empty());
    assert!(store.get(&b.id).is_ok(), "sibling object untouched by delete");
    // And no media file is orphaned for the deleted one.
    let leftover = walk_files(&store.layout.media_root);
    assert_eq!(leftover.len(), 1, "exactly the sibling's media file remains: {leftover:?}");
}

/// #2/#6 — one malformed /search must not poison a valid index's dim for the
/// process. After a wrong-length query errors, a correct-length query works.
#[test]
fn wrong_dim_query_does_not_poison_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let png = make_png(2);
    let put = store.put(&png, Some("image/png"), Facts::new()).unwrap();

    let good: Vec<f32> = {
        let mut v = vec![0.1f32; 8];
        v[0] = 1.0;
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    };
    store.add_vector(&put.id, "idx", &good).unwrap();
    store.flush().unwrap();
    drop(store);

    // Reopen so the index must lazy-load from disk.
    let store = Pan::open(dir.path()).unwrap();
    // A wrong-length query first — errors, but must NOT overwrite the real dim.
    let bad = vec![1.0f32, 0.0, 0.0];
    assert!(store.search("", &bad, 3, "idx").is_err(), "dim-3 query should error against dim-8 index");
    // Now a CORRECT-length query must still work.
    let hits = store.search("", &good, 3, "idx").unwrap();
    assert_eq!(hits.len(), 1, "valid dim-8 query rejected — index dim was poisoned");
    assert_eq!(hits[0].id, put.id);
}

/// #3/#4/#10 — a vector index name with path-traversal must be rejected, never
/// reach Path::join and write outside the store. Same for a traversal panId.
#[test]
fn traversal_index_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let png = make_png(3);
    let put = store.put(&png, Some("image/png"), Facts::new()).unwrap();
    let v = vec![1.0f32; 4];

    for evil in ["../escape", "a/b", "..", "/abs/path", "with\0nul", "dir/../x"] {
        assert!(
            store.add_vector(&put.id, evil, &v).is_err(),
            "add_vector accepted a traversal index name: {evil:?}"
        );
        assert!(
            store.search("", &v, 1, evil).is_err(),
            "search accepted a traversal index name: {evil:?}"
        );
        // A panId is also a path component (sidecar filename) — same gate.
        assert!(
            store.add_vector(evil, "ok-index", &v).is_err(),
            "add_vector accepted a traversal panId: {evil:?}"
        );
    }
    // Nothing was created outside the hnsw root by the rejected calls.
    let escaped = dir.path().parent().unwrap().join("escape");
    assert!(!escaped.exists(), "a traversal write escaped the store");

    // A legitimate name still works.
    assert!(store.add_vector(&put.id, "qwen-vl-2b-2048", &v).is_ok());
}

/// #11/#14 — a standard Adobe-style PNG (root rdf:about="") must ingest, not
/// fail the store. And an outright-garbage XMP must be skipped, not fatal.
#[test]
fn standard_adobe_xmp_ingests_and_garbage_is_skipped() {
    // Build a PNG carrying a hand-rolled standard-Adobe packet.
    let png = make_png(4);
    let packet = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n\
         <dc:title>Adobe Title</dc:title>\n\
         </rdf:Description>\n\
         </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>"
    );
    let adobe_png = pan::xmp::write_packet_into_png_bytes(&png, &packet).unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pan.yml"), "prefixes:\n  dc: http://purl.org/dc/elements/1.1/\n").unwrap();
    let store = Pan::open(dir.path()).unwrap();

    // Must NOT error — the whole point is real-world files ingest.
    let put = store.put(&adobe_png, Some("image/png"), Facts::new()).unwrap();
    let f = facts_map(&store, &put.id);
    assert_eq!(
        f.get("http://purl.org/dc/elements/1.1/title").map(|v| v.as_slice()),
        Some(["Adobe Title".to_string()].as_slice()),
        "standard-Adobe dc:title must ingest"
    );

    // A PNG with a corrupt XMP chunk must store fine, just without facts.
    let garbage = pan::xmp::write_packet_into_png_bytes(&make_png(5), "<not xml at all <<<").unwrap();
    let put2 = store.put(&garbage, Some("image/png"), Facts::new()).unwrap();
    assert!(store.get(&put2.id).is_ok(), "garbage XMP must not fail the store");
}

/// #12/#15 — rdf:type from travel XMP survives ingest as an IRI, so a type
/// query (`?s a copia:Sam3Region`) matches. Under assigned identity the region
/// sub-subject is REBASED onto the receiving store's subject — the type query
/// runs against the rebased IRI.
#[test]
fn travel_rdf_type_survives_as_iri_for_type_queries() {
    const COPIA: &str = "https://repolex.ai/ontology/kit/copia/";
    let dir_a = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("pan.yml"), format!("prefixes:\n  copia: {COPIA}\n")).unwrap();
    let store_a = Pan::open(dir_a.path()).unwrap();
    let png = make_png(6);
    let put = store_a.put(&png, Some("image/png"), Facts::new()).unwrap();

    // Author a region sub-subject with an rdf:type onto the graph, re-stamp.
    let region = format!("{}/Region/wolf/01", put.iri);
    store_a
        .describe_subject(&region, "http://www.w3.org/1999/02/22-rdf-syntax-ns#type", &format!("{COPIA}Sam3Region"), true)
        .unwrap();
    store_a
        .describe_subject(&region, &format!("{COPIA}regionDescriptor"), "wolf", false)
        .unwrap();
    store_a.restamp(&put.id).unwrap();
    let (stamped, _) = store_a.get(&put.id).unwrap();

    // Travel to a fresh store.
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_b.path().join("pan.yml"), format!("prefixes:\n  copia: {COPIA}\n")).unwrap();
    let store_b = Pan::open(dir_b.path()).unwrap();
    let put_b = store_b.put(&stamped, Some("image/png"), Facts::new()).unwrap();

    // The region rebased onto store_b's subject; the type query must match
    // there — proves rdf:type ingested as an IRI AND the rebase landed.
    let region_b = format!("{}/Region/wolf/01", put_b.iri);
    let results = store_b
        .query(&format!("ASK {{ <{region_b}> a <{COPIA}Sam3Region> }}"))
        .unwrap();
    match results {
        pan::QueryResults::Boolean(b) => assert!(b, "rdf:type degraded to a string or rebase failed; type query fails"),
        _ => panic!("expected ASK boolean"),
    }
}

/// #16 — a sub-subject with facts in MULTIPLE namespaces exports all of them
/// into the travel copy; none silently dropped at the next store.
#[test]
fn multi_namespace_sub_subject_survives_travel() {
    const COPIA: &str = "https://repolex.ai/ontology/kit/copia/";
    const DC: &str = "http://purl.org/dc/elements/1.1/";
    let dir_a = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("pan.yml"), format!("prefixes:\n  copia: {COPIA}\n  dc: {DC}\n")).unwrap();
    let store_a = Pan::open(dir_a.path()).unwrap();
    let png = make_png(7);
    let put = store_a.put(&png, Some("image/png"), Facts::new()).unwrap();
    let region = format!("{}/Region/sea/01", put.iri);
    store_a.describe_subject(&region, &format!("{COPIA}regionDescriptor"), "sea", false).unwrap();
    store_a.describe_subject(&region, &format!("{DC}creator"), "w4r3z", false).unwrap();
    store_a.restamp(&put.id).unwrap();
    let (stamped, _) = store_a.get(&put.id).unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_b.path().join("pan.yml"), format!("prefixes:\n  copia: {COPIA}\n  dc: {DC}\n")).unwrap();
    let store_b = Pan::open(dir_b.path()).unwrap();
    let put_b = store_b.put(&stamped, Some("image/png"), Facts::new()).unwrap();

    // Region facts live at the REBASED IRI in store_b; read them via SPARQL
    // (facts_for is panId-scoped; sub-subjects are plain graph nodes).
    let region_b = format!("{}/Region/sea/01", put_b.iri);
    let ask = |pred: &str, val: &str| -> bool {
        match store_b
            .query(&format!("ASK {{ <{region_b}> <{pred}> \"{val}\" }}"))
            .unwrap()
        {
            pan::QueryResults::Boolean(b) => b,
            _ => panic!("expected ASK boolean"),
        }
    };
    assert!(ask(&format!("{COPIA}regionDescriptor"), "sea"));
    assert!(
        ask(&format!("{DC}creator"), "w4r3z"),
        "second-namespace fact dropped in the travel copy"
    );
}

fn walk_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk_files(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}
