// ./core/src/scanner.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Library scanner. Walks one or more roots, reads audio metadata with lofty,
//! and upserts into the catalog. Incremental: files whose mtime is unchanged
//! since the last scan are skipped, and files that have vanished are pruned.

use crate::model::Track;
use crate::store::LibraryStore;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::items::popularimeter::Popularimeter;
use lofty::tag::{Accessor, ItemKey};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;
use walkdir::WalkDir;

/// Audio extensions we know how to read. opus/flac/mp3/ogg are the priority;
/// wav/m4a/aac are best-effort.
const EXTENSIONS: &[&str] = &["mp3", "flac", "ogg", "oga", "opus", "wav", "m4a", "aac"];

/// Live scan progress, updated with atomics so a UI thread can read it
/// without locking. Shared via `Arc<ScanProgress>`.
#[derive(Debug, Default)]
pub struct ScanProgress {
    pub scanned: AtomicU64,
    pub added: AtomicU64,
    pub updated: AtomicU64,
    pub unchanged: AtomicU64,
    pub removed: AtomicU64,
    pub errors: AtomicU64,
    pub done: AtomicBool,
}

impl ScanProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A snapshot of the current counts. Safe to call from any thread.
    pub fn snapshot(&self) -> ScanReport {
        ScanReport {
            scanned: self.scanned.load(Ordering::Relaxed),
            added: self.added.load(Ordering::Relaxed),
            updated: self.updated.load(Ordering::Relaxed),
            unchanged: self.unchanged.load(Ordering::Relaxed),
            removed: self.removed.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

/// Summary of one scan pass.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub scanned: u64,
    pub added: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub removed: u64,
    pub errors: u64,
}

/// Scan `roots`, updating `store` incrementally. Live counts are written to
/// `progress` so a UI can show feedback while the scan runs.
pub fn scan_roots(store: &LibraryStore, roots: &[String], progress: &ScanProgress) {
    let existing = store.paths_with_mtime();
    let mut keep: HashSet<String> = HashSet::new();

    for root in roots {
        let root = Path::new(root);
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| !is_hidden(e.path()))
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if !is_audio(path) {
                continue;
            }
            progress.scanned.fetch_add(1, Ordering::Relaxed);
            let path_str = path.to_string_lossy().to_string();
            keep.insert(path_str.clone());

            let mtime = file_mtime(path).unwrap_or(0);
            let prev = existing.get(&path_str).copied();
            if prev == Some(mtime) {
                progress.unchanged.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            match read_track(path, mtime) {
                Ok(track) => {
                    if store.upsert_track(&track).is_ok() {
                        match prev {
                            Some(_) => {
                                progress.updated.fetch_add(1, Ordering::Relaxed);
                            }
                            None => {
                                progress.added.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else {
                        progress.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    progress.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    let removed = store.prune(&keep) as u64;
    progress.removed.store(removed, Ordering::Relaxed);
    progress.done.store(true, Ordering::Relaxed);
}

/// True if `path` looks like an audio file we support.
pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// True if the file or directory is hidden (starts with a dot).
fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

fn file_mtime(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let secs = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;
    Some(secs.as_secs() as i64)
}

fn read_track(path: &Path, mtime: i64) -> Result<Track, ()> {
    let tagged_file = Probe::open(path).map_err(|_| ())?.read().map_err(|_| ())?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());
    let properties = tagged_file.properties();

    let title = tag
        .and_then(|t| t.title().as_deref().map(String::from))
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "Unknown".to_string());

    let artist = tag
        .and_then(|t| t.artist().as_deref().map(String::from))
        .unwrap_or_else(|| "Unknown Artist".to_string());

    let album_artist = tag
        .and_then(|t| t.get_string(ItemKey::AlbumArtist))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| artist.clone());

    let album = tag
        .and_then(|t| t.album().as_deref().map(String::from))
        .unwrap_or_else(|| "Unknown Album".to_string());

    let genre = tag
        .and_then(|t| t.genre().as_deref().map(String::from))
        .unwrap_or_default();

    let comment = tag
        .and_then(|t| t.comment().as_deref().map(String::from))
        .unwrap_or_default();

    let track_number = tag.and_then(|t| t.track()).unwrap_or(0);

    let year = tag
        .and_then(|t| t.get_string(ItemKey::Year))
        .or_else(|| tag.and_then(|t| t.get_string(ItemKey::RecordingDate)))
        .and_then(|s| {
            let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u32>().ok()
        })
        .unwrap_or(0);

    // Read an existing rating when present so a rescan does not clobber
    // ratings the user set elsewhere. Uses lofty's ratings() which handles
    // all formats (ID3v2 POPM, Vorbis RATING, MP4 rate) and provider scales
    // (MusicBee, WMP, Picard, default).
    let rating = tag
        .and_then(|t| t.ratings().next())
        .map(|p: Popularimeter<'_>| p.rating() as u8)
        .unwrap_or(0);

    // Lofty doesn't recognize FMPS_RATING/FMPS_PLAYCOUNT (used by
    // Strawberry/Clementine/Amarok in Vorbis comments). Parse the raw
    // comment block as a fallback for FLAC/OGG/Opus files.
    let (fmps_rating, fmps_play_count) = crate::vorbis_ext::read_fmps_fields(path);
    let rating = if rating > 0 { rating } else { fmps_rating };
    let play_count = if fmps_play_count > 0 {
        fmps_play_count
    } else {
        0
    };

    let duration_secs = properties.duration().as_secs() as u32;

    Ok(Track {
        id: path_id(path),
        path: path.to_string_lossy().to_string(),
        title,
        artist,
        album_artist,
        album,
        genre,
        comment,
        track_number,
        year,
        duration_secs,
        rating,
        play_count,
        last_played: None,
        file_mtime: mtime,
    })
}

/// A stable, deterministic id for a file: FNV-1a 64-bit of the canonical path.
pub fn path_id(path: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in path.to_string_lossy().as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Resolve the default music roots (XDG user directories) for first-run scans.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(m) = dirs::audio_dir() {
        roots.push(m);
    }
    if let Some(h) = dirs::home_dir() {
        let candidate = h.join("Music");
        if candidate.is_dir() && !roots.contains(&candidate) {
            roots.push(candidate);
        }
    }
    roots
}
