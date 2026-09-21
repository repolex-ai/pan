//! pan issue #50: the caption stage wrote twenty-three properties onto the
//! image and only seventeen reached the file. The three image scores, their
//! critiques, the prompt path and the render information were all in the
//! graph and absent from the XMP, because the packet builder enumerated the
//! scene fields alone.
//!
//! This test is the gate that was missing: set every field the rosters name,
//! then read the bytes back and fail on any one that did not survive. A field
//! added to a roster with no path into the file fails here.

use pan::{Pan, Perception, PERCEPTION_FIELDS, RENDER_REQUEST_CHUNK, STRUCTURAL_FIELDS};

/// A tiny PNG carrying a render chunk, the way a diffusion user interface
/// hands one over.
fn png_with_render_chunk(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 4, 4);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.add_text_chunk(RENDER_REQUEST_CHUNK.into(), text.into())
            .unwrap();
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[7u8; 4 * 4 * 3]).unwrap();
        w.finish().unwrap();
    }
    out
}

#[test]
fn every_roster_field_reaches_the_image_xmp() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pan.yml"), "storage_id: xmpfield\n").unwrap();
    let store = Pan::open(dir.path()).unwrap();

    let render_text = "SUBJECT: a wolf.\nSteps: 12, Sampler: Euler";
    let put = store
        .put(&png_with_render_chunk(render_text), Some("image/png"))
        .unwrap();
    let id = put.id;

    // Everything the caption stage returns, including all six judgement
    // values, with a value that is findable in the bytes.
    let scene: Vec<(String, String)> = PERCEPTION_FIELDS
        .iter()
        .filter(|l| !matches!(**l, "shortCaption" | "longCaption" | "sceneObjects"))
        .filter(|l| **l != "modelPromptPath")
        .map(|l| (l.to_string(), format!("value-of-{l}")))
        .collect();
    store
        .set_perception(
            &id,
            &Perception {
                short_caption: "short-caption-here".into(),
                long_caption: "long-caption-here".into(),
                scene_objects: vec!["wolf".into(), "ridge".into()],
                scene,
                prompt_path: "full-caption.default.md".into(),
            },
        )
        .unwrap();
    store.mark_enrichment_complete(&id).unwrap();

    let facts = store.facts_for(&id).unwrap();
    let in_graph = |local: &str| {
        facts
            .iter()
            .any(|(p, v)| p.ends_with(&format!("/{local}")) && !v.is_empty())
    };

    let media = store.layout.abs(
        facts
            .iter()
            .find(|(p, _)| p.ends_with("/mediaPath"))
            .and_then(|(_, v)| v.first())
            .expect("mediaPath"),
    );
    let bytes = std::fs::read(&media).unwrap();
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes)
        .unwrap()
        .expect("the file carries an XMP packet");

    // A property the graph holds must be written into the file. The reverse
    // is not asserted: a field with no value is simply absent from both.
    let mut missing = Vec::new();
    for local in PERCEPTION_FIELDS.iter().chain(STRUCTURAL_FIELDS.iter()) {
        if in_graph(local) && !packet.contains(&format!("<pan:{local}>")) {
            missing.push(*local);
        }
    }
    assert!(
        missing.is_empty(),
        "in the graph and missing from the image XMP: {missing:?}\n{packet}"
    );

    // The six that issue #50 was raised for, named so a regression says so.
    for local in [
        "imageTechnicalScore",
        "imageTechnicalCritique",
        "imageAestheticScore",
        "imageAestheticCritique",
        "imageAnatomyScore",
        "imageAnatomyCritique",
    ] {
        assert!(
            packet.contains(&format!("<pan:{local}>value-of-{local}</pan:{local}>")),
            "{local} is not in the file: {packet}"
        );
    }
    assert!(
        packet.contains("<pan:modelPromptPath>full-caption.default.md</pan:modelPromptPath>"),
        "the prompt path is not in the file: {packet}"
    );
    assert!(
        packet.contains("SUBJECT: a wolf."),
        "the render information is not in the file: {packet}"
    );
}
