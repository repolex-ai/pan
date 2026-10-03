//! Facts that arrived under an older spelling of a producer's namespace are
//! moved to the current one, in the graph and in the image's own XMP
//! (goodlux, 2026-10-03, for the copia facts under `ontology/kit/copia/`).

use pan::Pan;

const OLD: &str = "https://repolex.ai/ontology/kit/copia/";
const NEW: &str = "https://repolex.ai/ontology/copia/";

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

fn png_with(seed: u8, ns: &str, root_fields: &str) -> Vec<u8> {
    let packet = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         <rdf:Description rdf:about=\"\" xmlns:copia=\"{ns}\">\n\
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

#[test]
fn old_namespace_moves_to_the_new_one_in_graph_and_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    let old = store
        .put(
            &png_with(
                1,
                OLD,
                "<copia:seed>42</copia:seed>\n<copia:genModel>m</copia:genModel>",
            ),
            Some("image/png"),
        )
        .unwrap();
    let already = store
        .put(
            &png_with(2, NEW, "<copia:seed>7</copia:seed>"),
            Some("image/png"),
        )
        .unwrap();
    assert!(facts(&store, &old.id).contains_key(&format!("{OLD}seed")));

    let r = store.rename_namespace(OLD, NEW).unwrap();
    assert_eq!((r.images, r.quads), (1, 2), "one image, two facts moved");

    let f = facts(&store, &old.id);
    assert!(
        !f.keys().any(|k| k.starts_with(OLD)),
        "nothing left under the old spelling"
    );
    assert_eq!(f[&format!("{NEW}seed")], vec!["42".to_string()]);
    assert_eq!(f[&format!("{NEW}genModel")], vec!["m".to_string()]);
    assert_eq!(
        facts(&store, &already.id)[&format!("{NEW}seed")],
        vec!["7".to_string()],
        "untouched"
    );

    // The file says the same as the graph.
    let media = f[&format!("{}mediaPath", pan::PAN_NS)][0].clone();
    let bytes = std::fs::read(store.layout.abs(&media)).unwrap();
    let packet = pan::xmp::read_xmp_packet_from_bytes(&bytes)
        .unwrap()
        .unwrap();
    assert!(
        !packet.contains(OLD),
        "the old namespace is gone from the XMP"
    );
    assert!(packet.contains(&format!("xmlns:copia=\"{NEW}\"")));
    let quads = pan::xmp::load_packet_statements(&packet, &old.iri).unwrap();
    assert!(quads
        .iter()
        .any(|q| q.predicate.as_str() == format!("{NEW}seed")));

    // Running it again finds nothing to do.
    let r2 = store.rename_namespace(OLD, NEW).unwrap();
    assert_eq!((r2.images, r2.quads), (0, 0));
}

#[test]
fn same_namespace_twice_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_at(dir.path());
    assert!(store.rename_namespace(NEW, NEW).is_err());
    assert!(store.rename_namespace("", NEW).is_err());
}
