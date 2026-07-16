//! Minimal numpy `.npy` reader/writer for L2-normalized float32 1-D vectors.
//!
//! Lifted intact from Pool (`pool/src/npy.rs`) — the proven sidecar codec.
//! Sidecar vectors are saved with `np.save(path, vec)` where vec is `float32`,
//! L2-normalized, 1-D. We only handle that exact case — no broadcasting, no
//! fortran order, no object arrays.
//!
//! Format (numpy NEP-1, v1.0):
//!   bytes 0..6:  magic = b"\x93NUMPY"
//!   byte  6:     major version (1)
//!   byte  7:     minor version (0)
//!   bytes 8..10: little-endian u16 header_len
//!   bytes 10..10+header_len: ASCII Python dict literal, e.g.
//!     "{'descr': '<f4', 'fortran_order': False, 'shape': (2048,), }    \n"
//!   then: raw little-endian float32 bytes, shape[0] of them.
//!
//! We accept descr ∈ {'<f4', '|f4', 'f4'} (little-endian float32) and reject
//! everything else. Shape must be a 1-tuple.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::Path;

const MAGIC: &[u8] = b"\x93NUMPY";

/// Write a 1-D float32 vector as a numpy `.npy` (v1.0), ATOMICALLY.
///
/// The inverse of `read_f32_1d` — same exact format the reader accepts
/// (`<f4`, C-order, 1-D shape tuple).
///
/// Atomic via temp-file-in-the-same-dir + rename, so a concurrent reader never
/// sees a half-written file — `read_f32_1d` bails on "payload short", which
/// would otherwise drop the object. The vector is written verbatim: callers
/// pass a 1-D, L2-normalized vector — this fn does NOT normalize or reshape.
pub fn write_f32_1d(path: &Path, vec: &[f32]) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }

    let header = format!(
        "{{'descr': '<f4', 'fortran_order': False, 'shape': ({},), }}",
        vec.len()
    );
    // Pad so (10 + header_len) % 64 == 0 (numpy spec) and the header ends in \n.
    let mut padded = header;
    while (10 + padded.len() + 1) % 64 != 0 {
        padded.push(' ');
    }
    padded.push('\n');

    // Write to a temp sibling, then rename — rename is atomic within a dir, so
    // a reader sees either the old file or the complete new one, never a short
    // read.
    let tmp_path = path.with_extension("npy.tmp");
    {
        let mut f = fs::File::create(&tmp_path)
            .with_context(|| format!("create {}", tmp_path.display()))?;
        f.write_all(MAGIC).context("write npy magic")?;
        f.write_all(&[1u8, 0u8]).context("write npy version")?;
        let hl = padded.len() as u16;
        f.write_all(&hl.to_le_bytes()).context("write npy header len")?;
        f.write_all(padded.as_bytes()).context("write npy header")?;
        for v in vec {
            f.write_all(&v.to_le_bytes()).context("write npy payload")?;
        }
        f.flush().context("flush npy temp")?;
    }
    fs::rename(&tmp_path, path).with_context(|| {
        format!("rename {} -> {}", tmp_path.display(), path.display())
    })?;
    Ok(())
}

/// Read a `.npy` file and return the 1-D float32 vector.
///
/// Errors loudly on:
///   - missing magic / wrong version
///   - non-f32 dtype
///   - fortran-order arrays
///   - non-1-D shape
pub fn read_f32_1d(path: &Path) -> Result<Vec<f32>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;

    if bytes.len() < 10 {
        bail!(".npy file too short: {} bytes", bytes.len());
    }
    if &bytes[0..6] != MAGIC {
        bail!(".npy magic mismatch (expected \\x93NUMPY)");
    }
    let major = bytes[6];
    let minor = bytes[7];
    let (header_start, header_len) = match (major, minor) {
        (1, 0) => {
            let hl = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
            (10, hl)
        }
        (2, 0) | (3, 0) => {
            if bytes.len() < 12 {
                bail!(".npy v2/v3 header truncated");
            }
            let hl = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
            (12, hl)
        }
        _ => bail!(".npy unsupported version {major}.{minor}"),
    };

    if bytes.len() < header_start + header_len {
        bail!(".npy header truncated");
    }
    let header_bytes = &bytes[header_start..header_start + header_len];
    let header = std::str::from_utf8(header_bytes)
        .context("npy header is not utf8")?;

    let descr = extract_dict_value(header, "descr")?;
    if !matches!(descr.trim_matches('\''), "<f4" | "|f4" | "f4") {
        bail!(".npy dtype not float32 (got {descr})");
    }

    let fortran = extract_dict_value(header, "fortran_order")?;
    if fortran.trim() == "True" {
        bail!(".npy fortran_order arrays not supported");
    }

    let shape_str = extract_dict_value(header, "shape")?;
    let dims = parse_shape_tuple(&shape_str)?;
    if dims.len() != 1 {
        bail!(".npy shape must be 1-D, got {dims:?}");
    }
    let n = dims[0];

    let payload = &bytes[header_start + header_len..];
    let expected_bytes = n * 4;
    if payload.len() < expected_bytes {
        bail!(
            ".npy payload short: expected {} bytes ({} f32s), got {}",
            expected_bytes,
            n,
            payload.len()
        );
    }

    let mut out = Vec::with_capacity(n);
    for chunk in payload[..expected_bytes].chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

/// Extract a value from the npy header's Python-dict-like string. We don't
/// parse Python literals — just find `'key':` and read until the next comma
/// or closing brace at the top level. The values we care about are short.
fn extract_dict_value(header: &str, key: &str) -> Result<String> {
    let needle = format!("'{key}':");
    let start = header
        .find(&needle)
        .ok_or_else(|| anyhow!("npy header missing key '{key}'"))?
        + needle.len();
    let rest = &header[start..];
    let mut depth = 0i32;
    let mut end = rest.len();
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => depth -= 1,
            ',' | '}' if depth == 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    Ok(rest[..end].trim().to_string())
}

fn parse_shape_tuple(s: &str) -> Result<Vec<usize>> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| anyhow!("npy shape not a tuple: {s}"))?;
    let mut dims = Vec::new();
    for piece in inner.split(',') {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        let n: usize = p
            .parse()
            .map_err(|e| anyhow!("npy shape parse error: {e} (piece {p:?})"))?;
        dims.push(n);
    }
    Ok(dims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/v.npy"); // mkdir -p exercised
        let v: Vec<f32> = (0..2048).map(|i| (i as f32) * 0.0007 - 0.5).collect();
        write_f32_1d(&path, &v).unwrap();
        let got = read_f32_1d(&path).unwrap();
        assert_eq!(got.len(), 2048);
        for (a, b) in v.iter().zip(got.iter()) {
            assert!((a - b).abs() < 1e-7, "value drift: {a} vs {b}");
        }
    }

    #[test]
    fn write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.npy");
        write_f32_1d(&path, &[1.0, 2.0, 3.0]).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("npy.tmp").exists(), "temp file leaked");
    }

    #[test]
    fn reads_python_numpy_compatible_file() {
        // The exact byte layout numpy 1.26+ writes for a 4-element f32.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.npy");
        write_f32_1d(&path, &[1.0, 2.0, 3.0, 4.0]).unwrap();
        let got = read_f32_1d(&path).unwrap();
        assert_eq!(got, vec![1.0, 2.0, 3.0, 4.0]);
    }
}
