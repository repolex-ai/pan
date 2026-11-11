//! Video files (pan.ttl 0.4.22): MP4 and MOV, read from their own bytes.
//!
//! Both are the same box structure (ISO base media), so one walker serves
//! both: the file is a sequence of boxes, each a 32-bit size, a four-letter
//! type and a body; a size of 1 means a 64-bit size follows, a size of 0
//! means "to the end of the file". What Pan reads out of the header is the
//! movie duration and, from the video track, the frame rate and the picture
//! size. What Pan writes is one box at the end of the file: XMP in the
//! `uuid` box every XMP-aware reader looks for, so a rating or a note
//! changes a few kilobytes at the tail and never moves the picture data.
//! The first frame, for the thumbnail, comes from ffmpeg, the one outside
//! tool video needs (goodlux, 2026-11-11: a given when working with video).

use anyhow::{anyhow, Context, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// The UUID of the XMP box in MP4 and MOV files (Adobe's XMP specification,
/// part 3): BE7ACFCB-97A9-42E8-9C71-999491E3AFAC.
pub const XMP_UUID: [u8; 16] = [
    0xBE, 0x7A, 0xCF, 0xCB, 0x97, 0xA9, 0x42, 0xE8, 0x9C, 0x71, 0x99, 0x94, 0x91, 0xE3, 0xAF, 0xAC,
];

/// `video/mp4` or `video/quicktime` when the bytes begin with an `ftyp` box
/// (the one thing every MP4 and MOV starts with), None for anything else.
/// The brand says which: QuickTime's `qt  ` is a MOV, the rest are MP4.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    match &bytes[8..12] {
        b"qt  " => Some("video/quicktime"),
        _ => Some("video/mp4"),
    }
}

/// What the header says about the movie.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    /// Seconds, from the movie header.
    pub duration: f64,
    /// Frames per second of the video track: exact when every frame lasts
    /// the same time, the average otherwise. None when there is no video
    /// track.
    pub frame_rate: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// A number for an `xsd:decimal` field: up to three decimals, trailing
/// zeros dropped, so 24 is "24" and 29.97 is "29.97".
pub fn decimal(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

struct Box_ {
    offset: u64,
    header: u64,
    size: u64,
    kind: [u8; 4],
}

/// The boxes directly inside `[start, end)` of the reader. A size that runs
/// past `end` is reported as it is (the caller decides what a short last
/// box means).
fn boxes<R: Read + Seek>(r: &mut R, start: u64, end: u64) -> Result<Vec<Box_>> {
    let mut out = Vec::new();
    let mut off = start;
    while off + 8 <= end {
        r.seek(SeekFrom::Start(off))?;
        let mut h = [0u8; 8];
        r.read_exact(&mut h)?;
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let kind = [h[4], h[5], h[6], h[7]];
        let mut header = 8u64;
        if size == 1 {
            let mut l = [0u8; 8];
            r.read_exact(&mut l)?;
            size = u64::from_be_bytes(l);
            header = 16;
        } else if size == 0 {
            size = end - off;
        }
        if size < header {
            return Err(anyhow!(
                "box {:?} at byte {off} declares a size of {size}, smaller than its own header",
                String::from_utf8_lossy(&kind)
            ));
        }
        out.push(Box_ {
            offset: off,
            header,
            size,
            kind,
        });
        off += size;
    }
    Ok(out)
}

fn read_at<R: Read + Seek>(r: &mut R, off: u64, n: usize) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|s| {
        u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    })
}

/// Duration, frame rate and picture size from the `moov` box.
pub fn info(bytes: &[u8]) -> Result<VideoInfo> {
    let mut r = std::io::Cursor::new(bytes);
    let end = bytes.len() as u64;
    let top = boxes(&mut r, 0, end)?;
    let moov = top
        .iter()
        .find(|b| &b.kind == b"moov")
        .ok_or_else(|| anyhow!("no moov box: the file has no movie header"))?;
    let moov_body = moov.offset + moov.header;
    let moov_end = moov.offset + moov.size;
    let mut info = VideoInfo {
        duration: 0.0,
        frame_rate: None,
        width: None,
        height: None,
    };
    for b in boxes(&mut r, moov_body, moov_end)? {
        match &b.kind {
            b"mvhd" => {
                let body = read_at(&mut r, b.offset + b.header, (b.size - b.header) as usize)?;
                let version = body.first().copied().unwrap_or(0);
                let (timescale, duration) = if version == 1 {
                    (be32(&body, 20), be64(&body, 24))
                } else {
                    (be32(&body, 12), be32(&body, 16).map(u64::from))
                };
                let (Some(ts), Some(d)) = (timescale, duration) else {
                    return Err(anyhow!("mvhd box too short"));
                };
                if ts == 0 {
                    return Err(anyhow!("mvhd timescale is 0"));
                }
                info.duration = d as f64 / ts as f64;
            }
            b"trak" if info.frame_rate.is_none() => {
                if let Some((fps, w, h)) = video_track(&mut r, &b)? {
                    info.frame_rate = fps;
                    info.width = w;
                    info.height = h;
                }
            }
            _ => {}
        }
    }
    Ok(info)
}

/// (frame rate, width, height) when this `trak` is a video track.
fn video_track<R: Read + Seek>(
    r: &mut R,
    trak: &Box_,
) -> Result<Option<(Option<f64>, Option<u32>, Option<u32>)>> {
    let mut width = None;
    let mut height = None;
    let mut timescale: Option<u32> = None;
    let mut duration: Option<u64> = None;
    let mut is_video = false;
    let mut stts: Option<(u64, Option<u32>)> = None; // (sample count, the one delta if constant)
    for b in boxes(r, trak.offset + trak.header, trak.offset + trak.size)? {
        match &b.kind {
            b"tkhd" => {
                let body = read_at(r, b.offset + b.header, (b.size - b.header) as usize)?;
                let version = body.first().copied().unwrap_or(0);
                // After version+flags: creation and modification times,
                // track id, 4 reserved, duration (times and duration are 8
                // bytes in version 1); then 8 reserved, layer, alternate
                // group, volume, 2 reserved, the 36-byte matrix; then width
                // and height as 16.16 fixed point.
                let at = if version == 1 { 4 + 8 + 8 + 4 + 4 + 8 } else { 4 + 4 + 4 + 4 + 4 + 4 };
                let at = at + 8 + 2 + 2 + 2 + 2 + 36;
                width = be32(&body, at).map(|v| v >> 16);
                height = be32(&body, at + 4).map(|v| v >> 16);
            }
            b"mdia" => {
                for m in boxes(r, b.offset + b.header, b.offset + b.size)? {
                    match &m.kind {
                        b"mdhd" => {
                            let body =
                                read_at(r, m.offset + m.header, (m.size - m.header) as usize)?;
                            let version = body.first().copied().unwrap_or(0);
                            if version == 1 {
                                timescale = be32(&body, 20);
                                duration = be64(&body, 24);
                            } else {
                                timescale = be32(&body, 12);
                                duration = be32(&body, 16).map(u64::from);
                            }
                        }
                        b"hdlr" => {
                            let body = read_at(r, m.offset + m.header, 12)?;
                            is_video = &body[8..12] == b"vide";
                        }
                        b"minf" => {
                            for n in boxes(r, m.offset + m.header, m.offset + m.size)? {
                                if &n.kind != b"stbl" {
                                    continue;
                                }
                                for s in boxes(r, n.offset + n.header, n.offset + n.size)? {
                                    if &s.kind != b"stts" {
                                        continue;
                                    }
                                    let body = read_at(
                                        r,
                                        s.offset + s.header,
                                        (s.size - s.header) as usize,
                                    )?;
                                    let entries = be32(&body, 4).unwrap_or(0) as usize;
                                    let mut count = 0u64;
                                    let mut delta: Option<u32> = None;
                                    let mut constant = true;
                                    for i in 0..entries {
                                        let c = be32(&body, 8 + i * 8).unwrap_or(0);
                                        let d = be32(&body, 12 + i * 8).unwrap_or(0);
                                        count += c as u64;
                                        match delta {
                                            None => delta = Some(d),
                                            Some(x) if x != d => constant = false,
                                            _ => {}
                                        }
                                    }
                                    stts = Some((count, if constant { delta } else { None }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if !is_video {
        return Ok(None);
    }
    let fps = match (timescale, duration, stts) {
        (Some(ts), _, Some((_, Some(delta)))) if delta > 0 => Some(ts as f64 / delta as f64),
        (Some(ts), Some(d), Some((count, None))) if d > 0 => {
            Some(count as f64 * ts as f64 / d as f64)
        }
        _ => None,
    };
    Ok(Some((fps, width, height)))
}

/// Where the XMP box is, if anywhere.
#[derive(Debug, Clone, PartialEq)]
pub enum XmpPlace {
    /// No XMP box; a new one goes at the end.
    None,
    /// The XMP box is the last box; it is replaced in place.
    Last { offset: u64 },
    /// An XMP box sits before other boxes. Changing its size would move
    /// everything after it, and the header's sample offsets with it.
    Elsewhere { offset: u64 },
}

/// The XMP packet in the file and where its box is. Reads only box headers
/// and the packet itself, so a 200 MB film costs a few seeks.
pub fn xmp_in<R: Read + Seek>(r: &mut R) -> Result<(Option<String>, XmpPlace)> {
    let end = r.seek(SeekFrom::End(0))?;
    let top = boxes(r, 0, end)?;
    if let Some(b) = top.iter().find(|b| &b.kind == b"ftyp") {
        if b.offset != 0 {
            return Err(anyhow!("ftyp is not the first box"));
        }
    } else {
        return Err(anyhow!("no ftyp box: not an MP4 or MOV file"));
    }
    for (i, b) in top.iter().enumerate() {
        if &b.kind != b"uuid" || b.size < b.header + 16 {
            continue;
        }
        let id = read_at(r, b.offset + b.header, 16)?;
        if id[..] != XMP_UUID[..] {
            continue;
        }
        let body_at = b.offset + b.header + 16;
        let body_len = b.offset + b.size - body_at;
        let packet = if body_at + body_len <= end {
            let raw = read_at(r, body_at, body_len as usize)?;
            Some(String::from_utf8_lossy(&raw).trim_end_matches('\0').to_string())
        } else {
            // Cut short: a write that never finished. There is no packet
            // to read, and the place is where the next one goes.
            None
        };
        let place = if i + 1 == top.len() {
            XmpPlace::Last { offset: b.offset }
        } else {
            XmpPlace::Elsewhere { offset: b.offset }
        };
        return Ok((packet, place));
    }
    Ok((None, XmpPlace::None))
}

/// The XMP packet the bytes carry, if any.
pub fn read_xmp(bytes: &[u8]) -> Result<Option<String>> {
    let mut r = std::io::Cursor::new(bytes);
    Ok(xmp_in(&mut r)?.0)
}

fn xmp_box(packet: &str) -> Vec<u8> {
    let body = packet.as_bytes();
    let size = 8 + 16 + body.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_be_bytes());
    out.extend_from_slice(b"uuid");
    out.extend_from_slice(&XMP_UUID);
    out.extend_from_slice(body);
    out
}

/// Where a new XMP box may go: the end of the file, or the start of the one
/// already there when it is last. Refuses the two shapes Pan does not
/// rewrite: an XMP box with other boxes after it, and a last box whose size
/// is "to the end of the file", which would swallow anything appended.
fn xmp_write_offset<R: Read + Seek>(r: &mut R) -> Result<u64> {
    let (_, place) = xmp_in(r)?;
    let end = r.seek(SeekFrom::End(0))?;
    match place {
        XmpPlace::Last { offset } => Ok(offset),
        XmpPlace::Elsewhere { offset } => Err(anyhow!(
            "the file already carries an XMP box at byte {offset} with other boxes after it. \
             Pan writes its XMP as the last box and does not move picture data to make room; \
             strip that box or move it to the end (exiftool can) and deliver the file again."
        )),
        XmpPlace::None => {
            let top = boxes(r, 0, end)?;
            if let Some(last) = top.last() {
                let mut h = [0u8; 4];
                r.seek(SeekFrom::Start(last.offset))?;
                r.read_exact(&mut h)?;
                if h == [0, 0, 0, 0] {
                    return Err(anyhow!(
                        "the file's last box ({:?}) has no size and runs to the end of the file, \
                         so nothing can be appended after it. Rewrite the file with sizes \
                         (ffmpeg does) and deliver it again.",
                        String::from_utf8_lossy(&last.kind)
                    ));
                }
                if last.offset + last.size > end {
                    return Err(anyhow!(
                        "the file's last box ({:?}) is cut short: it declares {} bytes but the \
                         file ends {} bytes in",
                        String::from_utf8_lossy(&last.kind),
                        last.size,
                        end - last.offset
                    ));
                }
            }
            Ok(end)
        }
    }
}

/// The bytes with `packet` as the file's XMP: appended as the last box, or
/// replacing the XMP box that is last already.
pub fn with_xmp(bytes: &[u8], packet: &str) -> Result<Vec<u8>> {
    let mut r = std::io::Cursor::new(bytes);
    let at = xmp_write_offset(&mut r)? as usize;
    let mut out = Vec::with_capacity(at + packet.len() + 32);
    out.extend_from_slice(&bytes[..at]);
    out.extend_from_slice(&xmp_box(packet));
    Ok(out)
}

/// Put `packet` into the file on disk as its XMP, touching only the tail:
/// the file is cut at the XMP box (or its end) and the new box written
/// there. A kill in the middle leaves a short last box, which the next
/// write replaces; the picture data before it is never touched.
pub fn write_xmp_in_place(path: &Path, packet: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let at = xmp_write_offset(&mut f)?;
    f.set_len(at)
        .with_context(|| format!("cut {} at byte {at}", path.display()))?;
    f.seek(SeekFrom::Start(at))?;
    f.write_all(&xmp_box(packet))
        .with_context(|| format!("write the XMP box into {}", path.display()))?;
    f.sync_all()?;
    Ok(())
}

/// The first frame of the file as a decoded picture, by way of ffmpeg
/// (`ffmpeg -i <file> -frames:v 1 -f image2pipe -vcodec png -`).
pub fn first_frame(path: &Path) -> Result<image::DynamicImage> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"])
        .output()
        .map_err(|e| {
            anyhow!("ffmpeg could not be run ({e}). Video thumbnails need ffmpeg on the PATH.")
        })?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(anyhow!(
            "ffmpeg gave no first frame for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    image::load_from_memory(&out.stdout).context("decode the frame ffmpeg wrote")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP4: &[u8] = include_bytes!("../tests/fixtures/video/tiny.mp4");
    const MOV: &[u8] = include_bytes!("../tests/fixtures/video/tiny.mov");

    #[test]
    fn mp4_and_mov_are_told_apart_by_their_brand_and_a_png_is_neither() {
        assert_eq!(sniff(MP4), Some("video/mp4"));
        assert_eq!(sniff(MOV), Some("video/quicktime"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"), None);
        assert_eq!(sniff(b"short"), None);
    }

    #[test]
    fn the_header_gives_duration_frame_rate_and_size() {
        let i = info(MP4).unwrap();
        assert_eq!(decimal(i.duration), "0.5");
        assert_eq!(i.frame_rate.map(decimal).as_deref(), Some("24"));
        assert_eq!((i.width, i.height), (Some(16), Some(16)));
        let i = info(MOV).unwrap();
        assert_eq!(decimal(i.duration), "0.5");
        assert_eq!(i.frame_rate.map(decimal).as_deref(), Some("30"));
        assert_eq!(decimal(29.97002997), "29.97");
        assert_eq!(decimal(5.041667), "5.042");
    }

    #[test]
    fn xmp_goes_at_the_end_is_read_back_and_is_replaced_not_stacked() {
        assert_eq!(read_xmp(MP4).unwrap(), None);
        let one = with_xmp(MP4, "<x:xmpmeta>one</x:xmpmeta>").unwrap();
        assert_eq!(read_xmp(&one).unwrap().as_deref(), Some("<x:xmpmeta>one</x:xmpmeta>"));
        assert_eq!(&one[..MP4.len()], MP4, "every original byte is where it was");
        let two = with_xmp(&one, "<x:xmpmeta>two, longer</x:xmpmeta>").unwrap();
        assert_eq!(read_xmp(&two).unwrap().as_deref(), Some("<x:xmpmeta>two, longer</x:xmpmeta>"));
        assert_eq!(&two[..MP4.len()], MP4);
        assert_eq!(two.len(), MP4.len() + 8 + 16 + "<x:xmpmeta>two, longer</x:xmpmeta>".len());
        // The header is still readable with the box behind it.
        assert_eq!(info(&two).unwrap(), info(MP4).unwrap());
    }

    #[test]
    fn an_xmp_box_before_other_boxes_is_refused() {
        // The XMP box spliced in right after ftyp.
        let ftyp_len = 32;
        let mut spliced = MP4[..ftyp_len].to_vec();
        spliced.extend_from_slice(&xmp_box("<x:xmpmeta>early</x:xmpmeta>"));
        spliced.extend_from_slice(&MP4[ftyp_len..]);
        assert_eq!(read_xmp(&spliced).unwrap().as_deref(), Some("<x:xmpmeta>early</x:xmpmeta>"));
        let err = format!("{:#}", with_xmp(&spliced, "<x/>").unwrap_err());
        assert!(err.contains("other boxes after it"), "{err}");
    }

    #[test]
    fn writing_in_place_touches_only_the_tail_and_a_short_box_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.mp4");
        std::fs::write(&p, MP4).unwrap();
        write_xmp_in_place(&p, "<x:xmpmeta>first</x:xmpmeta>").unwrap();
        let after = std::fs::read(&p).unwrap();
        assert_eq!(&after[..MP4.len()], MP4);
        assert_eq!(read_xmp(&after).unwrap().as_deref(), Some("<x:xmpmeta>first</x:xmpmeta>"));
        // A write cut short by a kill: the last box declares more than is there.
        std::fs::write(&p, &after[..after.len() - 5]).unwrap();
        let mut f = std::fs::File::open(&p).unwrap();
        let (packet, place) = xmp_in(&mut f).unwrap();
        assert_eq!(packet, None, "a cut-short box carries no packet");
        assert_eq!(place, XmpPlace::Last { offset: MP4.len() as u64 });
        write_xmp_in_place(&p, "<x:xmpmeta>second</x:xmpmeta>").unwrap();
        let fixed = std::fs::read(&p).unwrap();
        assert_eq!(read_xmp(&fixed).unwrap().as_deref(), Some("<x:xmpmeta>second</x:xmpmeta>"));
        assert_eq!(&fixed[..MP4.len()], MP4);
    }

    #[test]
    fn the_first_frame_comes_out_of_ffmpeg_at_the_picture_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.mp4");
        std::fs::write(&p, MP4).unwrap();
        let img = first_frame(&p).unwrap();
        assert_eq!((img.width(), img.height()), (16, 16));
    }
}
