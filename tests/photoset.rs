//! Photosets over the Pan core (issue #4; pan.ttl 0.4.2, goodlux 2026-09-16):
//! a set is three facts in its own file and in the graph; membership is
//! pan:relatedToId on the image, in the graph AND in the image's XMP; the
//! graph is rebuilt from photosets/*.xml on open.

use pan::Pan;

fn make_png(w: u32, h: u32, seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        let px: Vec<u8> = (0..w * h * 3).map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed)).collect();
        writer.write_image_data(&px).unwrap();
        writer.finish().unwrap();
    }
    out
}

fn open_at(dir: &std::path::Path) -> Pan {
    if !dir.join("pan.yml").exists() {
        std::fs::write(dir.join("pan.yml"), "storage_id: test-store\nindex_id: test-idx\n").unwrap();
    }
    Pan::open(dir).unwrap()
}

fn xmp_of(store: &Pan, id: &str) -> String {
    let (bytes, _) = store.get(id).unwrap();
    pan::xmp::read_xmp_packet_from_bytes(&bytes).unwrap().expect("stored image carries XMP")
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
    let set = store.photoset_create(Some("  portraits  ")).unwrap();
    assert_eq!(set.description.as_deref(), Some("portraits"), "trimmed, one value");
    assert_eq!(set.iri, format!("{}Photoset/{}", pan::PAN_MEDIA_NS, set.id));

    // The file: photosets/<id>.xml at the store root, the three facts only.
    let file = dir.path().join("photosets").join(format!("{}.xml", set.id));
    let text = std::fs::read_to_string(&file).expect("the set has its own file");
    assert!(text.contains(&format!("<pan:id>&lt;pan/Photoset/{}&gt;</pan:id>", set.id)), "{text}");
    assert!(text.contains("<pan:description>portraits</pan:description>"), "{text}");
    assert!(text.contains("<pan:createdDate>"), "{text}");
    assert!(!text.contains("member") && !text.contains("git-lex"), "{text}");

    // The graph: the node under the universals.
    let listed = store.photoset_list().unwrap();
    assert_eq!(listed, vec![set.clone()]);
    assert_eq!(store.photoset_get(&set.id).unwrap(), Some(set.clone()));
    let mut typed = 0;
    if let pan::QueryResults::Solutions(sols) =
        store.query(&format!("SELECT ?d WHERE {{ <{}> a pan:Photoset ; git-lex:description ?d ; git-lex:createdDate ?c }}", set.iri)).unwrap()
    {
        for s in sols {
            s.unwrap();
            typed += 1;
        }
    }
    assert_eq!(typed, 1, "the set is a pan:Photoset with git-lex:description and git-lex:createdDate in the graph");

    // Membership: relatedToId from the image, in the graph and in its XMP.
    let a = store.put(&make_png(8, 8, 1), Some("image/png")).unwrap().id;
    let b = store.put(&make_png(8, 8, 2), Some("image/png")).unwrap().id;
    store.photoset_add(&set.id, &a).unwrap();
    store.photoset_add(&set.id, &b).unwrap();
    store.photoset_add(&set.id, &a).unwrap(); // twice is once
    assert_eq!(related_to(&store, &a), vec![set.iri.clone()], "one edge, the set's IRI, not a string");
    let xmp = xmp_of(&store, &a);
    assert!(xmp.contains(&format!("<pan:relatedToId>&lt;pan/Photoset/{}&gt;</pan:relatedToId>", set.id)), "{xmp}");
    assert_eq!(xmp.matches("<pan:relatedToId>").count(), 1, "{xmp}");
    let mut members = store.photoset_members(&set.id).unwrap();
    members.sort();
    let mut want = vec![format!("{}Image/{a}", pan::PAN_MEDIA_NS), format!("{}Image/{b}", pan::PAN_MEDIA_NS)];
    want.sort();
    assert_eq!(members, want, "members are read from the images, the set keeps no list");
    assert!(!std::fs::read_to_string(&file).unwrap().contains(&a), "adding a member does not touch the set's file");

    // Two sets, one image: two edges, two elements.
    let set2 = store.photoset_create(None).unwrap();
    assert_eq!(set2.description, None, "a set may have no description");
    store.photoset_add(&set2.id, &a).unwrap();
    assert_eq!(related_to(&store, &a).len(), 2);
    assert_eq!(xmp_of(&store, &a).matches("<pan:relatedToId>").count(), 2);

    // Remove: the edge goes from the graph and the XMP; the other set stays.
    store.photoset_remove(&set.id, &a).unwrap();
    store.photoset_remove(&set.id, &a).unwrap(); // not a member = no change
    assert_eq!(related_to(&store, &a), vec![set2.iri.clone()]);
    let xmp = xmp_of(&store, &a);
    assert!(!xmp.contains(&format!("Photoset/{}", set.id)), "{xmp}");
    assert!(xmp.contains(&format!("Photoset/{}", set2.id)), "{xmp}");
    assert_eq!(store.photoset_members(&set.id).unwrap(), vec![format!("{}Image/{b}", pan::PAN_MEDIA_NS)]);

    // Unknown set or image: refused, nothing written.
    assert!(store.photoset_add("zzzzzzzz", &a).unwrap_err().to_string().contains("photoset not found"));
    assert!(store.photoset_add(&set.id, "zzzzzzzz").unwrap_err().to_string().contains("id not found"));
    assert_eq!(store.photoset_get("zzzzzzzz").unwrap(), None);
}

#[test]
fn the_graph_is_rebuilt_from_the_set_files_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let (set, edited) = {
        let store = open_at(dir.path());
        let set = store.photoset_create(Some("first")).unwrap();
        // A set written by hand while pand was down: a file is enough.
        let edited = pan::Photoset {
            id: "handmade".into(),
            iri: format!("{}Photoset/handmade", pan::PAN_MEDIA_NS),
            description: Some("made by hand".into()),
            created_date: "2026-09-16T10:00:00-07:00".into(),
        };
        std::fs::write(dir.path().join("photosets/handmade.xml"), pan::photoset::build_photoset_file(&edited)).unwrap();
        // And the first set's file changed its description under the graph's feet.
        let changed = pan::Photoset { description: Some("renamed on disk".into()), ..set.clone() };
        std::fs::write(dir.path().join("photosets").join(format!("{}.xml", set.id)), pan::photoset::build_photoset_file(&changed)).unwrap();
        (set, edited)
    };
    let store = open_at(dir.path());
    let mut listed = store.photoset_list().unwrap();
    listed.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(listed.len(), 2, "both files became nodes");
    assert_eq!(store.photoset_get("handmade").unwrap(), Some(edited));
    assert_eq!(store.photoset_get(&set.id).unwrap().unwrap().description.as_deref(), Some("renamed on disk"), "the file wins over the graph");
}

#[test]
fn a_set_file_whose_name_and_id_disagree_refuses_the_open() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pan.yml"), "storage_id: test-store\n").unwrap();
    std::fs::create_dir_all(dir.path().join("photosets")).unwrap();
    let p = pan::Photoset {
        id: "abcd2345".into(),
        iri: format!("{}Photoset/abcd2345", pan::PAN_MEDIA_NS),
        description: None,
        created_date: "2026-09-16T10:00:00-07:00".into(),
    };
    std::fs::write(dir.path().join("photosets/other.xml"), pan::photoset::build_photoset_file(&p)).unwrap();
    let err = match Pan::open(dir.path()) {
        Ok(_) => panic!("a set file whose name and id disagree must refuse the open"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("file name and the id must agree"), "{err}");
}
