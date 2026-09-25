//! What pand keeps from an arriving file's pan: statements (goodlux,
//! 2026-09-22 and 2026-09-24, for the Pool migration, issues #66 and #69):
//! `pan:mediaCreatedDate`, checked as an RFC3339 date with a zone, and
//! `pan:relatedToId <pan/ImageSet/id>`, which makes the set when it does not
//! exist. Everything else pan: in the file still stays out.

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

/// A producer's packet: one root Description about the file itself.
fn png_with(seed: u8, root_fields: &str) -> Vec<u8> {
    let packet = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         <rdf:Description rdf:about=\"\" xmlns:pan=\"https://repolex.ai/ontology/pan/\" xmlns:copia=\"https://repolex.ai/ontology/copia/\">\n\
         {root_fields}\n\
         </rdf:Description>\n\
         </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>"
    );
    pan::xmp::write_packet_into_png_bytes(&make_png(8, 8, seed), &packet).unwrap()
}

fn open_at(dir: &std::path::Path) -> Pan {
    std::fs::write(
        dir.join("pan.yml"),
        "storage_id: test-store\nindex_id: test-idx\n",
    )
    .unwrap();
    Pan::open(dir).unwrap()
}

fn facts(store: &Pan, id: &str) -> std::collections::HashMap<String, Vec<String>> {
    store.facts_for(id).unwrap().into_iter().collect()
}

const P: &str = "https://repolex.ai/ontology/pan/";

#[test]
fn media_created_date_from_the_file_lands_in_the_graph_and_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    let png = png_with(
        1,
        "<pan:mediaCreatedDate>2026-09-21T17:00:01-07:00</pan:mediaCreatedDate>\n\
         <pan:relatedToId>&lt;copia/Moment/4y8at4fdmkkt-2-2&gt;</pan:relatedToId>\n\
         <copia:origin>direct-prompt</copia:origin>\n\
         <pan:mediaPath>somewhere/else.png</pan:mediaPath>",
    );
    let put = store.put(&png, Some("image/png")).unwrap();
    let f = facts(&store, &put.id);
    assert_eq!(
        f[&format!("{P}mediaCreatedDate")],
        vec!["2026-09-21T17:00:01-07:00".to_string()],
        "the producer's date is the media's date"
    );
    assert_ne!(
        f[&format!("{P}createdDate")],
        f[&format!("{P}mediaCreatedDate")],
        "the record's date is pand's own"
    );
    assert_eq!(
        f[&format!("{P}relatedToId")],
        vec!["https://repolex.ai/copia/Moment/4y8at4fdmkkt-2-2".to_string()]
    );
    assert_eq!(
        f[&format!("{P}mediaPath")],
        vec![put.media_path.clone()],
        "a previous store's pan:mediaPath stays out"
    );
    assert!(put.imagesets_made.is_empty());
    let (bytes, _) = store.get(&put.id).unwrap();
    let xmp = pan::xmp::read_xmp_packet_from_bytes(&bytes)
        .unwrap()
        .unwrap();
    assert!(
        xmp.contains("2026-09-21T17:00:01-07:00"),
        "the stored file carries the media date"
    );
}

#[test]
fn a_media_created_date_that_is_not_a_zoned_date_refuses_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    for bad in ["yesterday", "2026-09-21T17:00:01", "20260921-170001"] {
        let png = png_with(
            2,
            &format!("<pan:mediaCreatedDate>{bad}</pan:mediaCreatedDate>"),
        );
        let err = store.put(&png, Some("image/png")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("pan:mediaCreatedDate") && msg.contains("Nothing was stored"),
            "{bad}: {msg}"
        );
    }
    let two = png_with(
        3,
        "<pan:mediaCreatedDate>2026-09-21T17:00:01-07:00</pan:mediaCreatedDate>\n\
         <pan:mediaCreatedDate>2026-09-22T17:00:01-07:00</pan:mediaCreatedDate>",
    );
    let msg = format!("{:#}", store.put(&two, Some("image/png")).unwrap_err());
    assert!(msg.contains("2 pan:mediaCreatedDate values"), "{msg}");
    assert_eq!(store.counts().unwrap().images, 0, "nothing stored");
}

#[test]
fn an_arriving_image_makes_the_set_it_names_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    // A producer's long name, as the Pool's set names are.
    let sid = "20260921-waking-room-french-maid-sylkie-blush-two-selkies-reverse-square";
    assert!(sid.len() > 64);
    let field = format!("<pan:relatedToId>&lt;pan/ImageSet/{sid}&gt;</pan:relatedToId>");
    let first = store.put(&png_with(4, &field), Some("image/png")).unwrap();
    assert_eq!(first.imagesets_made, vec![sid.to_string()]);
    let set = store.imageset_get(sid).unwrap().expect("the set was made");
    assert!(
        store.imagesets_root().join(format!("{sid}.nq")).exists(),
        "the set has its file"
    );
    assert_eq!(
        store.imageset_members(sid).unwrap(),
        vec![first.iri.clone()]
    );

    let second = store.put(&png_with(5, &field), Some("image/png")).unwrap();
    assert!(
        second.imagesets_made.is_empty(),
        "an existing set is not remade"
    );
    let again = store.imageset_get(sid).unwrap().unwrap();
    assert_eq!(
        again.created_date, set.created_date,
        "the set is the same set"
    );
    let mut members = store.imageset_members(sid).unwrap();
    members.sort();
    let mut want = vec![first.iri.clone(), second.iri.clone()];
    want.sort();
    assert_eq!(members, want);
}

#[test]
fn a_set_name_pand_cannot_use_refuses_the_file_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    let sid = "x".repeat(201);
    let field = format!("<pan:relatedToId>&lt;pan/ImageSet/{sid}&gt;</pan:relatedToId>");
    let msg = format!(
        "{:#}",
        store
            .put(&png_with(6, &field), Some("image/png"))
            .unwrap_err()
    );
    assert!(
        msg.contains("could not be made") && msg.contains("invalid"),
        "{msg}"
    );
    assert_eq!(store.counts().unwrap().images, 0);
    assert!(store.imageset_list().unwrap().is_empty());
}
