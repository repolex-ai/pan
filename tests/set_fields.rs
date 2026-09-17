//! `pan set` / `pan unset` over the Pan core (issue #19): a person's facts —
//! rating, isPicked, isRejected — land in the graph AND in the image's XMP,
//! overwrite on re-set, come off on unset, and every other property is refused
//! with the settable list (goodlux, 2026-09-16).

use pan::Pan;

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

fn pan_fact(store: &Pan, id: &str, local: &str) -> Vec<String> {
    store
        .facts_for(id)
        .unwrap()
        .into_iter()
        .find(|(p, _)| p == &format!("{}{local}", pan::PAN_NS))
        .map(|(_, v)| v)
        .unwrap_or_default()
}

fn xmp_of(store: &Pan, id: &str) -> String {
    let (bytes, _) = store.get(id).unwrap();
    pan::xmp::read_xmp_packet_from_bytes(&bytes)
        .unwrap()
        .expect("stored image carries XMP")
}

#[test]
fn set_writes_graph_and_xmp_overwrite_replaces_unset_removes() {
    let (_dir, store) = open();
    let put = store.put(&make_png(8, 8, 3), Some("image/png")).unwrap();
    let id = put.id.clone();

    store
        .set_fields(
            &id,
            &[
                ("rating".into(), serde_json::json!(4)),
                ("isPicked".into(), serde_json::json!(true)),
            ],
        )
        .unwrap();
    assert_eq!(pan_fact(&store, &id, "rating"), ["4"]);
    assert_eq!(pan_fact(&store, &id, "isPicked"), ["true"]);
    let xmp = xmp_of(&store, &id);
    assert!(xmp.contains("<pan:rating>4</pan:rating>"), "{xmp}");
    assert!(xmp.contains("<pan:isPicked>true</pan:isPicked>"), "{xmp}");

    // Re-set overwrites: one value, never two.
    store
        .set_fields(&id, &[("rating".into(), serde_json::json!("2"))])
        .unwrap();
    assert_eq!(pan_fact(&store, &id, "rating"), ["2"]);
    let xmp = xmp_of(&store, &id);
    assert!(
        xmp.contains("<pan:rating>2</pan:rating>") && !xmp.contains("<pan:rating>4</pan:rating>"),
        "{xmp}"
    );

    // Typed so SPARQL can compare: FILTER(?r >= 2) finds it.
    let mut hits = 0;
    if let pan::QueryResults::Solutions(sols) = store
        .query("SELECT ?s WHERE { ?s pan:rating ?r . FILTER(?r >= 2) }")
        .unwrap()
    {
        for s in sols {
            s.unwrap();
            hits += 1;
        }
    }
    assert_eq!(hits, 1, "rating is a typed integer in the graph");

    store.unset_fields(&id, &["rating".into()]).unwrap();
    assert!(pan_fact(&store, &id, "rating").is_empty());
    assert!(!xmp_of(&store, &id).contains("<pan:rating>"));
    assert_eq!(
        pan_fact(&store, &id, "isPicked"),
        ["true"],
        "unset touches only the named field"
    );
}

#[test]
fn refusals_name_the_problem_and_write_nothing() {
    let (_dir, store) = open();
    let id = store.put(&make_png(8, 8, 5), Some("image/png")).unwrap().id;

    let e = store
        .set_fields(&id, &[("vibe".into(), serde_json::json!("x"))])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("vibe") && e.contains("settable: isPicked, isRejected, rating"),
        "{e}"
    );

    let e = store
        .set_fields(&id, &[("rating".into(), serde_json::json!(6))])
        .unwrap_err()
        .to_string();
    assert!(e.contains("0 to 5"), "{e}");

    let e = store
        .set_fields(
            &id,
            &[("shortDescription".into(), serde_json::json!("mine"))],
        )
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("shortDescription") && e.contains("not a property a person may set"),
        "{e}"
    );

    // One bad key refuses the whole request: the good key was not written.
    let e = store
        .set_fields(
            &id,
            &[
                ("isRejected".into(), serde_json::json!(true)),
                ("mediaPath".into(), serde_json::json!("x")),
            ],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("mediaPath"), "{e}");
    assert!(
        pan_fact(&store, &id, "isRejected").is_empty(),
        "nothing written when one key is refused"
    );

    let e = store
        .unset_fields(&id, &["longDescription".into()])
        .unwrap_err()
        .to_string();
    assert!(e.contains("longDescription"), "{e}");
}
