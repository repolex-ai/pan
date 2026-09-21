//! `image_sha256` hashes every block of a PNG except its XMP.
//!
//! goodlux, 2026-09-21: everything that is not XMP gets hashed. So the hash
//! must move when any part of the file that is not Pan's own metadata moves,
//! and must stay put when Pan rewrites that metadata.
//!
//! The three fixtures are the same picture saved three ways — 8-bit RGB, the
//! same with an alpha channel, and the same at 16 bits. Until today they were
//! required to hash IDENTICALLY, because the hash normalised everything down
//! to 8-bit RGB to match another program byte for byte. Nobody asked for that
//! rule and it threw away real information.

use pan::xmp::{image_sha256, write_packet_into_png_bytes};
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pixelhash")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

#[test]
fn three_different_files_hash_three_different_ways() {
    let rgb8 = image_sha256(&fixture("rgb8.png")).unwrap();
    let rgba8 = image_sha256(&fixture("rgba8.png")).unwrap();
    let rgb16 = image_sha256(&fixture("rgb16.png")).unwrap();
    assert_ne!(rgb8, rgba8, "an alpha channel must change the hash");
    assert_ne!(rgb8, rgb16, "sixteen-bit samples must change the hash");
    assert_ne!(rgba8, rgb16, "these are three different files");
    for h in [&rgb8, &rgba8, &rgb16] {
        assert!(h.starts_with("sha256:"), "labelled with its algorithm: {h}");
        assert_eq!(h.len(), "sha256:".len() + 64, "a full sha256 digest: {h}");
    }
}

#[test]
fn writing_xmp_does_not_move_the_hash() {
    let png = fixture("rgb8.png");
    let before = image_sha256(&png).unwrap();
    let once = write_packet_into_png_bytes(&png, "<x>first packet</x>").unwrap();
    let twice = write_packet_into_png_bytes(&once, "<x>a completely different packet</x>").unwrap();
    assert_ne!(png, once, "the file bytes do change");
    assert_eq!(image_sha256(&once).unwrap(), before, "adding XMP moved it");
    assert_eq!(
        image_sha256(&twice).unwrap(),
        before,
        "rewriting XMP moved it"
    );
}

#[test]
fn the_same_bytes_hash_the_same_every_time() {
    assert_eq!(
        image_sha256(&fixture("rgb8.png")).unwrap(),
        image_sha256(&fixture("rgb8.png")).unwrap()
    );
}
