//! The face stage's write path (pan.ttl 0.4.21): one run lands the node's
//! answer and one .npy per face under image/enrichment/face/, one Face record
//! per face hung off a reference on the image, each vector in the face index
//! under the FACE's id, and the image stops being pending for that model.
//! Forgetting or deleting takes the vectors out of the index again.

use pan::face::{FaceAnswer, CLASS, F_HEAD_POSE, F_LANDMARKS, REF_LOCAL};
use pan::Pan;

const FIXTURE: &str = include_str!("fixtures/face/answer.json");
const MODEL: &str = "buffalo-l";

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

/// The fixture's answer as JSON, with its one face repeated `n` times; the
/// second copy gets a different vector (the first one reversed, still unit
/// length) so the two faces are distinguishable in the index.
fn answer_with_faces(n: usize) -> (FaceAnswer, serde_json::Value) {
    let mut v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let face = v["faces"][0].clone();
    let mut faces = Vec::new();
    for i in 0..n {
        let mut f = face.clone();
        if i % 2 == 1 {
            let mut emb: Vec<serde_json::Value> = f["embedding"].as_array().unwrap().clone();
            emb.reverse();
            f["embedding"] = serde_json::Value::Array(emb);
        }
        faces.push(f);
    }
    v["faces"] = serde_json::Value::Array(faces);
    (serde_json::from_value(v.clone()).unwrap(), v)
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

#[test]
fn one_face_run_lands_answer_vectors_records_graph_xmp_and_index() {
    let (_dir, store) = open();
    let put = store.put(&make_png(4, 4, 1), Some("image/png")).unwrap();
    let id = put.id.clone();

    let pending = store.pending_for(REF_LOCAL, MODEL, 10, None).unwrap();
    assert!(pending.iter().any(|p| p.id == id), "pending for face before the run");

    let (answer, raw) = answer_with_faces(2);
    let rel = store.write_face(&id, MODEL, &answer, &raw).unwrap();
    let stem = std::path::Path::new(&put.media_path)
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let shard = &put.created_date[0..10].replace('-', "/");
    assert_eq!(rel, format!("image/enrichment/face/{shard}/{stem}.face.{MODEL}.nq"));
    assert!(store.layout.abs(&rel).exists(), "record file written");

    // The node's whole answer beside the record, named on the reference.
    let reply_rel = rel.replace(".nq", ".json");
    let reply: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.layout.abs(&reply_rel)).unwrap()).unwrap();
    assert_eq!(reply["faces"].as_array().unwrap().len(), 2);
    assert_eq!(reply["faces"][0]["age"], 29, "the guesses stay in the reply file");
    let named = rows(
        &store,
        &format!("SELECT ?r WHERE {{ <{}> pan:{REF_LOCAL} ?d . ?d pan:modelReplyPath ?r }}", put.iri),
        &["r"],
    );
    assert_eq!(named, vec![vec![reply_rel]]);

    // Graph: image → reference → item → Face, one per face, every field.
    let q = format!(
        "SELECT ?rec ?bbox ?score ?lm ?hp ?vp ?dim ?model ?prec ?prov WHERE {{ <{}> pan:{REF_LOCAL} ?d . ?d pan:item ?rec . ?rec a pan:{CLASS} ; pan:bbox ?bbox ; pan:score ?score ; pan:{F_LANDMARKS} ?lm ; pan:{F_HEAD_POSE} ?hp ; pan:vectorPath ?vp ; pan:dim ?dim ; pan:model ?model ; pan:precision ?prec ; pan:provider ?prov . }} ORDER BY ?rec",
        put.iri
    );
    let recs = rows(
        &store,
        &q,
        &["rec", "bbox", "score", "lm", "hp", "vp", "dim", "model", "prec", "prov"],
    );
    assert_eq!(recs.len(), 2, "two Face records: {recs:?}");
    for r in &recs {
        assert_eq!(r[1], "323.5,159.3,711.7,620.8");
        assert_eq!(r[2], "0.8149");
        assert_eq!(r[3], "476.5,335.5;647.5,364.1;579.3,446.2;442.2,472.7;624,497.3");
        assert_eq!(r[4], "-1.08,18.82,11.37");
        assert_eq!(r[6], "512");
        assert_eq!(r[7], MODEL);
        assert_eq!(r[8], "fp32-onnx-cpu");
        assert_eq!(r[9], "katoptron");
        // The vector file is named by the face, beside the record, and holds
        // the vector that is in the index under the face's id.
        let face_id = r[0].rsplit('/').next().unwrap().to_string();
        assert_eq!(r[5], format!("image/enrichment/face/{shard}/{stem}.face.{MODEL}.{face_id}.npy"));
        let on_disk = pan::npy::read_f32_1d(&store.layout.abs(&r[5])).unwrap();
        assert_eq!(on_disk.len(), 512);
        assert!(store.contains_id(&face_id, MODEL), "face {face_id} in index {MODEL}");
        assert_eq!(store.stored_vector(&face_id, MODEL).unwrap().unwrap(), on_disk);
        assert_eq!(
            store.image_of_face(&face_id).unwrap().map(|(img, _)| img),
            Some(put.iri.clone())
        );
    }
    // The two vectors differ: the second face's is the first one reversed.
    let a = store.stored_vector(recs[0][0].rsplit('/').next().unwrap(), MODEL).unwrap().unwrap();
    let b = store.stored_vector(recs[1][0].rsplit('/').next().unwrap(), MODEL).unwrap().unwrap();
    assert_ne!(a, b);
    // The image is never keyed in the face index.
    assert!(!store.contains_id(&id, MODEL));

    // The XMP in the file names the reference too.
    let bytes = std::fs::read(store.layout.abs(&put.media_path)).unwrap();
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes).unwrap().unwrap();
    assert!(packet.contains("faceData"), "image XMP lists the face reference: {packet}");

    // Done for this model.
    let pending = store.pending_for(REF_LOCAL, MODEL, 10, None).unwrap();
    assert!(!pending.iter().any(|p| p.id == id), "no longer pending after the run");
    // The store count is images with at least one face, like every count.
    assert_eq!(store.counts().unwrap().faces, 1);
}

#[test]
fn no_faces_is_a_finding_the_image_is_done_and_nothing_enters_the_index() {
    let (_dir, store) = open();
    let put = store.put(&make_png(4, 4, 2), Some("image/png")).unwrap();
    let (answer, raw) = answer_with_faces(0);
    let rel = store.write_face(&put.id, MODEL, &answer, &raw).unwrap();
    assert!(store.layout.abs(&rel).exists());
    let items = rows(
        &store,
        &format!("SELECT ?rec WHERE {{ <{}> pan:{REF_LOCAL} ?d . ?d pan:item ?rec }}", put.iri),
        &["rec"],
    );
    assert!(items.is_empty(), "a reference with no records");
    let pending = store.pending_for(REF_LOCAL, MODEL, 10, None).unwrap();
    assert!(!pending.iter().any(|p| p.id == put.id));
    assert!(store.index_stats().iter().all(|(name, _)| name != MODEL), "no face index was made");
}

#[test]
fn an_answer_without_a_faces_list_is_refused_and_the_image_stays_pending() {
    let (_dir, store) = open();
    let put = store.put(&make_png(4, 4, 3), Some("image/png")).unwrap();
    let answer: FaceAnswer = serde_json::from_str("{}").unwrap();
    let err = store
        .write_face(&put.id, MODEL, &answer, &serde_json::json!({}))
        .unwrap_err();
    assert!(err.to_string().contains("no faces list"), "{err}");
    let pending = store.pending_for(REF_LOCAL, MODEL, 10, None).unwrap();
    assert!(pending.iter().any(|p| p.id == put.id));
}

#[test]
fn similar_faces_come_back_across_images_and_leave_the_index_on_forget_and_delete() {
    let (_dir, store) = open();
    // Three images: A with one face, B with the same face again (a clone), C
    // with two faces, the second of which has the reversed vector.
    let a = store.put(&make_png(4, 4, 11), Some("image/png")).unwrap();
    let b = store.put(&make_png(4, 4, 12), Some("image/png")).unwrap();
    let c = store.put(&make_png(4, 4, 13), Some("image/png")).unwrap();
    for (img, n) in [(&a, 1), (&b, 1), (&c, 2)] {
        let (answer, raw) = answer_with_faces(n);
        store.write_face(&img.id, MODEL, &answer, &raw).unwrap();
    }
    let face_of = |img: &pan::PutResult| -> Vec<String> {
        rows(
            &store,
            &format!("SELECT ?rec WHERE {{ <{}> pan:{REF_LOCAL} ?d . ?d pan:item ?rec }} ORDER BY ?rec", img.iri),
            &["rec"],
        )
        .into_iter()
        .map(|r| r[0].rsplit('/').next().unwrap().to_string())
        .collect()
    };
    let fa = face_of(&a)[0].clone();
    let fb = face_of(&b)[0].clone();
    let fc = face_of(&c);
    assert_eq!(store.index_stats().iter().find(|(n, _)| n == MODEL).map(|(_, s)| s.count), Some(4));

    // Nearest to A's face, A's own face left out: B's clone first at ~1,
    // then C's identical copy at ~1, then C's reversed one lower.
    let v = store.stored_vector(&fa, MODEL).unwrap().unwrap();
    let hits = store.search("", &v, 10, MODEL, Some(&fa)).unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(hits.len(), 3, "{ids:?}");
    assert!(!ids.contains(&fa.as_str()), "the asked-about face is not its own hit");
    assert!(hits[0].score > 0.999 && hits[1].score > 0.999, "{hits:?}");
    assert!(ids[..2].contains(&fb.as_str()), "{ids:?}");
    assert!(hits[2].score < 0.999, "the reversed vector is farther: {hits:?}");
    // Each hit resolves to its image.
    assert_eq!(store.image_of_face(&fb).unwrap().unwrap().0, b.iri);
    assert_eq!(store.image_of_face(&fc[1]).unwrap().unwrap().0, c.iri);

    // Forgetting the stage on C removes both of C's faces from the index and
    // their files, and C is pending again.
    let report = store.forget_enrichment(&c.id, REF_LOCAL).unwrap();
    assert!(report.files_removed >= 4, "record, reply and two vectors: {report:?}");
    for f in &fc {
        assert!(!store.contains_id(f, MODEL), "face {f} left the index");
        assert!(store.image_of_face(f).unwrap().is_none());
    }
    assert!(store.pending_for(REF_LOCAL, MODEL, 10, None).unwrap().iter().any(|p| p.id == c.id));
    assert_eq!(store.index_stats().iter().find(|(n, _)| n == MODEL).map(|(_, s)| s.count), Some(2));

    // Deleting B takes its face out too.
    store.delete(&b.id).unwrap();
    assert!(!store.contains_id(&fb, MODEL));
    assert_eq!(store.index_stats().iter().find(|(n, _)| n == MODEL).map(|(_, s)| s.count), Some(1));
    let hits = store.search("", &v, 10, MODEL, Some(&fa)).unwrap();
    assert!(hits.is_empty(), "nothing else left near A: {hits:?}");
}

#[test]
fn the_face_vocabulary_is_declared_in_the_ontology() {
    for decl in [
        "\npan:Face a owl:Class",
        "\npan:faceData a owl:ObjectProperty",
        "\npan:landmarks a owl:DatatypeProperty",
        "\npan:headPose a owl:DatatypeProperty",
        "\npan:Face rdfs:subClassOf",
    ] {
        assert!(pan::PAN_ONTOLOGY_TTL.contains(decl), "missing in pan.ttl: {decl}");
    }
    // bbox, score, vectorPath and dim are shared now: declared once, on Node.
    for shared in ["bbox", "score", "vectorPath", "dim"] {
        let decl = format!("\npan:{shared} a owl:DatatypeProperty");
        let at = pan::PAN_ONTOLOGY_TTL.find(&decl).unwrap();
        let block = &pan::PAN_ONTOLOGY_TTL[at..at + 400];
        assert!(block.contains("rdfs:domain pan:Node"), "pan:{shared} is not on pan:Node:\n{block}");
    }
}

/// 2026-11-10, goodlux's lUX store: a SIGTERM during a save cut the face
/// index short, and every face write failed from then on with the index
/// library's own words. The save is now two files written beside their
/// targets and renamed into place, so a kill leaves the pair whole; a
/// damaged file is reported with its path and the route that rebuilds it;
/// and the rebuild puts every vector the graph names back from the `.npy`
/// files, leaving out the ones whose file is gone and saying which.
#[test]
fn a_torn_face_index_is_reported_with_its_path_and_rebuilt_from_the_vectors_on_disk() {
    let (dir, store) = open();
    let a = store.put(&make_png(4, 4, 21), Some("image/png")).unwrap();
    let c = store.put(&make_png(4, 4, 23), Some("image/png")).unwrap();
    for (img, n) in [(&a, 1), (&c, 2)] {
        let (answer, raw) = answer_with_faces(n);
        store.write_face(&img.id, MODEL, &answer, &raw).unwrap();
    }
    let face_of = |store: &Pan, img: &pan::PutResult| -> Vec<String> {
        rows(
            store,
            &format!("SELECT ?rec WHERE {{ <{}> pan:{REF_LOCAL} ?d . ?d pan:item ?rec }} ORDER BY ?rec", img.iri),
            &["rec"],
        )
        .into_iter()
        .map(|r| r[0].rsplit('/').next().unwrap().to_string())
        .collect()
    };
    let fa = face_of(&store, &a)[0].clone();
    let va = store.stored_vector(&fa, MODEL).unwrap().unwrap();
    // C's two faces by what they carry, not by id order: the same vector as
    // A's face, and the reversed one.
    let fc = face_of(&store, &c);
    let (same, reversed) = if store.stored_vector(&fc[0], MODEL).unwrap().unwrap() == va {
        (fc[0].clone(), fc[1].clone())
    } else {
        (fc[1].clone(), fc[0].clone())
    };
    let index_dir = store.layout.hnsw_root.join(MODEL);
    // Nothing is on disk until a save; the daemon saves every FLUSH_INTERVAL.
    assert!(!index_dir.join("index.usearch").exists(), "no save per write");
    store.flush().unwrap();
    // A finished save leaves no half-written file behind.
    assert!(index_dir.join("index.usearch").exists());
    assert!(index_dir.join("keymap.json").exists());
    assert!(!index_dir.join(".index.usearch.partial").exists());
    assert!(!index_dir.join(".keymap.json.partial").exists());
    // The second of C's vector files goes missing, as a disk might lose it.
    let gone_rel = rows(
        &store,
        &format!("SELECT ?vp WHERE {{ <{}{}/{}> pan:vectorPath ?vp }}", pan::config::PAN_MEDIA_NS, CLASS, reversed),
        &["vp"],
    )[0][0]
        .clone();
    drop(store);

    // Cut the index file short, as the kill did, and leave a stale partial
    // from a save that never finished.
    let idx = index_dir.join("index.usearch");
    let len = std::fs::metadata(&idx).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&idx)
        .unwrap()
        .set_len(len / 2)
        .unwrap();
    std::fs::write(index_dir.join(".index.usearch.partial"), b"half").unwrap();

    let store = Pan::open(dir.path()).unwrap();
    std::fs::remove_file(store.layout.abs(&gone_rel)).unwrap();
    let b = store.put(&make_png(4, 4, 22), Some("image/png")).unwrap();
    let (answer, raw) = answer_with_faces(1);
    let err = format!("{:#}", store.write_face(&b.id, MODEL, &answer, &raw).unwrap_err());
    assert!(err.contains(idx.to_str().unwrap()), "names the file: {err}");
    assert!(err.contains("cut short"), "says what happened: {err}");
    assert!(err.contains(&format!("indexes/{MODEL}/rebuild")), "names the route: {err}");
    assert!(!index_dir.join(".index.usearch.partial").exists(), "the stale partial is gone");
    let err = format!("{:#}", store.search("", &va, 5, MODEL, None).unwrap_err());
    assert!(err.contains("cannot be read"), "search says the same: {err}");

    let r = store.rebuild_index(MODEL).unwrap();
    assert_eq!((r.index.as_str(), r.dim), (MODEL, 512));
    assert_eq!(r.vectors, 2, "A's face and C's matching one: {r:?}");
    assert_eq!(r.missing, 1, "{r:?}");
    assert_eq!(r.missing_sample, vec![gone_rel.clone()]);
    assert_eq!(store.stored_vector(&fa, MODEL).unwrap().unwrap(), va, "A's vector is back from its .npy");
    assert!(store.contains_id(&same, MODEL));
    assert!(!store.contains_id(&reversed, MODEL), "the face without a file stays out");
    let hits = store.search("", &va, 5, MODEL, Some(&fa)).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].id, same);
    assert!(hits[0].score > 0.999);
    // Writes work again, and the rebuilt pair is what a fresh open loads.
    store.write_face(&b.id, MODEL, &answer, &raw).unwrap();
    assert_eq!(store.index_stats().iter().find(|(n, _)| n == MODEL).map(|(_, s)| s.count), Some(3));
    drop(store);
    let store = Pan::open(dir.path()).unwrap();
    assert_eq!(store.index_stats().iter().find(|(n, _)| n == MODEL).map(|(_, s)| s.count), Some(3));
    assert_eq!(store.search("", &va, 5, MODEL, Some(&fa)).unwrap().len(), 2);

    // An index the graph knows nothing about is refused, not written empty.
    let err = format!("{:#}", store.rebuild_index("no-such-model").unwrap_err());
    assert!(err.contains("names no vector"), "{err}");
}
