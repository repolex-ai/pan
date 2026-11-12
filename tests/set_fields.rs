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
        e.contains("vibe") && e.contains("declared: ") && e.contains("rating"),
        "{e}"
    );

    let e = store
        .set_fields(&id, &[("rating".into(), serde_json::json!(6))])
        .unwrap_err()
        .to_string();
    assert!(e.contains("0 to 5"), "{e}");

    // A field the caption stage writes may be overwritten by hand: pan set
    // accepts every property pan.ttl declares on an image (goodlux,
    // 2026-09-22). A name the ontology does not declare is still refused.
    store
        .set_fields(&id, &[("shortCaption".into(), serde_json::json!("mine"))])
        .unwrap();
    let e = store
        .set_fields(
            &id,
            &[("favouriteColour".into(), serde_json::json!("teal"))],
        )
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("favouriteColour") && e.contains("not a property pan.ttl declares"),
        "{e}"
    );

    // A path that names no file cannot be written into the image, so the
    // whole request is undone: the good key was not written either, and
    // the old path still stands.
    let old_path = pan_fact(&store, &id, "mediaPath");
    let e = format!(
        "{:#}",
        store
            .set_fields(
                &id,
                &[
                    ("isRejected".into(), serde_json::json!(true)),
                    ("mediaPath".into(), serde_json::json!("x")),
                ],
            )
            .unwrap_err()
    );
    assert!(e.contains("nothing was changed"), "{e}");
    assert!(
        pan_fact(&store, &id, "isRejected").is_empty(),
        "nothing written when the rewrite fails"
    );
    assert_eq!(pan_fact(&store, &id, "mediaPath"), old_path);

    // Any declared field may be unset; an undeclared name is refused.
    store.unset_fields(&id, &["longCaption".into()]).unwrap();
    let e = store
        .unset_fields(&id, &["vibe".into()])
        .unwrap_err()
        .to_string();
    assert!(e.contains("vibe"), "{e}");
}

#[test]
fn related_to_id_adds_links_one_at_a_time_and_unset_names_the_one_to_drop() {
    // goodlux, 2026-11-12: putting media in a set and linking a render to the
    // image it was made from must be one key=value on set or put, never an
    // XMP upload. relatedToId ADDS a link; the links already there stay.
    let (_dir, store) = open();
    let a = store.put(&make_png(8, 8, 11), Some("image/png")).unwrap().id;
    let b = store.put(&make_png(8, 8, 12), Some("image/png")).unwrap().id;
    let set = store.imageset_create(Some("test set")).unwrap().id;
    let a_bracket = format!("<pan/Image/{}>", pan::bare_id(&a));
    let b_bracket = format!("<pan/Image/{}>", pan::bare_id(&b));
    let set_bracket = format!("<pan/ImageSet/{set}>");

    // One call, two links: the set and the source image.
    store
        .set_fields(
            &a,
            &[(
                "relatedToId".into(),
                serde_json::json!([set_bracket, b_bracket]),
            )],
        )
        .unwrap();
    let links = pan_fact(&store, &a, "relatedToId");
    assert_eq!(links.len(), 2, "{links:?}");
    assert!(links.iter().any(|l| l.ends_with(&format!("/ImageSet/{set}"))));
    assert!(links.iter().any(|l| l == &format!("https://repolex.ai/pan/Image/{}", pan::bare_id(&b))));
    assert_eq!(store.imageset_members(&set).unwrap().len(), 1, "the set door sees the link");
    let xmp = xmp_of(&store, &a);
    assert!(xmp.contains(&format!("<pan:relatedToId>&lt;pan/ImageSet/{set}&gt;</pan:relatedToId>")), "{xmp}");
    assert!(xmp.contains(&format!("<pan:relatedToId>{}</pan:relatedToId>", b_bracket.replace('<', "&lt;").replace('>', "&gt;"))), "{xmp}");

    // Setting another link alongside a value keeps the first two.
    store
        .set_fields(&a, &[("rating".into(), serde_json::json!(3)), ("relatedToId".into(), serde_json::json!(set_bracket.clone()))])
        .unwrap();
    assert_eq!(pan_fact(&store, &a, "relatedToId").len(), 2, "already linked = no change");
    assert_eq!(pan_fact(&store, &a, "rating"), vec!["3"]);

    // Unset takes one link off, named; the other stays.
    store.unset_fields(&a, &[format!("relatedToId={b_bracket}")]).unwrap();
    let links = pan_fact(&store, &a, "relatedToId");
    assert_eq!(links.len(), 1, "{links:?}");
    assert!(links[0].ends_with(&format!("/ImageSet/{set}")));
    assert!(!xmp_of(&store, &a).contains(&b_bracket.replace('<', "&lt;").replace('>', "&gt;")));

    // Bare relatedToId on unset is refused; the set link is untouched.
    let e = store.unset_fields(&a, &["relatedToId".into()]).unwrap_err().to_string();
    assert!(e.contains("say which one"), "{e}");
    assert_eq!(pan_fact(&store, &a, "relatedToId").len(), 1);

    // Refusals: the media itself, a thing that is not here, a thing outside
    // Pan (the producer's to write), a set that does not exist, and a value
    // that is not a reference at all. Nothing is written by any of them.
    for (value, expect) in [
        (a_bracket.as_str(), "the media itself"),
        ("<pan/Image/nope1234>", "not an image, video or set in this store"),
        ("<copia/Moment/abc-1-2>", "outside Pan"),
        ("<pan/ImageSet/nosuchset>", "imageset not found"),
        ("just-text", "not a reference"),
    ] {
        let e = store
            .set_fields(&a, &[("relatedToId".into(), serde_json::json!(value))])
            .unwrap_err()
            .to_string();
        assert!(e.contains(expect), "{value}: {e}");
        assert_eq!(pan_fact(&store, &a, "relatedToId").len(), 1, "{value} wrote something");
    }
    // The refused-key message points at relatedToId.
    let e = store
        .set_fields(&a, &[("bogus".into(), serde_json::json!(1))])
        .unwrap_err()
        .to_string();
    assert!(e.contains("relatedToId=<pan/ImageSet/id>"), "{e}");
}
