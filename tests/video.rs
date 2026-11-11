//! Video in Pan (pan.ttl 0.4.22): an MP4 or MOV delivered like an image
//! becomes a pan:Video with the IRI <pan/Video/id>, is stored as delivered
//! under video/, carries duration, frame rate and size read from its own
//! header, gets a thumbnail from its first frame, and carries Pan's XMP as
//! the last box of the file, rewritten in place when a person sets a
//! rating, a flag, a description or a link to the image it was made from.
//! The image stages never see it.

use pan::Pan;

const MP4: &[u8] = include_bytes!("fixtures/video/tiny.mp4");
const MOV: &[u8] = include_bytes!("fixtures/video/tiny.mov");

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

fn rows(store: &Pan, q: &str, vars: &[&str]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    if let pan::QueryResults::Solutions(sols) = store.query(q).unwrap() {
        for s in sols {
            let s = s.unwrap();
            out.push(
                vars.iter()
                    .map(|v| match s.get(*v) {
                        Some(oxigraph::model::Term::Literal(l)) => l.value().to_string(),
                        Some(oxigraph::model::Term::NamedNode(n)) => n.as_str().to_string(),
                        _ => String::new(),
                    })
                    .collect(),
            );
        }
    }
    out
}

fn fact(store: &Pan, iri: &str, local: &str) -> Vec<String> {
    rows(store, &format!("SELECT ?v WHERE {{ <{iri}> pan:{local} ?v }}"), &["v"])
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

#[test]
fn an_mp4_lands_as_a_video_with_its_header_facts_a_thumbnail_and_xmp_in_the_file() {
    let (_dir, store) = open();
    let put = store.put(MP4, Some("video/mp4")).unwrap();
    assert_eq!(put.iri, format!("{}Video/{}", pan::config::PAN_MEDIA_NS, put.id));
    assert!(put.media_path.starts_with("video/img/source/"), "{}", put.media_path);
    assert!(put.media_path.ends_with(".mp4"), "{}", put.media_path);
    assert_eq!(put.original_path, None, "stored as delivered, nothing converted");
    assert_eq!((put.width, put.height), (Some(16), Some(16)));
    assert!(put.thumbnail, "a thumbnail from the first frame");

    let iri = put.iri.as_str();
    assert_eq!(fact(&store, iri, "mediaType"), vec!["video/mp4"]);
    assert_eq!(fact(&store, iri, "duration"), vec!["0.5"]);
    assert_eq!(fact(&store, iri, "frameRate"), vec!["24"]);
    assert_eq!(fact(&store, iri, "width"), vec!["16"]);
    assert_eq!(fact(&store, iri, "height"), vec!["16"]);
    assert_eq!(fact(&store, iri, "sourceFile"), vec![put.media_path.clone()]);
    assert!(fact(&store, iri, "imageSha256Hash").is_empty(), "declared on images only");
    let class = rows(&store, &format!("SELECT ?c WHERE {{ <{iri}> a ?c }}"), &["c"]);
    assert_eq!(class, vec![vec![format!("{}Video", pan::config::PAN_NS)]]);
    assert_eq!(store.subject_for(&put.id).unwrap().unwrap().as_str(), iri);

    // The file: every delivered byte first, then one XMP box carrying the
    // same facts the graph has.
    let abs = store.layout.abs(&put.media_path);
    let on_disk = std::fs::read(&abs).unwrap();
    assert_eq!(&on_disk[..MP4.len()], MP4, "the delivered bytes are untouched");
    let packet = pan::video::read_xmp(&on_disk).unwrap().expect("an XMP box");
    for needle in [
        &format!("<pan:id>&lt;pan/Video/{}&gt;</pan:id>", put.id),
        "<pan:duration>0.5</pan:duration>",
        "<pan:frameRate>24</pan:frameRate>",
        "<pan:mediaType>video/mp4</pan:mediaType>",
        "<pan:thumbnail rdf:parseType=\"Resource\">",
    ] {
        assert!(packet.contains(needle), "file XMP carries {needle}: {packet}");
    }
    // The header still reads with the box behind it.
    assert_eq!(pan::video::info(&on_disk).unwrap().duration, 0.5);

    // Thumbnail on disk, JPEG, named on the graph.
    let thumb = fact(&store, iri, "thumbnail");
    assert_eq!(thumb.len(), 1);
    let tpath = store.node_field(&thumb[0], "path").unwrap().unwrap();
    let tbytes = std::fs::read(store.layout.abs(&tpath)).unwrap();
    assert_eq!(&tbytes[..3], &[0xFF, 0xD8, 0xFF], "JPEG");

    // The image stages have nothing to do with it.
    assert!(store
        .pending_for("captionData", "m", 10, None)
        .unwrap()
        .iter()
        .all(|p| p.id != put.id));
    assert_eq!(store.counts().unwrap().images, 0, "not an image");

    // The MOV, the same way.
    let mov = store.put(MOV, Some("video/quicktime")).unwrap();
    assert!(mov.media_path.ends_with(".mov"));
    assert_eq!(fact(&store, &mov.iri, "mediaType"), vec!["video/quicktime"]);
    assert_eq!(fact(&store, &mov.iri, "frameRate"), vec!["30"]);
    assert_eq!(
        pan::video::sniff(&std::fs::read(store.layout.abs(&mov.media_path)).unwrap()),
        Some("video/quicktime")
    );
}

#[test]
fn a_person_sets_fields_on_a_video_and_the_file_tail_changes_in_place() {
    let (_dir, store) = open();
    let img = store.put(&make_png(4, 4, 3), Some("image/png")).unwrap();
    let put = store.put(MP4, Some("video/mp4")).unwrap();
    let abs = store.layout.abs(&put.media_path);
    let before = std::fs::read(&abs).unwrap();

    store
        .set_fields(
            &put.id,
            &[
                ("rating".to_string(), serde_json::json!(4)),
                ("isPicked".to_string(), serde_json::json!(true)),
                ("description".to_string(), serde_json::json!("the cat turns")),
            ],
        )
        .unwrap();
    assert_eq!(fact(&store, &put.iri, "rating"), vec!["4"]);
    assert_eq!(fact(&store, &put.iri, "description"), vec!["the cat turns"]);

    let after = std::fs::read(&abs).unwrap();
    assert_eq!(&after[..MP4.len()], MP4, "picture data untouched");
    let packet = pan::video::read_xmp(&after).unwrap().unwrap();
    assert!(packet.contains("<pan:rating>4</pan:rating>"), "{packet}");
    assert!(packet.contains("<pan:isPicked>true</pan:isPicked>"), "{packet}");
    assert!(packet.contains("<pan:description>the cat turns</pan:description>"), "{packet}");
    let _ = &img;
    assert!(packet.contains("<pan:duration>0.5</pan:duration>"), "{packet}");
    // One XMP box, not one per write.
    let xmp_boxes = after.windows(16).filter(|w| *w == pan::video::XMP_UUID).count();
    assert_eq!(xmp_boxes, 1, "{} bytes before, {} after", before.len(), after.len());

    // Unset takes it out of the file too.
    store.unset_fields(&put.id, &["rating".to_string()]).unwrap();
    let packet = pan::video::read_xmp(&std::fs::read(&abs).unwrap()).unwrap().unwrap();
    assert!(!packet.contains("<pan:rating>"), "{packet}");

    // Delete removes the file and the thumbnail.
    let tpath = store
        .node_field(&fact(&store, &put.iri, "thumbnail")[0], "path")
        .unwrap()
        .unwrap();
    store.delete(&put.id).unwrap();
    assert!(!abs.exists());
    assert!(!store.layout.abs(&tpath).exists());
    assert!(store.subject_for(&put.id).unwrap().is_none());
}

#[test]
fn a_video_that_arrives_with_xmp_keeps_it_and_its_facts_land() {
    let (_dir, store) = open();
    let img = store.put(&make_png(4, 4, 5), Some("image/png")).unwrap();
    // What a producer writes into a clip before delivery: the image it was
    // made from, and when it was made. (A producer's own pan: fields other
    // than these two are dropped, as for images.)
    let producer = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         <rdf:Description rdf:about=\"\" xmlns:pan=\"https://repolex.ai/ontology/pan/\">\n\
         <pan:relatedToId>&lt;pan/Image/{}&gt;</pan:relatedToId>\n\
         <pan:mediaCreatedDate>2026-11-08T02:47:00-08:00</pan:mediaCreatedDate>\n\
         </rdf:Description>\n\
         </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>",
        img.id
    );
    let with = pan::video::with_xmp(MP4, &producer).unwrap();
    let put = store.put(&with, Some("video/mp4")).unwrap();
    assert_eq!(put.statements, 2);
    assert_eq!(fact(&store, &put.iri, "relatedToId"), vec![img.iri.clone()], "an edge, not a string");
    assert_eq!(fact(&store, &put.iri, "mediaCreatedDate"), vec!["2026-11-08T02:47:00-08:00"]);
    let packet = pan::video::read_xmp(&std::fs::read(store.layout.abs(&put.media_path)).unwrap())
        .unwrap()
        .unwrap();
    assert!(packet.contains("2026-11-08T02:47:00-08:00"), "the producer's block stays: {packet}");
    assert!(packet.contains("<pan:duration>"), "Pan's block is beside it: {packet}");
    let xmp_boxes = std::fs::read(store.layout.abs(&put.media_path))
        .unwrap()
        .windows(16)
        .filter(|w| *w == pan::video::XMP_UUID)
        .count();
    assert_eq!(xmp_boxes, 1, "the producer's box was replaced by one carrying both blocks");
}

#[test]
fn what_is_not_a_video_is_refused_whole() {
    let (_dir, store) = open();
    let err = format!("{:#}", store.put(&make_png(4, 4, 1), Some("video/mp4")).unwrap_err());
    assert!(err.contains("not a readable video"), "{err}");
    let err = format!("{:#}", store.put(b"not a movie at all, just text", Some("video/quicktime")).unwrap_err());
    assert!(err.contains("not an MP4 or MOV"), "{err}");
    assert_eq!(store.counts().unwrap().images, 0);
    // A PNG is a PNG whatever the label says.
    let put = store.put(&make_png(4, 4, 1), Some("image/png")).unwrap();
    assert!(put.iri.contains("/Image/"));
}
