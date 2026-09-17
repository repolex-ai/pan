//! The depth stage's write path (issue #24): one run lands the map PNG and
//! the node's sidecar under image/data/depth/, one Depth record hung off a
//! reference on the image, in the graph AND the image's XMP, and the image
//! stops being pending for that model.

use pan::depth::{DepthAnswer, CLASS, F_MAP_PATH, F_MAX, F_MIN, REF_LOCAL};
use pan::Pan;

const FIXTURE: &str = include_str!("fixtures/depth/answer.json");

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

fn open() -> (tempfile::TempDir, Pan) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pan.yml"), "storage_id: test-store\nindex_id: test-idx\n").unwrap();
    let store = Pan::open(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn one_depth_run_lands_map_sidecar_record_graph_and_xmp() {
    let (_dir, store) = open();
    let put = store.put(&make_png(4, 4, 1), Some("image/png")).unwrap();
    let id = put.id.clone();
    let model = "depth-anything/Depth-Anything-V2-Base-hf";

    // Pending before: the graph is the queue.
    let pending = store.pending_for(REF_LOCAL, model, 10, None).unwrap();
    assert!(pending.iter().any(|p| p.id == id), "image is pending for depth before the run");

    let answer: DepthAnswer = serde_json::from_str(FIXTURE).unwrap();
    let rel = store.write_depth(&id, model, &answer).unwrap();
    assert!(rel.starts_with("image/data/depth/"), "record file under data/depth: {rel}");
    assert!(rel.ends_with(&format!("{id}.xml")), "{rel}");
    assert!(store.layout.abs(&rel).exists(), "record file written");

    // The map and the sidecar sit beside the record, model id flattened.
    let map_rel = rel.replace(&format!("{id}.xml"), &format!("{id}.depth-anything-Depth-Anything-V2-Base-hf.png"));
    let map_abs = store.layout.abs(&map_rel);
    assert!(map_abs.exists(), "map PNG at {map_rel}");
    assert!(std::fs::read(&map_abs).unwrap().starts_with(b"\x89PNG"));
    let side: serde_json::Value = serde_json::from_slice(&std::fs::read(map_abs.with_extension("json")).unwrap()).unwrap();
    assert_eq!(side["provider"], "salad");
    assert_eq!(side["depth_png"], map_rel);
    assert!(side.get("depth_png_b64").is_none(), "the bytes live in the PNG, not the sidecar");

    // Graph: image → reference → item → Depth record with its fields.
    let facts = store.facts_for(&id).unwrap();
    let refs = facts.iter().find(|(p, _)| p == &format!("{}{REF_LOCAL}", pan::PAN_NS)).map(|(_, v)| v.clone()).unwrap_or_default();
    assert_eq!(refs.len(), 1, "one depth reference on the image: {facts:?}");
    let q = format!(
        "SELECT ?rec ?path ?min ?max ?w ?h WHERE {{ <{}> pan:{REF_LOCAL} ?d . ?d pan:item ?rec . ?rec a pan:{CLASS} ; pan:{F_MAP_PATH} ?path ; pan:{F_MIN} ?min ; pan:{F_MAX} ?max ; pan:width ?w ; pan:height ?h . }}",
        put.iri
    );
    let mut rows: Vec<Vec<String>> = Vec::new();
    if let pan::QueryResults::Solutions(sols) = store.query(&q).unwrap() {
        for s in sols {
            let s = s.unwrap();
            let lit = |v: &str| match s.get(v) {
                Some(pan::Term::Literal(l)) => l.value().to_string(),
                other => format!("{other:?}"),
            };
            rows.push(vec![lit("path"), lit("min"), lit("max"), lit("w"), lit("h")]);
        }
    }
    assert_eq!(rows.len(), 1, "one Depth record reachable through the reference: {rows:?}");
    assert_eq!(rows[0], vec![map_rel.clone(), "-1.2637".into(), "9.3984".into(), "4".into(), "4".into()]);

    // XMP: the image's own packet lists the reference bag.
    let (bytes, _) = store.get(&id).unwrap();
    let xmp = pan::xmp::read_xmp_packet_from_bytes(&bytes).unwrap().expect("stored image carries XMP");
    assert!(xmp.contains(&format!("<pan:{REF_LOCAL}>")), "{xmp}");
    assert!(xmp.contains(&format!("<pan:path>{rel}</pan:path>")), "{xmp}");
    assert!(!xmp.contains("<pan:count>"), "a depth reference carries no count: {xmp}");

    // Not pending any more.
    let pending = store.pending_for(REF_LOCAL, model, 10, None).unwrap();
    assert!(!pending.iter().any(|p| p.id == id), "image retired from the depth queue");

    // State reports the model under the depth reference.
    let st = store.state_for(&id).unwrap().unwrap();
    assert!(st.enrichment.iter().any(|(l, ms)| l == REF_LOCAL && ms.iter().any(|m| m == model)), "{:?}", st.enrichment);
}

#[test]
fn an_answer_without_a_map_is_refused_and_the_image_stays_pending() {
    let (_dir, store) = open();
    let put = store.put(&make_png(4, 4, 2), Some("image/png")).unwrap();
    let answer: DepthAnswer = serde_json::from_str("{}").unwrap();
    assert!(store.write_depth(&put.id, "m", &answer).is_err());
    let pending = store.pending_for(REF_LOCAL, "m", 10, None).unwrap();
    assert!(pending.iter().any(|p| p.id == put.id));
    assert!(!store.layout.media_root.join("image/data/depth").exists(), "nothing written");
}
