//! VideoSets (pan.ttl 0.4.26, goodlux 2026-11-12): the clips of one film in a
//! set of their own, the same shape as an ImageSet — own file
//! VideoSet/<id>.nq, membership as pan:relatedToId on the clip, videos only
//! — through every door: the videoset calls, `set relatedToId=`, and a file
//! that arrives naming the set.

use pan::Pan;

fn make_png(seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 8, 8);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        let px: Vec<u8> = (0..8 * 8 * 3).map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed)).collect();
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

fn related_to(store: &Pan, id: &str) -> Vec<String> {
    store
        .facts_for(id)
        .unwrap()
        .into_iter()
        .find(|(p, _)| p == &format!("{}relatedToId", pan::PAN_NS))
        .map(|(_, v)| v)
        .unwrap_or_default()
}

const CLIP: &[u8] = include_bytes!("fixtures/video/tiny.mp4");

#[test]
fn a_videoset_holds_clips_only_through_every_door_and_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    let clip_a = store.put(CLIP, Some("video/mp4")).unwrap().id;
    let clip_b = store.put(include_bytes!("fixtures/video/tiny.mov"), Some("video/quicktime")).unwrap().id;
    let still = store.put(&make_png(3), Some("image/png")).unwrap().id;

    let film = store.videoset_create(Some("prudhon-chalk")).unwrap();
    assert_eq!(film.class, "VideoSet");
    assert!(film.iri.contains("/VideoSet/"));
    let file = store.mediasets_root(&pan::imageset::VIDEOSET).join(format!("{}.nq", film.id));
    assert!(file.exists(), "{}", file.display());
    assert!(file.to_string_lossy().contains("/VideoSet/"));
    assert!(store.imageset_get(&film.id).unwrap().is_none(), "an ImageSet by that id does not exist");

    // Door 1: the videoset call. A clip goes in; a still is refused by name.
    store.videoset_add(&film.id, &clip_a).unwrap();
    let e = store.videoset_add(&film.id, &still).unwrap_err().to_string();
    assert!(e.contains("not a pan:Video") && e.contains("takes videos only") && e.contains("pan:ImageSet"), "{e}");
    // And an ImageSet refuses a clip, naming the VideoSet as the place.
    let album = store.imageset_create(Some("stills")).unwrap();
    let e = store.imageset_add(&album.id, &clip_a).unwrap_err().to_string();
    assert!(e.contains("not a pan:Image") && e.contains("pan:VideoSet"), "{e}");

    // Door 2: set relatedToId=, the set and the source still in one call.
    store
        .set_fields(
            &clip_b,
            &[
                ("relatedToId".into(), serde_json::json!([format!("<pan/VideoSet/{}>", film.id), format!("<pan/Image/{}>", pan::bare_id(&still))])),
                ("renderPrompt".into(), serde_json::json!("the chalk drifts")),
                ("renderRequestDate".into(), serde_json::json!("2026-11-12T10:00:00-08:00")),
            ],
        )
        .unwrap();
    let mut members = store.videoset_members(&film.id).unwrap();
    members.sort();
    assert_eq!(members.len(), 2, "{members:?}");
    assert!(related_to(&store, &clip_b).iter().any(|l| l.ends_with(&format!("/Image/{}", pan::bare_id(&still)))));
    let (clip_path, _) = store.media_file(&clip_b).unwrap();
    let bytes = std::fs::read(&clip_path).unwrap();
    let packet = pan::video::read_xmp(&bytes).unwrap().unwrap();
    assert!(packet.contains(&format!("<pan:relatedToId>&lt;pan/VideoSet/{}&gt;</pan:relatedToId>", film.id)), "{packet}");
    assert!(packet.contains("<pan:renderRequestDate>2026-11-12T10:00:00-08:00</pan:renderRequestDate>"), "{packet}");
    // A still may not be linked into a VideoSet through this door either.
    let e = store
        .set_fields(&still, &[("relatedToId".into(), serde_json::json!(format!("<pan/VideoSet/{}>", film.id)))])
        .unwrap_err()
        .to_string();
    assert!(e.contains("takes videos only"), "{e}");

    // Door 3: a clip arrives naming a VideoSet that does not exist: made.
    let named = "film-two";
    let xmp = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n<rdf:Description rdf:about=\"\" xmlns:pan=\"https://repolex.ai/ontology/pan/\">\n<pan:relatedToId>&lt;pan/VideoSet/{named}&gt;</pan:relatedToId>\n</rdf:Description>\n</rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>"
    );
    let clip_with = pan::video::with_xmp(CLIP, &xmp).unwrap();
    let clip_c = store.put(&clip_with, Some("video/mp4")).unwrap();
    assert_eq!(clip_c.videosets_made, vec![named.to_string()]);
    assert!(store.videoset_get(named).unwrap().is_some());
    assert_eq!(store.videoset_members(named).unwrap().len(), 1);
    // An IMAGE arriving and naming a VideoSet is refused whole, nothing made.
    let png_with = pan::xmp::write_packet_into_png_bytes(&make_png(9), &xmp.replace(named, "film-three")).unwrap();
    let e = store.put(&png_with, Some("image/png")).unwrap_err().to_string();
    assert!(e.contains("takes videos only") && e.contains("Nothing was stored"), "{e}");
    assert!(store.videoset_get("film-three").unwrap().is_none());

    // The lists are per kind.
    assert_eq!(store.videoset_list().unwrap().len(), 2);
    assert_eq!(store.imageset_list().unwrap().len(), 1);

    // Reopen: both kinds come back from their files.
    drop(store);
    let store = open_at(dir.path());
    assert_eq!(store.videoset_list().unwrap().len(), 2);
    assert_eq!(store.videoset_members(&film.id).unwrap().len(), 2);
    assert_eq!(store.imageset_list().unwrap().len(), 1);

    // Delete the film: clips unlinked, the still link on clip_b stays.
    let gone = store.videoset_delete(&film.id).unwrap();
    assert_eq!(gone.len(), 2);
    assert!(store.videoset_get(&film.id).unwrap().is_none());
    let b_links = related_to(&store, &clip_b);
    assert_eq!(b_links.len(), 1, "{b_links:?}");
    assert!(b_links[0].contains("/Image/"));
}
