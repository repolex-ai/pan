//! Where an arrival lands (goodlux, 2026-09-16): the source is always a PNG
//! under img/source/, a non-PNG arrival is kept as delivered under
//! img/original/, the thumbnail is the _512 JPEG under img/jpg/.

use pan::Pan;

fn open() -> (tempfile::TempDir, Pan) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pan.yml"), "storage_id: test-store\n").unwrap();
    let store = Pan::open(dir.path()).unwrap();
    (dir, store)
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 5) as u8, (y * 9) as u8, 77]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92).encode_image(&img).unwrap();
    out
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 5) as u8, (y * 9) as u8, 77]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[test]
fn a_jpeg_arrival_is_kept_as_original_and_worked_from_as_png() {
    let (_dir, store) = open();
    let arrival = jpeg(64, 40);
    let r = store.put(&arrival, Some("image/jpeg")).unwrap();

    assert!(r.media_path.starts_with("image/img/source/"), "{}", r.media_path);
    assert!(r.media_path.ends_with(&format!("{}.png", r.id)), "{}", r.media_path);
    let original = r.original_path.clone().expect("a converted arrival keeps its original");
    assert!(original.starts_with("image/img/original/"), "{original}");
    assert!(original.ends_with(&format!("{}.jpg", r.id)), "{original}");

    let source = std::fs::read(store.layout.abs(&r.media_path)).unwrap();
    assert!(pan::xmp::is_png(&source), "the source is a PNG");
    let packet = pan::xmp::read_xmp_packet_from_bytes(&source).unwrap().expect("Pan's XMP is inside the source");
    assert!(packet.contains(&format!("&lt;pan/Image/{}&gt;", r.id)));

    let kept = std::fs::read(store.layout.abs(&original)).unwrap();
    assert_eq!(kept, arrival, "the original is the bytes as delivered");

    let a = image::load_from_memory(&arrival).unwrap().to_rgb8();
    let b = image::load_from_memory(&source).unwrap().to_rgb8();
    assert_eq!(a.as_raw(), b.as_raw(), "same pixels in the PNG as the decoder saw");

    let (_, facts) = store.get(&r.id).unwrap();
    let media_type = facts.iter().find(|(p, _)| p.ends_with("/mediaType")).map(|(_, v)| v[0].clone()).unwrap();
    assert_eq!(media_type, "image/png", "the stored bytes are PNG, and the graph says so");
    let source_file = facts.iter().find(|(p, _)| p.ends_with("/sourceFile")).map(|(_, v)| v[0].clone()).unwrap();
    assert_eq!(source_file, original, "pan:sourceFile names the original the PNG was made from");
    assert!(packet.contains(&format!("<pan:sourceFile>{original}</pan:sourceFile>")), "sourceFile rides in the XMP");
    let thumb = facts.iter().find(|(p, _)| p.ends_with("/thumbnail")).expect("a thumbnail node");
    assert_eq!(thumb.1.len(), 1);
    let thumb_path = std::fs::read_dir(store.layout.media_root.join("image/img/jpg")).map(|_| ()).is_ok();
    assert!(thumb_path, "thumbnails live under img/jpg/");
}

#[test]
fn a_png_arrival_has_no_original() {
    let (_dir, store) = open();
    let r = store.put(&png(64, 40), Some("image/png")).unwrap();
    assert!(r.original_path.is_none());
    assert!(r.media_path.starts_with("image/img/source/"));
    assert!(!store.layout.media_root.join("image/img/original").exists());
    let (_, facts) = store.get(&r.id).unwrap();
    let source_file = facts.iter().find(|(p, _)| p.ends_with("/sourceFile")).map(|(_, v)| v[0].clone()).unwrap();
    assert_eq!(source_file, r.media_path, "a PNG arrival's sourceFile is the source itself");
    let thumbs: Vec<_> = walk(&store.layout.media_root.join("image/img/jpg"));
    assert_eq!(thumbs.len(), 1);
    assert!(thumbs[0].ends_with(&format!("{}_512.jpg", r.id)), "{}", thumbs[0]);
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}

fn tiff(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 5) as u8, (y * 9) as u8, 77]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Tiff).unwrap();
    out.into_inner()
}

#[test]
fn a_tiff_arrival_converts_to_png_with_the_same_pixels() {
    let (_dir, store) = open();
    let arrival = tiff(48, 32);
    let r = store.put(&arrival, Some("image/tiff")).unwrap();
    let original = r.original_path.clone().expect("a converted arrival keeps its original");
    assert!(original.ends_with(&format!("{}.tiff", r.id)) || original.ends_with(&format!("{}.tif", r.id)), "{original}");
    let source = std::fs::read(store.layout.abs(&r.media_path)).unwrap();
    assert!(pan::xmp::is_png(&source));
    let a = image::load_from_memory(&arrival).unwrap().to_rgb8();
    let b = image::load_from_memory(&source).unwrap().to_rgb8();
    assert_eq!(a.as_raw(), b.as_raw(), "same pixels in the PNG as in the TIFF");
}

/// pan issue #27: the Thumbnail's producedDate was in the graph but not in
/// the image's XMP thumbnail struct. File and graph must say the same.
#[test]
fn the_thumbnail_struct_in_the_file_carries_the_produced_date_the_graph_has() {
    let (_dir, store) = open();
    let r = store.put(&png(64, 40), Some("image/png")).unwrap();

    // The graph: the Thumbnail node's pan:producedDate.
    let q = "PREFIX pan: <https://repolex.ai/ontology/pan/> SELECT ?d WHERE { ?img pan:thumbnail ?t . ?t pan:producedDate ?d }";
    let mut graph_dates = Vec::new();
    if let pan::QueryResults::Solutions(sols) = store.query(q).unwrap() {
        for s in sols {
            let s = s.unwrap();
            let pan::Term::Literal(l) = s.get("d").unwrap().clone() else { panic!("producedDate is a literal") };
            graph_dates.push(l.value().to_string());
        }
    }
    assert_eq!(graph_dates.len(), 1, "one Thumbnail node with one producedDate: {graph_dates:?}");
    assert_eq!(graph_dates[0], r.created_date, "the thumbnail is produced when the image is created");

    // The file: the pan:thumbnail struct inside the source PNG's XMP.
    let source = std::fs::read(store.layout.abs(&r.media_path)).unwrap();
    let packet = pan::xmp::read_xmp_packet_from_bytes(&source).unwrap().expect("Pan's XMP is inside the source");
    let parsed = pan::xmp::parse_packet(&packet).unwrap();
    let thumb = parsed
        .iter()
        .flat_map(|s| s.structs.iter())
        .find(|(pred, _)| pred.ends_with("/thumbnail"))
        .and_then(|(_, members)| members.first())
        .expect("the file carries the thumbnail struct");
    let file_date = thumb.iter().find(|(f, _)| f.ends_with("/producedDate")).map(|(_, v)| v.value().to_string());
    assert_eq!(file_date.as_deref(), Some(graph_dates[0].as_str()), "pan:producedDate in the file's thumbnail struct equals the graph's: {thumb:?}");
}
