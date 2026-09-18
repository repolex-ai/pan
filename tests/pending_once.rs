//! Issue #29 (go-live review, 2026-09-17): an image with several scene
//! objects was handed to the sam3 stage once PER OBJECT, because the
//! work-list query joined on pan:sceneObjects without DISTINCT. Four paid
//! segmentation calls, four references, 240 regions for 60. This pins:
//! one image, one row, however many objects the caption named.

use pan::{Pan, Perception};

fn make_png(seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 10, 10);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        let px: Vec<u8> = (0..300)
            .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
            .collect();
        w.write_image_data(&px).unwrap();
        w.finish().unwrap();
    }
    out
}

fn captioned(store: &Pan, seed: u8, objects: &str) -> String {
    let put = store.put(&make_png(seed), Some("image/png")).unwrap();
    let json = format!(
        "{{\"shortCaption\": \"s\", \"longCaption\": \"a long description\", \"sceneObjects\": {objects}, \"sceneMood\": \"still\"}}"
    );
    let p = Perception::parse(&json).unwrap();
    store.set_perception(&put.id, &p).unwrap();
    put.id
}

#[test]
fn image_with_many_scene_objects_is_pending_for_sam3_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let many = captioned(
        &store,
        1,
        "[\"hand\", \"hand\", \"face\", \"rock\", \"sky\", \"tree\"]",
    );
    let one = captioned(&store, 2, "[\"cat\"]");
    // No caption at all: not ready for sam3, must not appear.
    let bare = store.put(&make_png(3), Some("image/png")).unwrap().id;

    let pending = store
        .pending_for("regionData", "facebook/sam3", 16, None)
        .unwrap();
    let ids: Vec<&str> = pending.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids.iter().filter(|i| **i == many).count(),
        1,
        "one row for the many-object image: {ids:?}"
    );
    assert_eq!(
        ids.iter().filter(|i| **i == one).count(),
        1,
        "one row for the one-object image: {ids:?}"
    );
    assert!(
        !ids.contains(&bare.as_str()),
        "an uncaptioned image is not sam3-pending: {ids:?}"
    );
    assert_eq!(
        pending.len(),
        2,
        "exactly two images pending, no repeats: {ids:?}"
    );
}

#[test]
fn embed_work_list_is_one_row_per_image() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    let a = captioned(&store, 4, "[\"wolf\", \"ridge\", \"snow\"]");
    let pending = store
        .pending_for("vectorData", "qwen3-vl-embedding-2b", 16, None)
        .unwrap();
    assert_eq!(pending.len(), 1, "one row for one captioned image");
    assert_eq!(pending[0].id, a);
}
