//! Every fact Pan writes is spelled pan: — in the image file AND in the
//! graph (goodlux, 2026-09-17). The graph carries pan:id and pan:createdDate
//! on the Image, and pand puts NO predicate under the git-lex or subtexture
//! namespace anywhere in the store.

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

#[test]
fn graph_spells_identity_and_creation_pan_and_nothing_git_lex_or_subtexture() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let put = store.put(&make_png(8, 8, 3), Some("image/png")).unwrap();
    let set = store.imageset_create(Some("spelling")).unwrap();
    store.imageset_add(&set.id, &put.id).unwrap();

    let facts: std::collections::HashMap<String, Vec<String>> =
        store.facts_for(&put.id).unwrap().into_iter().collect();
    assert_eq!(
        facts["https://repolex.ai/ontology/pan/id"],
        vec![put.iri.clone()],
        "pan:id is the Image's identity in the graph"
    );
    assert_eq!(
        facts["https://repolex.ai/ontology/pan/createdDate"],
        vec![put.created_date.clone()],
        "pan:createdDate in the graph, as in the file"
    );
    assert!(
        facts.contains_key("https://repolex.ai/ontology/pan/relatedToId"),
        "membership is pan:relatedToId"
    );

    // The whole store: no predicate pand wrote lives under git-lex or subtexture.
    let mut foreign = Vec::new();
    for ns in [
        "https://repolex.ai/ontology/git-lex/",
        "https://repolex.ai/ontology/subtexture/",
    ] {
        let q = format!("SELECT ?s ?p WHERE {{ ?s ?p ?o . FILTER(STRSTARTS(STR(?p), \"{ns}\")) }}");
        if let pan::QueryResults::Solutions(sols) = store.query(&q).unwrap() {
            for s in sols {
                let s = s.unwrap();
                foreign.push(format!("{} {}", s.get("s").unwrap(), s.get("p").unwrap()));
            }
        }
    }
    assert!(
        foreign.is_empty(),
        "pand wrote predicates outside pan: {foreign:?}"
    );
}
