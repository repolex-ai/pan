//! Thumbnail generation — a reduced rendition Pan makes at ingest.
//!
//! Longest edge bounded to [`THUMB_MAX_EDGE`], JPEG output. Written beside
//! the media under `thumbnail/YYYY/MM/DD/<id>.jpg` and declared on the image
//! as a `pan:Thumbnail` node (path, width, height) — never left to filesystem
//! convention.

use anyhow::{Context, Result};
use image::imageops::FilterType;
use image::ImageReader;
use std::io::Cursor;

pub const THUMB_MAX_EDGE: u32 = 512;
pub const THUMB_JPEG_QUALITY: u8 = 85;

pub struct Thumb {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The source image's own pixel size, read on the way through.
    pub source_width: u32,
    pub source_height: u32,
}

pub fn make(bytes: &[u8]) -> Result<Thumb> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("sniff image format")?
        .decode()
        .context("decode image for thumbnail")?;
    let (sw, sh) = (img.width(), img.height());
    let small = if sw.max(sh) > THUMB_MAX_EDGE {
        img.resize(THUMB_MAX_EDGE, THUMB_MAX_EDGE, FilterType::Triangle)
    } else {
        img
    };
    let rgb = small.to_rgb8();
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, THUMB_JPEG_QUALITY);
    enc.encode_image(&rgb).context("encode thumbnail jpeg")?;
    Ok(Thumb {
        width: rgb.width(),
        height: rgb.height(),
        jpeg: out,
        source_width: sw,
        source_height: sh,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 7]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn large_image_is_bounded_on_its_longest_edge() {
        let t = make(&png(2048, 1024)).unwrap();
        assert_eq!(t.width, THUMB_MAX_EDGE);
        assert_eq!(t.height, THUMB_MAX_EDGE / 2);
        assert_eq!((t.source_width, t.source_height), (2048, 1024));
        assert!(t.jpeg.starts_with(&[0xFF, 0xD8]), "jpeg magic");
    }

    #[test]
    fn small_image_is_not_upscaled() {
        let t = make(&png(100, 60)).unwrap();
        assert_eq!((t.width, t.height), (100, 60));
    }
}
