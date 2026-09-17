//! The embed stage's write path (issue #31): one run lands the `.npy`, the
//! server's answer as `.json`, and — like every other stage — an XML data
//! file holding the vectorData reference and the Embedding record, so the
//! record is rebuildable from disk. The record's precision and provider are
//! the values the SERVER reported; `pan:model` stays the index name pand
//! embeds with (goodlux, 2026-09-05: one index for Mac and Salad vectors).

use pan::Pan;

const FIXTURE: &str = include_str!("fixtures/embed/answer.json");
const INDEX: &str = "qwen3-vl-embedding-2b";

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

/// Split a server answer the way the daemon does: the vector, and
/// everything else as `details`.
fn split(answer: &str) -> (Vec<f32>, serde_json::Map<String, serde_json::Value>) {
    let mut v: serde_json::Value = serde_json::from_str(answer).unwrap();
    let obj = v.as_object_mut().unwrap();
    let vector: Vec<f32> = serde_json::from_value(obj.remove("vector").unwrap()).unwrap();
    obj.remove("dim");
    (vector, obj.clone())
}

fn rows(store: &Pan, q: &str, vars: &[&str]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    if let pan::QueryResults::Solutions(sols) = store.query(q).unwrap() {
        for s in sols {
            let s = s.unwrap();
            out.push(
                vars.iter()
                    .map(|v| match s.get(*v) {
                        Some(pan::Term::Literal(l)) => l.value().to_string(),
                        Some(pan::Term::NamedNode(n)) => n.as_str().to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect(),
            );
        }
    }
    out
}

#[test]
fn one_embed_run_lands_npy_json_record_file_graph_and_xmp() {
    let (_dir, store) = open();
    let put = store.put(&make_png(4, 4, 1), Some("image/png")).unwrap();
    let id = put.id.clone();
    let (vector, details) = split(FIXTURE);

    let rel = store
        .write_embedding(&id, INDEX, INDEX, &vector, &details)
        .unwrap();
    assert_eq!(
        rel,
        format!("image/data/vectors/{INDEX}/{id}.xml"),
        "record file beside the vector"
    );
    let record_abs = store.layout.abs(&rel);
    assert!(record_abs.exists(), "record file written");
    assert!(record_abs.with_extension("npy").exists(), ".npy beside it");
    let side: serde_json::Value =
        serde_json::from_slice(&std::fs::read(record_abs.with_extension("json")).unwrap()).unwrap();
    assert_eq!(
        side["model"], "Qwen/Qwen3-VL-Embedding-2B",
        "the server's own model id is kept verbatim in the .json"
    );
    assert_eq!(side["provider"], "salad");

    // The data file has the declared shape: reference → pan:item → record.
    let text = std::fs::read_to_string(&record_abs).unwrap();
    assert!(text.contains("<pan:item rdf:resource="), "{text}");
    assert!(
        text.contains(&format!("rdf:resource=\"{}Embedding\"", pan::PAN_NS)),
        "record typed pan:Embedding: {text}"
    );
    assert!(
        text.contains("<pan:precision>bf16-cuda</pan:precision>"),
        "{text}"
    );
    assert!(
        text.contains("<pan:provider>salad</pan:provider>"),
        "{text}"
    );
    assert!(
        text.contains(&format!("<pan:model>{INDEX}</pan:model>")),
        "{text}"
    );
    assert!(text.contains("<pan:dim>8</pan:dim>"), "{text}");
    assert!(
        text.contains(&format!(
            "<pan:vectorPath>image/data/vectors/{INDEX}/{id}.npy</pan:vectorPath>"
        )),
        "{text}"
    );
    assert!(text.contains("<pan:producedDate>"), "{text}");
    assert!(
        !text.contains("git-lex"),
        "the file speaks pan: only: {text}"
    );

    // Graph: image → reference (pan:path = the record file) → item → record,
    // with the server-reported precision and provider and the index name as model.
    let q = format!(
        "SELECT ?path ?rec ?model ?dim ?prec ?prov ?vp ?pd WHERE {{ <{}> pan:vectorData ?d . ?d pan:path ?path ; pan:item ?rec . ?rec a pan:Embedding ; pan:model ?model ; pan:dim ?dim ; pan:precision ?prec ; pan:provider ?prov ; pan:vectorPath ?vp ; pan:producedDate ?pd . }}",
        put.iri
    );
    let got = rows(
        &store,
        &q,
        &["path", "rec", "model", "dim", "prec", "prov", "vp", "pd"],
    );
    assert_eq!(
        got.len(),
        1,
        "one Embedding record reachable through the reference: {got:?}"
    );
    let r = &got[0];
    assert_eq!(r[0], rel, "reference path names the record file");
    assert!(
        r[1].starts_with(&format!("{}Embedding/", pan::PAN_MEDIA_NS)),
        "record IRI: {}",
        r[1]
    );
    assert_eq!(r[2], INDEX);
    assert_eq!(r[3], "8");
    assert_eq!(
        r[4],
        details["precision"].as_str().unwrap(),
        "precision is what the server reported"
    );
    assert_eq!(
        r[5],
        details["provider"].as_str().unwrap(),
        "provider is what the server reported"
    );
    assert_eq!(r[6], format!("image/data/vectors/{INDEX}/{id}.npy"));
    assert!(!r[7].is_empty());

    // The image's XMP carries the reference with the same path.
    let bytes = std::fs::read(store.layout.abs(&put.media_path)).unwrap();
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes)
        .unwrap()
        .expect("stored image carries XMP");
    assert!(packet.contains("vectorData"), "{packet}");
    assert!(packet.contains(&rel), "reference path in the XMP: {packet}");

    // No longer pending for this index.
    let pending = store.pending_for("vectorData", INDEX, 10, None).unwrap();
    assert!(
        !pending.iter().any(|p| p.id == id),
        "image is no longer pending for embed"
    );
}

#[test]
fn record_file_id_survives_a_reread_of_the_disk() {
    // The whole point of #31: the record's id is on disk, not only in memory.
    let (_dir, store) = open();
    let put = store.put(&make_png(4, 4, 2), Some("image/png")).unwrap();
    let (vector, details) = split(FIXTURE);
    let rel = store
        .write_embedding(&put.id, INDEX, INDEX, &vector, &details)
        .unwrap();
    let text = std::fs::read_to_string(store.layout.abs(&rel)).unwrap();
    let q = format!(
        "SELECT ?rec WHERE {{ <{}> pan:vectorData ?d . ?d pan:item ?rec . }}",
        put.iri
    );
    let rec = rows(&store, &q, &["rec"])[0][0].clone();
    let bracket = pan::xmp::bracket_of_iri(&rec);
    assert!(
        text.contains(&format!(
            "<pan:id>{}</pan:id>",
            bracket.replace('<', "&lt;").replace('>', "&gt;")
        )),
        "record id {bracket} is in the file: {text}"
    );
}
