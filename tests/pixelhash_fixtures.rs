//! `pixel_sha256` hashes the pixels as the file has them, and these fixtures
//! prove it drops nothing.
//!
//! The three files are the same picture saved three ways: 8-bit RGB, the same
//! with an alpha channel, and the same at 16 bits. They used to be required to
//! hash IDENTICALLY, because the hash normalised everything down to 8-bit RGB
//! to match another program byte for byte. goodlux ruled on 2026-09-21 that
//! matching another program is not a requirement and the hash is of the
//! pixels as they are, so the three must now hash DIFFERENTLY: alpha is real
//! and the low byte of a 16-bit sample is real.

use pan::xmp::pixel_sha256;
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pixelhash")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

#[test]
fn alpha_and_bit_depth_are_hashed_not_discarded() {
    let rgb8 = pixel_sha256(&fixture("rgb8.png")).unwrap();
    let rgba8 = pixel_sha256(&fixture("rgba8.png")).unwrap();
    let rgb16 = pixel_sha256(&fixture("rgb16.png")).unwrap();
    assert_ne!(rgb8, rgba8, "an alpha channel must change the hash");
    assert_ne!(rgb8, rgb16, "sixteen-bit samples must change the hash");
    assert_ne!(rgba8, rgb16, "these are three different rasters");
    for h in [&rgb8, &rgba8, &rgb16] {
        assert!(
            h.starts_with("sha256:"),
            "hash is labelled with its algorithm: {h}"
        );
        assert_eq!(h.len(), "sha256:".len() + 64, "full sha256 digest: {h}");
    }
}

#[test]
fn the_same_bytes_hash_the_same_every_time() {
    let a = pixel_sha256(&fixture("rgb8.png")).unwrap();
    let b = pixel_sha256(&fixture("rgb8.png")).unwrap();
    assert_eq!(a, b);
}
