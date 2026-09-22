// File: ./core/tests/metadata.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Regression tests for the rating/play-count tag write-back.
//!
//! lofty's OGG writer silently drops all audio (while still returning success)
//! when the setup header shares a page with the first audio packet. These tests
//! pin `write_rating_and_play_count` against both layouts: a safe-layout file is
//! tagged in place, and a trigger-layout file is left untouched (the write is
//! rejected and the original preserved byte-for-byte).
//!
//! The fixtures are small OGG files. `ogg_safe.ogg` has the setup header ending
//! on a page boundary; `ogg_trigger.ogg` was re-paginated so the setup header
//! ends mid-page, sharing a page with the first audio packet.

use lofty::file::{AudioFile, TaggedFileExt};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// Copy a fixture into the temp dir so the original is never touched.
fn temp_copy(name: &str) -> PathBuf {
    let dst = std::env::temp_dir().join(format!("currant-meta-test-{}-{name}", std::process::id()));
    std::fs::copy(fixture(name), &dst).expect("copy fixture to temp dir");
    dst
}

// The temp file the writer creates next to `path` (mirrors metadata::temp_path_for).
fn expected_temp_path(path: &Path) -> PathBuf {
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

// Count the OGG pages in a file. A writer that drops the audio collapses this.
fn ogg_page_count(path: &Path) -> usize {
    let data = std::fs::read(path).unwrap_or_default();
    let mut pos = 0;
    let mut pages = 0;
    while pos + 27 <= data.len() {
        if &data[pos..pos + 4] != b"OggS" {
            break;
        }
        let nsegs = data[pos + 26] as usize;
        let seg_table = pos + 27;
        if seg_table + nsegs > data.len() {
            break;
        }
        let body: usize = data[seg_table..seg_table + nsegs]
            .iter()
            .map(|&b| b as usize)
            .sum();
        let end = seg_table + nsegs + body;
        pages += 1;
        if end >= data.len() {
            break;
        }
        pos = end;
    }
    pages
}

// Parse with lofty and return the audio duration (0 if the audio was dropped).
fn audio_duration(path: &Path) -> std::time::Duration {
    let probe = lofty::probe::Probe::open(path).expect("parse file");
    probe.read().expect("read file").properties().duration()
}

// A safe-layout OGG is tagged in place: the write succeeds, the audio survives,
// and the popularimeter is readable back.
#[test]
fn safe_layout_is_tagged_in_place() {
    let path = temp_copy("ogg_safe.ogg");
    let before = std::fs::metadata(&path).unwrap().len();
    let before_pages = ogg_page_count(&path);

    let res = currant_core::metadata::write_rating_and_play_count(&path, 4, 12);
    assert!(res.is_ok(), "safe layout must be writable: {res:?}");

    let after = std::fs::metadata(&path).unwrap().len();
    assert!(after >= before, "file must not shrink: {before} -> {after}");
    assert!(
        ogg_page_count(&path) >= before_pages,
        "audio pages must survive (was {before_pages})"
    );
    assert!(
        audio_duration(&path) > std::time::Duration::ZERO,
        "audio must survive"
    );

    let probe = lofty::probe::Probe::open(&path).expect("parse file");
    let tf = probe.read().expect("read file");
    let tag = tf.first_tag().expect("tag present");
    let popm = tag
        .get_string(lofty::tag::ItemKey::Popularimeter)
        .expect("popularimeter present");
    assert_eq!(popm, "MusicBee|4|12");

    let _ = std::fs::remove_file(&path);
}

// A trigger-layout OGG must be left untouched: the write is rejected (lofty
// would have dropped all audio) and the original is preserved byte-for-byte.
#[test]
fn trigger_layout_is_left_untouched() {
    let path = temp_copy("ogg_trigger.ogg");
    let before = std::fs::metadata(&path).unwrap().len();
    let before_bytes = std::fs::read(&path).unwrap();
    let before_pages = ogg_page_count(&path);

    let res = currant_core::metadata::write_rating_and_play_count(&path, 4, 12);
    assert!(res.is_err(), "trigger layout must be rejected: {res:?}");

    let after_bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        after_bytes, before_bytes,
        "original must be preserved byte-for-byte"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    assert_eq!(
        ogg_page_count(&path),
        before_pages,
        "page count must not collapse"
    );
    assert!(
        audio_duration(&path) > std::time::Duration::ZERO,
        "audio must survive"
    );
    assert!(
        !expected_temp_path(&path).exists(),
        "no temp file may be left behind"
    );

    let _ = std::fs::remove_file(&path);
}
