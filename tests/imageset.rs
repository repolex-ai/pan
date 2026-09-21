//! ImageSets over the Pan core (issue #4; pan.ttl 0.4.2, goodlux 2026-09-16;
//! renamed from Photoset, pan.ttl 0.4.7, goodlux 2026-09-17): a set is three
//! facts in its own file and in the graph; membership is pan:relatedToId on
//! the image, in the graph AND in the image's XMP; the graph is rebuilt from
//! ImageSet/*.nq on open; only a pan:Image may be a member.

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

fn open_at(dir: &std::path::Path) -> Pan {
    if !dir.join("pan.yml").exists() {
        std::fs::write(
            dir.join("pan.yml"),
            "storage_id: test-store\nindex_id: test-idx\n",
        )
        .unwrap();
    }
    Pan::open(dir).unwrap()
}

fn xmp_of(store: &Pan, id: &str) -> String {
    let (bytes, _) = store.get(id).unwrap();
    pan::xmp::read_xmp_packet_from_bytes(&bytes)
        .unwrap()
        .expect("stored image carries XMP")
}

fn related_to(store: &Pan, id: &str) -> Vec<String> {
    store
        .facts_for(id)
        .unwrap()
        .into_iter()
        .find(|(p, _)| p == &format!("{}relatedToId", pan::PAN_NS))
        .map(|(_, v)| v)
        .unwrap_or_default()
}

#[test]
fn a_set_is_a_file_and_a_node_and_membership_is_an_edge_from_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    let set = store.imageset_create(Some("  portraits  ")).unwrap();
    assert_eq!(
        set.description.as_deref(),
        Some("portraits"),
        "trimmed, one value"
    );
    assert_eq!(set.iri, format!("{}ImageSet/{}", pan::PAN_MEDIA_NS, set.id));

    // The file: ImageSet/<id>.nq at the store root, the folder named for the
    // class, N-Quads in Pan's graph, the set's own facts only.
    let file = dir.path().join("ImageSet").join(format!("{}.nq", set.id));
    let text = std::fs::read_to_string(&file).expect("the set has its own file");
    let ns = pan::PAN_NS;
    let node = format!("<{}>", set.iri);
    let graph = format!("<{}> .", pan::config::PAN_GRAPH_IRI);
    assert!(
        text.lines()
            .all(|l| l.starts_with(&node) && l.ends_with(&graph)),
        "{text}"
    );
    assert!(text.contains(&format!("{node} <{ns}id> {node} ")), "{text}");
    assert!(
        text.contains(&format!("<{ns}description> \"portraits\"")),
        "{text}"
    );
    assert!(text.contains(&format!("<{ns}createdDate> ")), "{text}");
    assert!(
        !text.contains("relatedToId") && !text.contains("git-lex"),
        "{text}"
    );

    // The graph: the node under the universals.
    let listed = store.imageset_list().unwrap();
    assert_eq!(listed, vec![set.clone()]);
    assert_eq!(store.imageset_get(&set.id).unwrap(), Some(set.clone()));
    let mut typed = 0;
    if let pan::QueryResults::Solutions(sols) = store
        .query(&format!(
            "SELECT ?d WHERE {{ <{}> a pan:ImageSet ; pan:description ?d ; pan:createdDate ?c }}",
            set.iri
        ))
        .unwrap()
    {
        for s in sols {
            s.unwrap();
            typed += 1;
        }
    }
    assert_eq!(
        typed, 1,
        "the set is a pan:ImageSet with pan:description and pan:createdDate in the graph"
    );

    // Membership: relatedToId from the image, in the graph and in its XMP.
    let a = store.put(&make_png(8, 8, 1), Some("image/png")).unwrap().id;
    let b = store.put(&make_png(8, 8, 2), Some("image/png")).unwrap().id;
    store.imageset_add(&set.id, &a).unwrap();
    store.imageset_add(&set.id, &b).unwrap();
    store.imageset_add(&set.id, &a).unwrap(); // twice is once
    assert_eq!(
        related_to(&store, &a),
        vec![set.iri.clone()],
        "one edge, the set's IRI, not a string"
    );
    let xmp = xmp_of(&store, &a);
    assert!(
        xmp.contains(&format!(
            "<pan:relatedToId>&lt;pan/ImageSet/{}&gt;</pan:relatedToId>",
            set.id
        )),
        "{xmp}"
    );
    assert_eq!(xmp.matches("<pan:relatedToId>").count(), 1, "{xmp}");
    let mut members = store.imageset_members(&set.id).unwrap();
    members.sort();
    let mut want = vec![
        format!("{}Image/{a}", pan::PAN_MEDIA_NS),
        format!("{}Image/{b}", pan::PAN_MEDIA_NS),
    ];
    want.sort();
    assert_eq!(
        members, want,
        "members are read from the images, the set keeps no list"
    );
    assert!(
        !std::fs::read_to_string(&file).unwrap().contains(&a),
        "adding a member does not touch the set's file"
    );

    // Two sets, one image: two edges, two elements.
    let set2 = store.imageset_create(None).unwrap();
    assert_eq!(set2.description, None, "a set may have no description");
    store.imageset_add(&set2.id, &a).unwrap();
    assert_eq!(related_to(&store, &a).len(), 2);
    assert_eq!(xmp_of(&store, &a).matches("<pan:relatedToId>").count(), 2);

    // Remove: the edge goes from the graph and the XMP; the other set stays.
    store.imageset_remove(&set.id, &a).unwrap();
    store.imageset_remove(&set.id, &a).unwrap(); // not a member = no change
    assert_eq!(related_to(&store, &a), vec![set2.iri.clone()]);
    let xmp = xmp_of(&store, &a);
    assert!(!xmp.contains(&format!("ImageSet/{}", set.id)), "{xmp}");
    assert!(xmp.contains(&format!("ImageSet/{}", set2.id)), "{xmp}");
    assert_eq!(
        store.imageset_members(&set.id).unwrap(),
        vec![format!("{}Image/{b}", pan::PAN_MEDIA_NS)]
    );

    // Unknown set or image: refused, nothing written.
    assert!(store
        .imageset_add("zzzzzzzz", &a)
        .unwrap_err()
        .to_string()
        .contains("imageset not found"));
    assert!(store
        .imageset_add(&set.id, "zzzzzzzz")
        .unwrap_err()
        .to_string()
        .contains("id not found"));
    assert_eq!(store.imageset_get("zzzzzzzz").unwrap(), None);
}

#[test]
fn an_imageset_refuses_media_that_is_not_an_image() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    let set = store.imageset_create(Some("images only")).unwrap();
    // Anything not image/* is stored as delivered under the base class pan:Media.
    let other = store
        .put(b"not a picture at all", Some("application/octet-stream"))
        .unwrap()
        .id;
    let err = store.imageset_add(&set.id, &other).unwrap_err().to_string();
    assert!(
        err.contains("pan:Media") && err.contains("not a pan:Image") && err.contains("images only"),
        "{err}"
    );
    assert_eq!(
        store.imageset_members(&set.id).unwrap(),
        Vec::<String>::new(),
        "nothing was written"
    );
    assert!(
        related_to(&store, &other).is_empty(),
        "no edge on the refused media"
    );
    let image = store.put(&make_png(8, 8, 9), Some("image/png")).unwrap().id;
    store.imageset_add(&set.id, &image).unwrap();
    assert_eq!(
        store.imageset_members(&set.id).unwrap(),
        vec![format!("{}Image/{image}", pan::PAN_MEDIA_NS)]
    );
}

#[test]
fn the_graph_is_rebuilt_from_the_set_files_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let (set, edited) = {
        let store = open_at(dir.path());
        let set = store.imageset_create(Some("first")).unwrap();
        // A set written by hand while pand was down: a file is enough.
        let edited = pan::ImageSet {
            id: "handmade".into(),
            iri: format!("{}ImageSet/handmade", pan::PAN_MEDIA_NS),
            description: Some("made by hand".into()),
            created_date: "2026-09-16T10:00:00-07:00".into(),
        };
        std::fs::write(
            dir.path().join("ImageSet/handmade.nq"),
            pan::imageset::build_imageset_file(&edited).unwrap(),
        )
        .unwrap();
        // And the first set's file changed its description under the graph's feet.
        let changed = pan::ImageSet {
            description: Some("renamed on disk".into()),
            ..set.clone()
        };
        std::fs::write(
            dir.path().join("ImageSet").join(format!("{}.nq", set.id)),
            pan::imageset::build_imageset_file(&changed).unwrap(),
        )
        .unwrap();
        (set, edited)
    };
    let store = open_at(dir.path());
    let mut listed = store.imageset_list().unwrap();
    listed.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(listed.len(), 2, "both files became nodes");
    assert_eq!(store.imageset_get("handmade").unwrap(), Some(edited));
    assert_eq!(
        store
            .imageset_get(&set.id)
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("renamed on disk"),
        "the file wins over the graph"
    );
}

#[test]
fn a_set_file_whose_name_and_id_disagree_refuses_the_open() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pan.yml"), "storage_id: test-store\n").unwrap();
    std::fs::create_dir_all(dir.path().join("ImageSet")).unwrap();
    let p = pan::ImageSet {
        id: "abcd2345".into(),
        iri: format!("{}ImageSet/abcd2345", pan::PAN_MEDIA_NS),
        description: None,
        created_date: "2026-09-16T10:00:00-07:00".into(),
    };
    std::fs::write(
        dir.path().join("ImageSet/other.nq"),
        pan::imageset::build_imageset_file(&p).unwrap(),
    )
    .unwrap();
    let err = match Pan::open(dir.path()) {
        Ok(_) => panic!("a set file whose name and id disagree must refuse the open"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("file name and the id must agree"), "{err}");
}

/// Deleting an image in a set takes the image, its records and every file a
/// stage wrote for it, and leaves the set exactly as it was.
#[test]
fn deleting_a_member_leaves_the_set_and_takes_every_stage_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    let set = store.imageset_create(Some("keepers")).unwrap();
    let a = store.put(&make_png(4, 4, 1), Some("image/png")).unwrap();
    let b = store.put(&make_png(4, 4, 2), Some("image/png")).unwrap();
    store.imageset_add(&set.id, &a.id).unwrap();
    store.imageset_add(&set.id, &b.id).unwrap();

    let rec = pan::enrich::EnrichmentRecord::new(pan::gen_pan_id(), "Caption", "m")
        .field("text", "a caption");
    let record_rel = store
        .write_enrichment(
            &a.id,
            "caption",
            "captionData",
            "m",
            std::slice::from_ref(&rec),
            Default::default(),
        )
        .unwrap();
    let mut details = serde_json::Map::new();
    details.insert("provider".into(), "salad".into());
    let embed_rel = store
        .write_embedding(&a.id, "idx", "idx", &[0.1, 0.2, 0.3, 0.4], &details)
        .unwrap();
    let files = [
        record_rel.clone(),
        embed_rel.clone(),
        embed_rel.replace(".nq", ".npy"),
        embed_rel.replace(".nq", ".json"),
    ];
    for f in &files {
        assert!(store.layout.abs(f).exists(), "{f} written");
    }

    store.delete(&a.id).unwrap();

    for f in &files {
        assert!(!store.layout.abs(f).exists(), "{f} removed with the image");
    }
    assert_eq!(
        store.imageset_get(&set.id).unwrap(),
        Some(set.clone()),
        "the set keeps every fact it had"
    );
    assert_eq!(
        store.imageset_members(&set.id).unwrap(),
        vec![b.iri.clone()],
        "the other member is still a member"
    );
    let left = match store
        .query(&format!(
            "SELECT ?s WHERE {{ ?s ?p ?o FILTER(CONTAINS(STR(?s), \"{}\") || CONTAINS(STR(?o), \"{}\")) }}",
            a.id, a.id
        ))
        .unwrap()
    {
        pan::QueryResults::Solutions(s) => s.count(),
        _ => 0,
    };
    assert_eq!(
        left, 0,
        "nothing in the graph still names the deleted image"
    );
    let records = match store
        .query("SELECT ?r WHERE { { ?r a pan:Caption } UNION { ?r a pan:Embedding } UNION { ?r a pan:Enrichment } }")
        .unwrap()
    {
        pan::QueryResults::Solutions(s) => s.count(),
        _ => 0,
    };
    assert_eq!(records, 0, "its references and records went with it");
}
