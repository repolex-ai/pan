//! pan:renderPrompt (pan.ttl 0.4.24): the prompt that asked for the media,
//! settable on an image or a video like every Media field, and written into
//! the file. pan:renderRequestInformation is a different thing and stays
//! what the file arrived with.

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

#[test]
fn render_prompt_is_declared_settable_on_an_image_and_a_video_and_reaches_the_file() {
    assert!(pan::settable_fields().iter().any(|f| f.local == "renderPrompt"));
    let (_dir, store) = open();
    let prompt = "A 50mm medium shot of Kira celebrating a quiet summit moment with a cup of coffee";

    let img = store.put(&make_png(4, 4, 9), Some("image/png")).unwrap();
    store
        .set_fields(&img.id, &[("renderPrompt".to_string(), serde_json::json!(prompt))])
        .unwrap();
    let facts: std::collections::HashMap<String, Vec<String>> =
        store.facts_for(&img.id).unwrap().into_iter().collect();
    assert_eq!(facts["https://repolex.ai/ontology/pan/renderPrompt"], vec![prompt.to_string()]);
    assert!(!facts.contains_key("https://repolex.ai/ontology/pan/renderRequestInformation"));
    let bytes = std::fs::read(store.layout.abs(&img.media_path)).unwrap();
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes).unwrap().unwrap();
    assert!(packet.contains(&format!("<pan:renderPrompt>{prompt}</pan:renderPrompt>")), "{packet}");

    let clip = store
        .put(include_bytes!("fixtures/video/tiny.mp4"), Some("video/mp4"))
        .unwrap();
    store
        .set_fields(&clip.id, &[("renderPrompt".to_string(), serde_json::json!("the cat turns"))])
        .unwrap();
    let packet = pan::video::read_xmp(&std::fs::read(store.layout.abs(&clip.media_path)).unwrap())
        .unwrap()
        .unwrap();
    assert!(packet.contains("<pan:renderPrompt>the cat turns</pan:renderPrompt>"), "{packet}");

    // One value: setting again overwrites.
    store
        .set_fields(&img.id, &[("renderPrompt".to_string(), serde_json::json!("second"))])
        .unwrap();
    let facts: std::collections::HashMap<String, Vec<String>> =
        store.facts_for(&img.id).unwrap().into_iter().collect();
    assert_eq!(facts["https://repolex.ai/ontology/pan/renderPrompt"], vec!["second".to_string()]);
}
