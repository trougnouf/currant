// ./core/src/scanner.rs
use crate::model::Track;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey}; // <-- Added ItemKey here!
use std::path::Path;
use walkdir::WalkDir;

pub fn scan_directory(dir: &Path) -> Vec<Track> {
    let mut tracks = Vec::new();
    let mut id_counter = 0;

    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if let Ok(tagged_file) = Probe::open(path).and_then(|p| p.read()) {
            let tag = tagged_file
                .primary_tag()
                .or_else(|| tagged_file.first_tag());
            let properties = tagged_file.properties();

            let title = tag
                .and_then(|t| t.title().as_deref().map(String::from))
                .unwrap_or_else(|| path.file_stem().unwrap().to_string_lossy().to_string());

            let artist = tag
                .and_then(|t| t.artist().as_deref().map(String::from))
                .unwrap_or_else(|| "Unknown Artist".to_string());

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

            // FIXED: Fetching Year properly for Lofty 0.25+
            let year = tag
                .and_then(|t| t.get_string(ItemKey::Year))
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);

            let duration_secs = properties.duration().as_secs() as u32;

            tracks.push(Track {
                id: id_counter.to_string(),
                path: path.to_string_lossy().to_string(),
                title,
                artist,
                album,
                genre,
                comment,
                track_number,
                year,
                duration_secs,
                rating: 0,
                play_count: 0,
                last_played: None,
            });
            id_counter += 1;
        }
    }
    tracks
}
