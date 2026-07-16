//! Regression tests for the defects the adversarial review fleet confirmed.
//! Each names the finding it pins so a future change that reopens the hole
//! fails loudly here.

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

fn facts_map(store: &Pan, cid: &str) -> HashMap<String, Vec<String>> {
    store.facts_for(cid).unwrap().into_iter().collect()
}

/// #1/#18 — re-put with a different content_type must NOT fork a second
/// blobPath/mediaType or orphan a blob on delete.
#[test]
fn reput_different_content_type_does_not_fork_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let png = make_png(1);

    let a = store.put(&png, Some("image/png"), Facts::new()).unwrap();
    // Same PIXELS, different (wrong) Content-Type on the re-put.
    let b = store.put(&png, Some("image/jpeg"), Facts::new()).unwrap();
    assert_eq!(a.cid, b.cid);
    assert_eq!(a.blob_path, b.blob_path, "blobPath must stay stable across re-put");

    let f = facts_map(&store, &a.cid);
    assert_eq!(f["https://repolex.ai/ontology/pan/blobPath"].len(), 1, "exactly one blobPath");
    assert_eq!(f["https://repolex.ai/ontology/pan/mediaType"].len(), 1, "exactly one mediaType");
    assert_eq!(f["https://repolex.ai/ontology/pan/mediaType"][0], "image/png", "original mediaType wins");

    // Delete removes the one and only blob — nothing orphaned.
    store.delete(&a.cid).unwrap();
    let blob = store.layout.blob_root.join(format!("{}.png", a.cid.rsplit(':').next().unwrap()));
    // (path is date-sharded; just assert the store has no leftover blob files)
    let leftover = walk_files(&store.layout.blob_root);
    assert!(leftover.is_empty(), "delete orphaned files: {leftover:?}");
    let _ = blob;
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
    store.add_vector(&put.cid, "idx", &good).unwrap();
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
    assert_eq!(hits[0].cid, put.cid);
}

/// #3/#4/#10 — a vector index name with path-traversal must be rejected, never
/// reach Path::join and write outside the store.
#[test]
fn traversal_index_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let png = make_png(3);
    let put = store.put(&png, Some("image/png"), Facts::new()).unwrap();
    let v = vec![1.0f32; 4];

    for evil in ["../escape", "a/b", "..", "/abs/path", "with\0nul", "dir/../x"] {
        assert!(
            store.add_vector(&put.cid, evil, &v).is_err(),
            "add_vector accepted a traversal index name: {evil:?}"
        );
        assert!(
            store.search("", &v, 1, evil).is_err(),
            "search accepted a traversal index name: {evil:?}"
        );
    }
    // Nothing was created outside the hnsw root by the rejected calls.
    let escaped = dir.path().parent().unwrap().join("escape");
    assert!(!escaped.exists(), "a traversal write escaped the store");

    // A legitimate name still works.
    assert!(store.add_vector(&put.cid, "qwen-vl-2b-2048", &v).is_ok());
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
    let f = facts_map(&store, &put.cid);
    assert_eq!(
        f.get("http://purl.org/dc/elements/1.1/title").map(|v| v.as_slice()),
        Some(["Adobe Title".to_string()].as_slice()),
        "standard-Adobe dc:title must ingest"
    );

    // A PNG with a corrupt XMP chunk must store fine, just without facts.
    let garbage = pan::xmp::write_packet_into_png_bytes(&make_png(5), "<not xml at all <<<").unwrap();
    let put2 = store.put(&garbage, Some("image/png"), Facts::new()).unwrap();
    assert!(store.get(&put2.cid).is_ok(), "garbage XMP must not fail the store");
}

/// #12/#15 — rdf:type from travel XMP survives ingest as an IRI, so a type
/// query (`?s a copia:Sam3Region`) matches.
#[test]
fn travel_rdf_type_survives_as_iri_for_type_queries() {
    const COPIA: &str = "https://repolex.ai/ontology/kit/copia/";
    let dir_a = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("pan.yml"), format!("prefixes:\n  copia: {COPIA}\n")).unwrap();
    let store_a = Pan::open(dir_a.path()).unwrap();
    let png = make_png(6);
    let put = store_a.put(&png, Some("image/png"), Facts::new()).unwrap();

    // Author a region sub-subject with an rdf:type onto the graph, re-stamp.
    let region = format!("urn:{}/Region/wolf/01", put.cid);
    store_a
        .describe_subject(&region, "http://www.w3.org/1999/02/22-rdf-syntax-ns#type", &format!("{COPIA}Sam3Region"), true)
        .unwrap();
    store_a
        .describe_subject(&region, &format!("{COPIA}regionDescriptor"), "wolf", false)
        .unwrap();
    store_a.restamp(&put.cid).unwrap();
    let (stamped, _) = store_a.get(&put.cid).unwrap();

    // Travel to a fresh store.
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_b.path().join("pan.yml"), format!("prefixes:\n  copia: {COPIA}\n")).unwrap();
    let store_b = Pan::open(dir_b.path()).unwrap();
    store_b.put(&stamped, Some("image/png"), Facts::new()).unwrap();

    // The type query must match — proves rdf:type ingested as an IRI.
    let results = store_b
        .query(&format!("ASK {{ <{region}> a <{COPIA}Sam3Region> }}"))
        .unwrap();
    match results {
        pan::QueryResults::Boolean(b) => assert!(b, "rdf:type degraded to a string; type query fails"),
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
    let region = format!("urn:{}/Region/sea/01", put.cid);
    store_a.describe_subject(&region, &format!("{COPIA}regionDescriptor"), "sea", false).unwrap();
    store_a.describe_subject(&region, &format!("{DC}creator"), "w4r3z", false).unwrap();
    store_a.restamp(&put.cid).unwrap();
    let (stamped, _) = store_a.get(&put.cid).unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_b.path().join("pan.yml"), format!("prefixes:\n  copia: {COPIA}\n  dc: {DC}\n")).unwrap();
    let store_b = Pan::open(dir_b.path()).unwrap();
    store_b.put(&stamped, Some("image/png"), Facts::new()).unwrap();

    let f: HashMap<String, Vec<String>> = store_b.facts_for(&region).unwrap().into_iter().collect();
    assert_eq!(f.get(&format!("{COPIA}regionDescriptor")).map(|v| v.as_slice()), Some(["sea".to_string()].as_slice()));
    assert_eq!(
        f.get(&format!("{DC}creator")).map(|v| v.as_slice()),
        Some(["w4r3z".to_string()].as_slice()),
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
