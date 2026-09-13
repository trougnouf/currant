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
use std::path::Path;

/// Persist `rating` (0-5) and `play_count` into the file's tag at `path`.
/// A rating of 0 removes the popularimeter entry. Errors are returned but
/// the caller treats them as non-fatal (the catalog is always updated).
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

    tagged_file
        .save_to_path(path, WriteOptions::default())
        .map_err(|e| format!("save {path:?}: {e}"))
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
