//! Pixel-CID cross-check against the canonical fixtures (carried from Pool).
//!
//! Two repos independently computing "the pixel cid" is a byte-drift landmine;
//! these fixtures pin Pan's png-crate path to the eye's PIL path byte-for-byte
//! (PIL 12.2.0). See tests/fixtures/pixelcid/MANIFEST.json.

use pan::xmp::compute_pixel_cid;
use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pixelcid")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

const EXPECTED: &str = "sha256:4570a542a91fe28e9a05cb49edf4123d2b71dea607085310cb38df196d018391";

#[test]
fn rgb8_matches_canonical_cid() {
    assert_eq!(compute_pixel_cid(&fixture("rgb8.png")).unwrap(), EXPECTED);
}

#[test]
fn rgba8_alpha_is_stripped_not_hashed() {
    // LOAD-BEARING: rgba8 must equal rgb8 — if they differ, alpha leaked in.
    assert_eq!(compute_pixel_cid(&fixture("rgba8.png")).unwrap(), EXPECTED);
}

#[test]
fn rgb16_downsamples_to_same_cid() {
    // Exercises the 16→8 high-byte downsample (PIL's >>8).
    assert_eq!(compute_pixel_cid(&fixture("rgb16.png")).unwrap(), EXPECTED);
}
