//! What Pan puts on the wire to a CAPTION provider — and only a caption
//! provider.
//!
//! Rob, 2026-09-05: the stored PNG carries Horae's copia block and Pan's own
//! block in its XMP. A captioning model must not see that data (it would read
//! the scene description it is supposed to produce), and a third-party API
//! must not receive it (it is the soul's data, not the provider's). So Pan
//! sends pixels and nothing else: the stored image decoded to RGB and
//! re-encoded as a same-size, high-quality JPEG. No metadata survives a
//! decode-to-pixels round trip — there is no "strip" step to get wrong.
//!
//! This is Pan's job, not Iris's (Rob, 2026-09-05): the only way to
//! guarantee one standard and no leak is for the producer of the wire bytes
//! to be the one that makes them. Nothing en route may massage the data.
//!
//! Pose, segment and depth get the same kind of copy at quality 90 (goodlux,
//! 2026-11-04): same pixel size, so the geometry they return is still in the
//! stored image's pixel space, and about a fifth of the bytes, which matters
//! because the upload link is the cap on every perception pass (a 3.5 MB
//! PNG is a 0.6 MB JPEG). Embed keeps receiving the stored PNG byte for
//! byte, so the vectors already made and the ones still to come are one
//! recipe.

use anyhow::{Context, Result};
use image::ImageReader;
use std::io::Cursor;

/// JPEG quality for the caption wire copy. High enough that the caption
/// model sees what a viewer sees; Phala's own ingest takes JPEG readily
/// (m3rc measured: raw PNG 200 s timeout, JPEG 37 s).
pub const CAPTION_JPEG_QUALITY: u8 = 92;

/// JPEG quality for the pose, segment and depth copy. Keypoints, outlines
/// and a depth map do not change at this quality; the bytes do.
pub const PERCEPT_JPEG_QUALITY: u8 = 90;

pub struct WireImage {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// The caption-provider copy of a stored image: same pixel size, RGB, JPEG,
/// no metadata of any kind.
pub fn caption_copy(stored: &[u8]) -> Result<WireImage> {
    pixels_copy(stored, CAPTION_JPEG_QUALITY)
}

/// The pose, segment and depth copy: the same thing at `PERCEPT_JPEG_QUALITY`.
pub fn percept_copy(stored: &[u8]) -> Result<WireImage> {
    pixels_copy(stored, PERCEPT_JPEG_QUALITY)
}

fn pixels_copy(stored: &[u8], quality: u8) -> Result<WireImage> {
    let img = ImageReader::new(Cursor::new(stored))
        .with_guessed_format()
        .context("sniff image format")?
        .decode()
        .context("decode stored image for the wire copy")?;
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    enc.encode_image(&rgb).context("encode wire jpeg")?;
    Ok(WireImage {
        width: rgb.width(),
        height: rgb.height(),
        bytes: out,
        media_type: "image/jpeg",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percept_copy_keeps_the_pixel_size_and_is_a_jpeg() {
        let img = image::RgbaImage::from_fn(40, 24, |x, y| image::Rgba([x as u8 * 6, y as u8 * 10, 90, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let w = percept_copy(&png).unwrap();
        assert_eq!((w.width, w.height), (40, 24));
        assert_eq!(w.media_type, "image/jpeg");
        assert_eq!(&w.bytes[..2], &[0xFF, 0xD8], "JPEG SOI marker");
        let back = ImageReader::new(Cursor::new(&w.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!((back.width(), back.height()), (40, 24));
    }

    /// A PNG that carries an XMP packet in an iTXt chunk, the way Horae's
    /// output does — the thing that must NOT reach the caption provider.
    fn png_with_xmp(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 7, 200])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        // Splice an iTXt chunk with an XMP packet before IEND.
        let xmp = b"<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:copia=\"https://repolex.ai/copia/\"><copia:Moment>SECRET-MOMENT-TEXT</copia:Moment></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>";
        let mut data = Vec::new();
        data.extend_from_slice(b"XML:com.adobe.xmp\0\0\0\0\0");
        data.extend_from_slice(xmp);
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
        chunk.extend_from_slice(b"iTXt");
        chunk.extend_from_slice(&data);
        let crc = crc32(&chunk[4..]);
        chunk.extend_from_slice(&crc.to_be_bytes());
        let iend = out.len() - 12;
        out.splice(iend..iend, chunk);
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut c: u32 = 0xFFFF_FFFF;
        for &b in bytes {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn caption_copy_is_same_size_jpeg_with_no_metadata() {
        let src = png_with_xmp(640, 960);
        assert!(
            contains(&src, b"SECRET-MOMENT-TEXT"),
            "fixture carries the packet"
        );
        assert!(
            contains(&src, b"adobe:ns:meta"),
            "fixture carries the xmp envelope"
        );
        let w = caption_copy(&src).unwrap();
        assert_eq!((w.width, w.height), (640, 960), "same pixel size");
        assert_eq!(w.media_type, "image/jpeg");
        assert!(w.bytes.starts_with(&[0xFF, 0xD8]), "jpeg magic");
        assert!(
            !contains(&w.bytes, b"SECRET-MOMENT-TEXT"),
            "no moment text on the wire"
        );
        assert!(
            !contains(&w.bytes, b"adobe:ns:meta"),
            "no xmp envelope on the wire"
        );
        assert!(
            !contains(&w.bytes, b"http://ns.adobe.com/xap/1.0/"),
            "no APP1 xmp marker"
        );
        assert!(!contains(&w.bytes, b"Exif"), "no exif");
    }

    #[test]
    fn caption_copy_of_a_jpeg_is_still_a_clean_jpeg() {
        let img = image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([x as u8, y as u8, 1]));
        let mut src = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut src), image::ImageFormat::Jpeg)
            .unwrap();
        let w = caption_copy(&src).unwrap();
        assert_eq!((w.width, w.height), (64, 48));
        assert!(w.bytes.starts_with(&[0xFF, 0xD8]));
    }
}
