//! Regression tests for the defects the adversarial review fleet confirmed.
//! Each names the finding it pins so a future change that reopens the hole
//! fails loudly here. (Updated for the Day-50 identity model: assigned panId,
//! standard https subject IRIs, no content-addressing.)

use pan::Pan;
use std::collections::HashMap;

fn make_png(seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 10, 10);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        let px: Vec<u8> = (0..300)
            .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
            .collect();
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

    let a = store.put(&png, Some("image/png")).unwrap();
    let b = store.put(&png, Some("image/png")).unwrap();
    assert_ne!(a.id, b.id, "assigned ids never collide on same bytes");
    assert_ne!(
        a.media_path, b.media_path,
        "each object owns its own media file"
    );

    let fa = facts_map(&store, &a.id);
    assert_eq!(
        fa["https://repolex.ai/ontology/pan/mediaPath"].len(),
        1,
        "exactly one mediaPath"
    );
    assert_eq!(
        fa["https://repolex.ai/ontology/pan/mediaType"].len(),
        1,
        "exactly one mediaType"
    );

    // Deleting one object leaves the other fully intact.
    store.delete(&a.id).unwrap();
    assert!(store.facts_for(&a.id).unwrap().is_empty());
    assert!(
        store.get(&b.id).is_ok(),
        "sibling object untouched by delete"
    );
    // And no media file is orphaned for the deleted one.
    let leftover = walk_files(&store.layout.media_root);
    assert_eq!(
        leftover.len(),
        2,
        "exactly the sibling's media file + thumbnail remain: {leftover:?}"
    );
    assert!(
        leftover.iter().all(|p| p.to_string_lossy().contains(&b.id)),
        "every remaining file is the sibling's: {leftover:?}"
    );
}

/// #2/#6 — one malformed /search must not poison a valid index's dim for the
/// process. After a wrong-length query errors, a correct-length query works.
#[test]
fn wrong_dim_query_does_not_poison_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let png = make_png(2);
    let put = store.put(&png, Some("image/png")).unwrap();

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
    assert!(
        store.search("", &bad, 3, "idx").is_err(),
        "dim-3 query should error against dim-8 index"
    );
    // Now a CORRECT-length query must still work.
    let hits = store.search("", &good, 3, "idx").unwrap();
    assert_eq!(
        hits.len(),
        1,
        "valid dim-8 query rejected — index dim was poisoned"
    );
    assert_eq!(hits[0].id, put.id);
}

/// #3/#4/#10 — a vector index name with path-traversal must be rejected, never
/// reach Path::join and write outside the store. Same for a traversal panId.
#[test]
fn traversal_index_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let png = make_png(3);
    let put = store.put(&png, Some("image/png")).unwrap();
    let v = vec![1.0f32; 4];

    for evil in [
        "../escape",
        "a/b",
        "..",
        "/abs/path",
        "with\0nul",
        "dir/../x",
    ] {
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
/// fail the store. An outright-garbage XMP is refused whole (Rob, 2026-09-04:
/// the file's XMP IS its metadata; a lenient reader hides invalid data).
#[test]
fn standard_adobe_xmp_ingests_and_garbage_is_refused() {
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
    std::fs::write(
        dir.path().join("pan.yml"),
        "prefixes:\n  dc: http://purl.org/dc/elements/1.1/\n",
    )
    .unwrap();
    let store = Pan::open(dir.path()).unwrap();

    // Must NOT error — the whole point is real-world files ingest.
    let put = store.put(&adobe_png, Some("image/png")).unwrap();
    let f = facts_map(&store, &put.id);
    assert_eq!(
        f.get("http://purl.org/dc/elements/1.1/title")
            .map(|v| v.as_slice()),
        Some(["Adobe Title".to_string()].as_slice()),
        "standard-Adobe dc:title must ingest"
    );

    // A PNG with a corrupt XMP chunk is refused, and leaves no file behind.
    let before = walk_files(&store.layout.media_root).len();
    let garbage =
        pan::xmp::write_packet_into_png_bytes(&make_png(5), "<not xml at all <<<").unwrap();
    let err = store.put(&garbage, Some("image/png")).unwrap_err();
    assert!(
        err.to_string().contains("XMP"),
        "says what was wrong: {err}"
    );
    assert_eq!(
        walk_files(&store.layout.media_root).len(),
        before,
        "refused file left nothing on disk"
    );
}

/// A producer's copia block (Rob, 2026-09-03/04): the producer writes it into
/// the image's own XMP before handing the file over; Pan loads it into the
/// graph unchanged and typed, keeps it verbatim beside its own block — and it
/// TRAVELS: a second store receiving the same bytes reads the same copia facts.
#[test]
fn copia_block_in_the_files_xmp_is_loaded_kept_and_travels() {
    const COPIA: &str = "https://repolex.ai/ontology/copia/";
    let dir_a = tempfile::tempdir().unwrap();
    let store_a = Pan::open(dir_a.path()).unwrap();
    let block = format!(
        "<rdf:Description rdf:about=\"https://repolex.ai/copia/Moment/3hyh7rwekpmq\" xmlns:copia=\"{COPIA}\">\
           <copia:momentId>3hyh7rwekpmq</copia:momentId>\
           <copia:origin>smoke</copia:origin>\
           <copia:genSteps rdf:datatype=\"http://www.w3.org/2001/XMLSchema#integer\">12</copia:genSteps>\
         </rdf:Description>"
    );
    let packet = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n{block}\n</rdf:RDF>\n\
         </x:xmpmeta>\n<?xpacket end=\"w\"?>"
    );
    let png = pan::xmp::write_packet_into_png_bytes(&make_png(6), &packet).unwrap();
    let put = store_a.put(&png, Some("image/png")).unwrap();
    assert_eq!(
        put.statements, 3,
        "every statement in the file's XMP is loaded"
    );
    let typed = store_a
        .query(&format!(
            "ASK {{ <https://repolex.ai/copia/Moment/3hyh7rwekpmq> <{COPIA}genSteps> 12 }}"
        ))
        .unwrap();
    assert!(
        matches!(typed, pan::QueryResults::Boolean(true)),
        "rdf:datatype survives: genSteps is the integer 12"
    );
    let ask_a = store_a
        .query(&format!(
            "ASK {{ <https://repolex.ai/copia/Moment/3hyh7rwekpmq> <{COPIA}origin> \"smoke\" }}"
        ))
        .unwrap();
    assert!(
        matches!(ask_a, pan::QueryResults::Boolean(true)),
        "copia facts loaded unchanged"
    );

    let (bytes, _) = store_a.get(&put.id).unwrap();
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes)
        .unwrap()
        .expect("XMP written into the image");
    assert!(
        packet.contains("<copia:origin>smoke</copia:origin>"),
        "block verbatim in the packet"
    );
    assert!(
        packet.contains("pan:mediaPath"),
        "Pan's own block present too"
    );

    // Travel: a fresh store reads the same copia facts back out of the bytes.
    let dir_b = tempfile::tempdir().unwrap();
    let store_b = Pan::open(dir_b.path()).unwrap();
    let put_b = store_b.put(&bytes, Some("image/png")).unwrap();
    assert_eq!(
        put_b.statements, 3,
        "the copia statements, and not the first store's pan block, are read back"
    );
    let ask_b = store_b
        .query(&format!(
            "ASK {{ <https://repolex.ai/copia/Moment/3hyh7rwekpmq> <{COPIA}origin> \"smoke\" }}"
        ))
        .unwrap();
    assert!(
        matches!(ask_b, pan::QueryResults::Boolean(true)),
        "copia facts travel with the image"
    );
    let stray = store_b
        .query(&format!(
            "ASK {{ ?s <https://repolex.ai/ontology/git-lex/id> <{}> }}",
            put.iri
        ))
        .unwrap();
    assert!(
        matches!(stray, pan::QueryResults::Boolean(false)),
        "the first store's identity did not travel"
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
