//! Arrival → source: the managed store keeps ONE working format, PNG
//! (goodlux, 2026-09-16). A JPEG, WebP, GIF or TIFF that arrives is decoded
//! once and written out as PNG; the bytes as delivered are kept beside it
//! under `img/original/` and never read again.
//!
//! What this carries: the pixels, at the decoded bit depth and channel
//! count. What it does NOT carry yet — the ICC profile, EXIF, and any XMP the
//! arrival held — is issue #23 (PNG-only ingest, metadata carry-over,
//! pixel-hash verification). Until #23 lands, a converted source starts with
//! only the XMP Pan writes.

use anyhow::{Context, Result};
use image::ImageReader;
use std::io::Cursor;

/// Decode any supported raster format and re-encode the pixels as PNG.
/// Lossless with respect to the decoded pixels; irreversible with respect to
/// the arriving file.
pub fn to_png(bytes: &[u8]) -> Result<Vec<u8>> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("sniff image format")?
        .decode()
        .context("decode image for PNG conversion")?;
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .context("encode PNG")?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img =
            image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 7) as u8, (y * 3) as u8, 128]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92)
            .encode_image(&img)
            .unwrap();
        out
    }

    #[test]
    fn jpeg_becomes_png_with_the_same_decoded_pixels() {
        let j = jpeg(20, 12);
        let p = to_png(&j).unwrap();
        assert!(crate::xmp::is_png(&p));
        let a = image::load_from_memory(&j).unwrap().to_rgb8();
        let b = image::load_from_memory(&p).unwrap().to_rgb8();
        assert_eq!(a.dimensions(), b.dimensions());
        assert_eq!(
            a.as_raw(),
            b.as_raw(),
            "the PNG holds exactly what the decoder saw"
        );
    }
}
