// ./core/src/metadata.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Writes mutable user data (rating, play count) back into audio file tags.
//!
//! Ratings use the MusicBee-style popularimeter so they interoperate with
//! Amarok/Clementine/Strawberry and other players that read POPM frames.
//! opus/flac/ogg/vorbis-comment formats carry the star rating; the play
//! counter is best-effort and only persisted where the format supports it.

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::items::popularimeter::{Popularimeter, StarRating};
use lofty::tag::{ItemKey, Tag};
use std::path::{Path, PathBuf};

/// Persist `rating` (0-5) and `play_count` into the file's tag at `path`.
/// A rating of 0 removes the popularimeter entry. Errors are returned but
/// the caller treats them as non-fatal (the catalog is always updated).
///
/// The write goes to a temporary file in the same directory, is verified, and
/// only then atomically renamed over the original. This guarantees the
/// original is never left corrupted: lofty's OGG writer silently drops all
/// audio (while still reporting success) when the setup header shares a page
/// with the first audio packet, so a bad result is discarded and the original
/// is kept.
pub fn write_rating_and_play_count(path: &Path, rating: u8, play_count: u32) -> Result<(), String> {
    let mut tagged_file = Probe::open(path)
        .map_err(|e| format!("open {path:?}: {e}"))?
        .read()
        .map_err(|e| format!("read {path:?}: {e}"))?;

    let Some(tag) = primary_tag_mut(&mut tagged_file) else {
        // No tag to write to; nothing we can do but report success.
        return Ok(());
    };

    if rating == 0 {
        tag.remove_key(ItemKey::Popularimeter);
    } else {
        let star = star_rating(rating);
        let pop = Popularimeter::musicbee(star, play_count as u64);
        tag.insert_text(ItemKey::Popularimeter, pop.to_string());
    }

    let original_len = std::fs::metadata(path)
        .map_err(|e| format!("stat {path:?}: {e}"))?
        .len();

    let tmp = temp_path_for(path);
    // lofty's `save_to_path` edits a file in place: it reads the existing
    // structure to preserve the audio, then rewrites. So the temp must be a
    // full copy of the original, not an empty file.
    std::fs::copy(path, &tmp).map_err(|e| format!("copy {path:?} -> {tmp:?}: {e}"))?;
    if let Err(e) = tagged_file.save_to_path(&tmp, WriteOptions::default()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("save {tmp:?}: {e}"));
    }

    if let Err(e) = verify_write(&tmp, original_len) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename {tmp:?} -> {path:?}: {e}"));
    }

    Ok(())
}

/// A temporary file in the same directory as `path`, so the final rename is
/// atomic (same filesystem). The original extension is preserved so the
/// format can still be identified from the name (lofty's `Probe` guesses the
/// file type from the extension).
fn temp_path_for(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut name = format!(".{stem}.currant-tmp");
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        name.push('.');
        name.push_str(ext);
    }
    dir.join(name)
}

/// Confirm the written file is still a valid, complete audio file before it is
/// allowed to replace the original.
///
/// A rating/play-count write changes the file size by only a few dozen bytes.
/// A writer that dropped the audio shrinks it by orders of magnitude, so a size
/// regression is a reliable corruption signal that a plain re-parse would miss
/// (an audio-less OGG still parses fine).
fn verify_write(written: &Path, original_len: u64) -> Result<(), String> {
    Probe::open(written)
        .map_err(|e| format!("verify {written:?}: open failed: {e}"))?
        .read()
        .map_err(|e| format!("verify {written:?}: no longer parses: {e}"))?;

    let new_len = std::fs::metadata(written)
        .map_err(|e| format!("stat {written:?}: {e}"))?
        .len();

    const MAX_SHRINK: u64 = 4096;
    if original_len > new_len && original_len - new_len > MAX_SHRINK {
        return Err(format!(
            "verify {written:?}: shrank from {original_len} to {new_len} bytes; \
             writer dropped audio, original left untouched"
        ));
    }

    Ok(())
}

/// Prefer the primary tag; fall back to the first tag of any type.
fn primary_tag_mut(tagged_file: &mut lofty::file::TaggedFile) -> Option<&mut Tag> {
    if tagged_file.primary_tag_mut().is_some() {
        tagged_file.primary_tag_mut()
    } else {
        tagged_file.first_tag_mut()
    }
}

fn star_rating(rating: u8) -> StarRating {
    match rating {
        1 => StarRating::One,
        2 => StarRating::Two,
        3 => StarRating::Three,
        4 => StarRating::Four,
        _ => StarRating::Five,
    }
}
