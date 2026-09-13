use crate::matcher::SearchExpr;
use crate::model::{SmartPlaylist, SortPreset, Track};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct LibraryStore {
    pub tracks: HashMap<String, Track>,
    pub smart_playlists: Vec<SmartPlaylist>,
}

impl LibraryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluates the SearchExpr against the entire library.
    /// Returns references to avoid cloning strings, making it lightning fast.
    pub fn filter<'a>(&'a self, expr: &SearchExpr, sort: SortPreset) -> Vec<&'a Track> {
        // If the query is completely empty, skip the matching logic for speed
        let mut results: Vec<&Track> = if let SearchExpr::Term(s) = expr {
            if s.is_empty() {
                self.tracks.values().collect()
            } else {
                self.tracks.values().filter(|t| expr.matches(t)).collect()
            }
        } else {
            self.tracks.values().filter(|t| expr.matches(t)).collect()
        };

        match sort {
            SortPreset::ArtistAlbumTrack => {
                results.sort_unstable_by(|a, b| {
                    a.artist
                        .cmp(&b.artist)
                        .then_with(|| a.album.cmp(&b.album))
                        .then_with(|| a.track_number.cmp(&b.track_number))
                });
            }
            SortPreset::YearDesc => results.sort_unstable_by(|a, b| b.year.cmp(&a.year)),
            SortPreset::MostPlayed => {
                results.sort_unstable_by(|a, b| b.play_count.cmp(&a.play_count))
            }
            SortPreset::HighestRated => results.sort_unstable_by(|a, b| b.rating.cmp(&a.rating)),
            SortPreset::Random => {
                // O(1) shuffle using fastrand
                fastrand::shuffle(&mut results);
            }
        }

        results
    }

    pub fn add_track(&mut self, track: Track) {
        self.tracks.insert(track.id.clone(), track);
    }
}
