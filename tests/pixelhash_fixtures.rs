//! Pixel-hash cross-check against the canonical fixtures (carried from Pool).
//!
//! `pixel_hash` pins the stamp invariant (stamping never touches the image).
//! Two repos independently computing "the pixel hash" is a byte-drift
//! landmine; these fixtures pin Pan's png-crate path to the eye's PIL path
//! byte-for-byte (PIL 12.2.0). See tests/fixtures/pixelhash/MANIFEST.json
//! (the manifest predates the CID→panId rename; its "cid" keys mean this hash).

use pan::xmp::pixel_hash;
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pixelhash")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

const EXPECTED: &str = "sha256:4570a542a91fe28e9a05cb49edf4123d2b71dea607085310cb38df196d018391";

#[test]
fn rgb8_matches_canonical_hash() {
    assert_eq!(pixel_hash(&fixture("rgb8.png")).unwrap(), EXPECTED);
}

#[test]
fn rgba8_alpha_is_stripped_not_hashed() {
    // LOAD-BEARING: rgba8 must equal rgb8 — if they differ, alpha leaked in.
    assert_eq!(pixel_hash(&fixture("rgba8.png")).unwrap(), EXPECTED);
}

#[test]
fn rgb16_downsamples_to_same_hash() {
    // Exercises the 16→8 high-byte downsample (PIL's >>8).
    assert_eq!(pixel_hash(&fixture("rgb16.png")).unwrap(), EXPECTED);
}
