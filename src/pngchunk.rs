//! PNG chunk surgery — replace ONE chunk, copy every other byte unchanged.
//!
//! Pan writes its XMP into the image (Rob, 2026-09-03: standard RDF-in-XMP,
//! nothing invented) and NEVER strips what was already there. So the file is
//! not decoded and re-encoded: every chunk the producer wrote — IHDR, IDAT,
//! sdapi's `parameters` tEXt, eXIf, iCCP, anything — is copied byte-for-byte,
//! and only the `XML:com.adobe.xmp` text chunk is replaced (or added, before
//! IEND, when there was none). Pixels cannot change because IDAT is never
//! touched.

use anyhow::{anyhow, Result};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
pub const XMP_KEYWORD: &str = "XML:com.adobe.xmp";

/// One chunk as it sits in the file: type + raw data (no length/CRC).
#[derive(Debug, Clone)]
pub struct Chunk {
    pub kind: [u8; 4],
    pub data: Vec<u8>,
}

impl Chunk {
    pub fn kind_str(&self) -> &str {
        std::str::from_utf8(&self.kind).unwrap_or("????")
    }

    /// The keyword of a tEXt / iTXt / zTXt chunk (up to the first NUL).
    pub fn text_keyword(&self) -> Option<&str> {
        match self.kind_str() {
            "tEXt" | "iTXt" | "zTXt" => {
                let end = self.data.iter().position(|b| *b == 0)?;
                std::str::from_utf8(&self.data[..end]).ok()
            }
            _ => None,
        }
    }

    fn is_xmp(&self) -> bool {
        self.text_keyword() == Some(XMP_KEYWORD)
    }
}

pub fn read_chunks(png: &[u8]) -> Result<Vec<Chunk>> {
    if png.len() < 8 || png[..8] != SIGNATURE {
        return Err(anyhow!("not a PNG (bad signature)"));
    }
    let mut chunks = Vec::new();
    let mut i = 8;
    while i + 8 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&png[i + 4..i + 8]);
        let start = i + 8;
        let end = start.checked_add(len).ok_or_else(|| anyhow!("PNG chunk length overflow"))?;
        if end + 4 > png.len() {
            return Err(anyhow!("truncated PNG chunk {}", std::str::from_utf8(&kind).unwrap_or("????")));
        }
        chunks.push(Chunk { kind, data: png[start..end].to_vec() });
        i = end + 4; // skip CRC; recomputed on write
        if &kind == b"IEND" {
            break;
        }
    }
    if chunks.last().map(|c| &c.kind) != Some(b"IEND") {
        return Err(anyhow!("PNG has no IEND chunk"));
    }
    Ok(chunks)
}

pub fn write_chunks(chunks: &[Chunk]) -> Vec<u8> {
    let total: usize = 8 + chunks.iter().map(|c| 12 + c.data.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&SIGNATURE);
    for c in chunks {
        out.extend_from_slice(&(c.data.len() as u32).to_be_bytes());
        out.extend_from_slice(&c.kind);
        out.extend_from_slice(&c.data);
        let mut h = crc32fast::Hasher::new();
        h.update(&c.kind);
        h.update(&c.data);
        out.extend_from_slice(&h.finalize().to_be_bytes());
    }
    out
}

/// An uncompressed UTF-8 iTXt chunk carrying the XMP packet — the PNG home
/// for XMP (Adobe XMP spec part 3): keyword, compression flag 0, method 0,
/// empty language tag, empty translated keyword, then the text.
fn xmp_itxt(packet: &str) -> Chunk {
    let mut data = Vec::with_capacity(XMP_KEYWORD.len() + 5 + packet.len());
    data.extend_from_slice(XMP_KEYWORD.as_bytes());
    data.push(0); // keyword terminator
    data.push(0); // compression flag: uncompressed
    data.push(0); // compression method
    data.push(0); // language tag (empty) terminator
    data.push(0); // translated keyword (empty) terminator
    data.extend_from_slice(packet.as_bytes());
    Chunk { kind: *b"iTXt", data }
}

/// Return the PNG with its XMP chunk replaced by `packet`. Every other chunk
/// is copied unchanged, in order. With no prior XMP chunk, the new one is
/// inserted just before IEND.
pub fn replace_xmp(png: &[u8], packet: &str) -> Result<Vec<u8>> {
    let mut chunks = read_chunks(png)?;
    let new = xmp_itxt(packet);
    match chunks.iter().position(|c| c.is_xmp()) {
        Some(pos) => {
            chunks[pos] = new;
            // A second stray XMP chunk (some writers leave one) would make the
            // file ambiguous; keep exactly one.
            let mut seen = false;
            chunks.retain(|c| {
                if c.is_xmp() {
                    if seen {
                        return false;
                    }
                    seen = true;
                }
                true
            });
        }
        None => {
            let iend = chunks.len() - 1;
            chunks.insert(iend, new);
        }
    }
    Ok(write_chunks(&chunks))
}

/// The XMP packet text, if the PNG carries one (tEXt / iTXt uncompressed;
/// a zlib-compressed zTXt or compressed iTXt is decoded).
pub fn read_xmp(png: &[u8]) -> Result<Option<String>> {
    for c in read_chunks(png)? {
        if !c.is_xmp() {
            continue;
        }
        let kw_end = c.data.iter().position(|b| *b == 0).unwrap_or(0);
        match c.kind_str() {
            "tEXt" => return Ok(Some(latin1(&c.data[kw_end + 1..]))),
            "zTXt" => {
                // keyword NUL method(1) zlib-data
                let body = &c.data[kw_end + 2..];
                return Ok(Some(latin1(&inflate(body)?)));
            }
            "iTXt" => {
                // keyword NUL flag(1) method(1) lang NUL translated NUL text
                let mut p = kw_end + 1;
                let flag = *c.data.get(p).ok_or_else(|| anyhow!("short iTXt"))?;
                p += 2;
                let lang_end = p + c.data[p..].iter().position(|b| *b == 0).ok_or_else(|| anyhow!("short iTXt"))?;
                p = lang_end + 1;
                let tr_end = p + c.data[p..].iter().position(|b| *b == 0).ok_or_else(|| anyhow!("short iTXt"))?;
                let text = &c.data[tr_end + 1..];
                let bytes = if flag == 1 { inflate(text)? } else { text.to_vec() };
                return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn inflate(z: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(z).read_to_end(&mut out).map_err(|e| anyhow!("inflate text chunk: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png_with_text() -> Vec<u8> {
        // Encode a 2x2 RGB PNG with a `parameters` tEXt chunk the way sdapi does.
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 2);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.add_text_chunk("parameters".into(), "a cat, Steps: 20, Seed: 42".into()).unwrap();
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]).unwrap();
        }
        out
    }

    #[test]
    fn replace_xmp_keeps_every_other_chunk_and_the_pixels() {
        let src = tiny_png_with_text();
        let before = read_chunks(&src).unwrap();
        let out = replace_xmp(&src, "<x:xmpmeta/>").unwrap();
        let after = read_chunks(&out).unwrap();
        assert_eq!(after.len(), before.len() + 1, "exactly one chunk added");
        let idat_before: Vec<_> = before.iter().filter(|c| &c.kind == b"IDAT").map(|c| c.data.clone()).collect();
        let idat_after: Vec<_> = after.iter().filter(|c| &c.kind == b"IDAT").map(|c| c.data.clone()).collect();
        assert_eq!(idat_before, idat_after, "pixel data untouched");
        assert!(after.iter().any(|c| c.text_keyword() == Some("parameters")), "sdapi parameters kept");
        assert_eq!(read_xmp(&out).unwrap().as_deref(), Some("<x:xmpmeta/>"));
        // Second replace swaps, never duplicates.
        let out2 = replace_xmp(&out, "<x:xmpmeta v='2'/>").unwrap();
        assert_eq!(read_chunks(&out2).unwrap().len(), after.len());
        assert_eq!(read_xmp(&out2).unwrap().as_deref(), Some("<x:xmpmeta v='2'/>"));
    }

    #[test]
    fn output_is_a_valid_png_for_a_real_decoder() {
        let out = replace_xmp(&tiny_png_with_text(), "<x:xmpmeta/>").unwrap();
        let mut r = png::Decoder::new(std::io::Cursor::new(&out)).read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size()];
        r.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..3], &[1, 2, 3]);
    }
}
