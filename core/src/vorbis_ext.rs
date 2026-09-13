// ./core/src/vorbis_ext.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Extended Vorbis comment reader for fields that lofty doesn't map.
//!
//! Strawberry/Clementine/Amarok store ratings as `FMPS_RATING` (0.0-1.0
//! float) and play counts as `FMPS_PLAYCOUNT` in Vorbis comments. Lofty
//! only recognizes the `RATING` key (integer 0-100), so `FMPS_*` fields
//! are silently dropped during its VorbisComments → Tag conversion.
//!
//! This module parses the raw Vorbis comment block directly from FLAC and
//! OGG containers to recover those fields. No external dependencies.

use std::path::Path;

/// Read `FMPS_RATING` (0.0-1.0) and `FMPS_PLAYCOUNT` from the raw Vorbis
/// comments in a FLAC or OGG file. Returns `(rating, play_count)` or
/// `(0, 0)` if not found or unparseable.
pub fn read_fmps_fields(path: &Path) -> (u8, u32) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let data = match read_file_head(path, 512 * 1024) {
        Some(d) => d,
        None => return (0, 0),
    };

    let comments = match ext.as_str() {
        "flac" => parse_flac(&data),
        "ogg" | "oga" | "opus" => parse_ogg(&data),
        _ => return (0, 0),
    };

    let mut rating = 0u8;
    let mut play_count = 0u32;

    for (key, value) in comments {
        if key.eq_ignore_ascii_case("FMPS_RATING") {
            if let Ok(f) = value.parse::<f64>() {
                rating = (f * 5.0).round().clamp(0.0, 5.0) as u8;
            }
        } else if key.eq_ignore_ascii_case("FMPS_PLAYCOUNT")
            && let Ok(n) = value.parse::<u32>()
        {
            play_count = n;
        }
    }

    (rating, play_count)
}

/// Read up to `max_bytes` from the start of a file.
fn read_file_head(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max_bytes];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

// --- FLAC ---

/// Parse Vorbis comments from a FLAC file's metadata blocks.
fn parse_flac(data: &[u8]) -> Vec<(String, String)> {
    if data.len() < 4 || &data[..4] != b"fLaC" {
        return Vec::new();
    }

    let mut pos = 4;
    loop {
        if pos + 4 > data.len() {
            break;
        }
        let header = data[pos];
        let block_type = header & 0x7f;
        let is_last = (header & 0x80) != 0;
        let block_len = (u32::from(data[pos + 1]) << 16
            | u32::from(data[pos + 2]) << 8
            | u32::from(data[pos + 3])) as usize;
        pos += 4;

        if block_type == 4 && pos + block_len <= data.len() {
            return parse_vorbis_comment_data(&data[pos..pos + block_len]);
        }

        pos += block_len;
        if is_last {
            break;
        }
    }

    Vec::new()
}

// --- OGG (Vorbis / Opus) ---

/// Parse Vorbis comments from an OGG file's comment header page.
fn parse_ogg(data: &[u8]) -> Vec<(String, String)> {
    let mut pos = 0;

    while pos + 27 <= data.len() {
        if &data[pos..pos + 4] != b"OggS" {
            // Not aligned — skip forward. This is inefficient but only runs
            // on the first few KB of the file, not the audio data.
            pos += 1;
            continue;
        }

        let num_segments = data[pos + 26] as usize;
        let seg_table_start = pos + 27;
        if seg_table_start + num_segments > data.len() {
            break;
        }
        let segment_table = &data[seg_table_start..seg_table_start + num_segments];

        let page_seq = u32::from_le_bytes([
            data[pos + 18],
            data[pos + 19],
            data[pos + 20],
            data[pos + 21],
        ]);

        let data_start = seg_table_start + num_segments;
        let total_len: usize = segment_table.iter().map(|&s| s as usize).sum();
        if data_start + total_len > data.len() {
            break;
        }
        let segment_data = &data[data_start..data_start + total_len];

        // The comment header is page sequence 1 (page 0 is the identification
        // header). Skip the codec-specific preamble:
        //   Vorbis: 1 byte (type 3) + 6 bytes "vorbis" = 7 bytes
        //   Opus:   8 bytes "OpusTags"
        if page_seq == 1 {
            if segment_data.len() > 7 && &segment_data[1..7] == b"vorbis" {
                return parse_vorbis_comment_data(&segment_data[7..]);
            } else if segment_data.len() >= 8 && &segment_data[..8] == b"OpusTags" {
                return parse_vorbis_comment_data(&segment_data[8..]);
            }
        }

        pos = data_start + total_len;
    }

    Vec::new()
}

// --- Vorbis comment block parser ---

/// Parse the standard Vorbis comment block format:
///   vendor_length (u32 LE) | vendor_string | count (u32 LE) | comments...
/// Each comment is: length (u32 LE) | "KEY=VALUE" (UTF-8).
fn parse_vorbis_comment_data(data: &[u8]) -> Vec<(String, String)> {
    let mut pos = 0;

    // Vendor string
    let vendor_len = match read_u32_le(data, pos) {
        Some(n) => n,
        None => return Vec::new(),
    };
    pos += 4 + vendor_len as usize;

    // Comment count
    let count = match read_u32_le(data, pos) {
        Some(n) => n,
        None => return Vec::new(),
    };
    pos += 4;

    let mut comments = Vec::new();
    for _ in 0..count {
        let len = match read_u32_le(data, pos) {
            Some(n) => n,
            None => break,
        };
        pos += 4;

        let end = pos + len as usize;
        if end > data.len() {
            break;
        }

        let comment = String::from_utf8_lossy(&data[pos..end]);
        pos = end;

        if let Some((key, value)) = comment.split_once('=') {
            comments.push((key.to_string(), value.to_string()));
        }
    }

    comments
}

fn read_u32_le(data: &[u8], pos: usize) -> Option<u32> {
    if pos + 4 > data.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flac_vorbis_comments() {
        // Minimal FLAC: "fLaC" + STREAMINFO (34 bytes, is_last) won't work
        // because we need the Vorbis comment block. Build a two-block header:
        // STREAMINFO (not last, 34 bytes) + Vorbis comment (is_last).
        let mut buf = Vec::new();
        buf.extend_from_slice(b"fLaC");

        // STREAMINFO block: type=0, not last, length=34
        buf.push(0x00); // type 0, not last
        buf.push(0x00);
        buf.push(0x00);
        buf.push(34); // length
        buf.extend_from_slice(&[0u8; 34]); // dummy STREAMINFO

        // Vorbis comment block: type=4, is last
        let mut vc = Vec::new();
        // vendor
        vc.extend_from_slice(&5u32.to_le_bytes());
        vc.extend_from_slice(b"test\x00");
        // 2 comments
        vc.extend_from_slice(&2u32.to_le_bytes());
        // FMPS_RATING=0.8
        let c1 = b"FMPS_RATING=0.8";
        vc.extend_from_slice(&(c1.len() as u32).to_le_bytes());
        vc.extend_from_slice(c1);
        // FMPS_PLAYCOUNT=42
        let c2 = b"FMPS_PLAYCOUNT=42";
        vc.extend_from_slice(&(c2.len() as u32).to_le_bytes());
        vc.extend_from_slice(c2);

        buf.push(0x80 | 4); // type 4, is last
        buf.push((vc.len() >> 16) as u8);
        buf.push((vc.len() >> 8) as u8);
        buf.push(vc.len() as u8);
        buf.extend_from_slice(&vc);

        let comments = parse_flac(&buf);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].0, "FMPS_RATING");
        assert_eq!(comments[0].1, "0.8");
        assert_eq!(comments[1].0, "FMPS_PLAYCOUNT");
        assert_eq!(comments[1].1, "42");
    }
}
