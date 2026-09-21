//! Ingest confirms what a file is before it does anything else (goodlux,
//! 2026-09-21). The format comes from the bytes, an image is decoded in
//! full, and a file that will not decode is refused with nothing stored.
//! What Pan records as the type is what it verified, never the sender's
//! label.

use pan::Pan;

fn png() -> Vec<u8> {
    let img = image::RgbImage::from_fn(16, 16, |x, y| image::Rgb([x as u8 * 9, y as u8 * 9, 40]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn jpeg() -> Vec<u8> {
    let img = image::RgbImage::from_fn(16, 16, |x, y| image::Rgb([x as u8 * 9, 90, y as u8 * 9]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&img)
        .unwrap();
    out
}

fn open() -> (tempfile::TempDir, Pan) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pan.yml"), "storage_id: verify\n").unwrap();
    let store = Pan::open(dir.path()).unwrap();
    (dir, store)
}

fn files_under(root: &std::path::Path) -> usize {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                n += 1;
            }
        }
    }
    n
}

fn media_type_of(store: &Pan, id: &str) -> String {
    store
        .facts_for(id)
        .unwrap()
        .iter()
        .find(|(p, _)| p.ends_with("/mediaType"))
        .and_then(|(_, v)| v.first().cloned())
        .unwrap()
}

#[test]
fn a_file_that_starts_like_a_png_and_will_not_decode_is_refused_and_nothing_is_kept() {
    let (_dir, store) = open();
    // A true PNG signature, then nonsense: the old check passed this.
    let mut broken = png()[..8].to_vec();
    broken.extend_from_slice(&[0u8; 400]);
    let err = store.put(&broken, Some("image/png")).unwrap_err();
    assert!(
        format!("{err:#}").contains("not a readable image"),
        "{err:#}"
    );

    // A PNG cut off halfway through its pixel data.
    let whole = png();
    let err = store
        .put(&whole[..whole.len() / 2], Some("image/png"))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("not a readable image"),
        "{err:#}"
    );

    // Plain text wearing an image label.
    let err = store
        .put(b"this is not a picture", Some("image/jpeg"))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("not a readable image"),
        "{err:#}"
    );

    assert_eq!(
        files_under(&store.layout.media_root),
        0,
        "a refused delivery leaves no file behind"
    );
    assert_eq!(
        store.counts().unwrap().images,
        0,
        "and no image in the graph"
    );
}

#[test]
fn the_recorded_type_comes_from_the_bytes_not_from_the_label() {
    let (_dir, store) = open();

    // A real PNG labelled as a JPEG: stored as the PNG it is, nothing converted.
    let a = store.put(&png(), Some("image/jpeg")).unwrap();
    assert_eq!(media_type_of(&store, &a.id), "image/png");
    assert!(a.media_path.ends_with(".png"), "{}", a.media_path);
    assert!(a.original_path.is_none(), "a PNG arrival has no original");

    // A real PNG with a label that is not a type at all.
    let b = store.put(&png(), Some("image/@@nonsense")).unwrap();
    assert_eq!(media_type_of(&store, &b.id), "image/png");

    // A real JPEG labelled as a PNG: converted, and the original kept as the
    // JPEG it is.
    let c = store.put(&jpeg(), Some("image/png")).unwrap();
    assert_eq!(media_type_of(&store, &c.id), "image/png");
    assert!(c.media_path.ends_with(".png"), "{}", c.media_path);
    let original = c.original_path.expect("the JPEG as delivered is kept");
    assert!(original.ends_with(".jpg"), "{original}");

    // A real PNG with no label at all.
    let d = store.put(&png(), None).unwrap();
    assert_eq!(media_type_of(&store, &d.id), "image/png");

    // Every one of them got a thumbnail from the same decode.
    assert_eq!(store.counts().unwrap().thumbnails, 4);
}
