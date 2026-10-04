//! Redo one stage for one image: forget its result so the stage runs it
//! again. Written on 2026-10-04 to re-embed six images after the text the
//! embedding is built from changed.

use pan::Pan;

const INDEX: &str = "qwen3-vl-embedding-2b";

fn make_png(seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 8, 8);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        let px: Vec<u8> = (0..192)
            .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed))
            .collect();
        w.write_image_data(&px).unwrap();
        w.finish().unwrap();
    }
    out
}

fn open() -> (tempfile::TempDir, Pan) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pan.yml"),
        "storage_id: test-store\nindex_id: test-idx\n",
    )
    .unwrap();
    let store = Pan::open(dir.path()).unwrap();
    (dir, store)
}

fn facts(store: &Pan, id: &str) -> std::collections::HashMap<String, Vec<String>> {
    store.facts_for(id).unwrap().into_iter().collect()
}

#[test]
fn forgetting_an_embedding_removes_record_files_index_entry_and_makes_it_pending_again() {
    let (_dir, store) = open();
    let put = store.put(&make_png(1), Some("image/png")).unwrap();
    let p = pan::Perception::parse(
        "{\"shortCaption\": \"s\", \"longCaption\": \"a long description\", \"sceneObjects\": [\"wolf\"]}",
    )
    .unwrap();
    store.set_perception(&put.id, &p).unwrap();
    let vector: Vec<f32> = (0..8).map(|i| i as f32 / 8.0).collect();
    let extra = serde_json::Map::new();
    let rel = store
        .write_embedding(&put.id, INDEX, INDEX, &vector, &extra)
        .unwrap();
    let nq = store.layout.abs(&rel);
    let npy = nq.with_extension("npy");
    assert!(nq.exists() && npy.exists(), "record and vector on disk");
    assert!(store
        .pending_for("vectorData", INDEX, 10, None)
        .unwrap()
        .is_empty());
    let f = facts(&store, &put.id);
    assert!(f.contains_key(&format!("{}vectorData", pan::PAN_NS)));

    let r = store.forget_enrichment(&put.id, "vectorData").unwrap();
    assert!(
        r.files_removed >= 2,
        "record file and vector removed: {r:?}"
    );
    assert!(r.facts_removed > 0);
    assert!(!nq.exists() && !npy.exists());
    let f = facts(&store, &put.id);
    assert!(!f.contains_key(&format!("{}vectorData", pan::PAN_NS)));
    assert!(
        f.contains_key(&format!("{}longCaption", pan::PAN_NS)),
        "the caption stays"
    );
    let pending = store.pending_for("vectorData", INDEX, 10, None).unwrap();
    assert_eq!(pending.len(), 1, "pending for embed again");
    assert_eq!(pending[0].id, put.id);
    // The XMP no longer names the reference.
    let bytes = std::fs::read(store.layout.abs(&put.media_path)).unwrap();
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes)
        .unwrap()
        .unwrap();
    assert!(!packet.contains("vectorData"), "{packet}");
    // Writing it again works: the index entry was released.
    store
        .write_embedding(&put.id, INDEX, INDEX, &vector, &extra)
        .unwrap();
    assert!(store
        .pending_for("vectorData", INDEX, 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn forgetting_a_stage_that_never_ran_is_a_no_op_and_a_bad_name_is_refused() {
    let (_dir, store) = open();
    let put = store.put(&make_png(2), Some("image/png")).unwrap();
    let r = store.forget_enrichment(&put.id, "poseData").unwrap();
    assert_eq!((r.files_removed, r.facts_removed), (0, 0));
    assert!(store.forget_enrichment(&put.id, "rating").is_err());
}
