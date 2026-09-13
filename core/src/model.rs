// ./core/src/model.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Core data model: track, search fields/operators, sort presets and the
//! unified intent enum shared by every frontend (TUI and Android).

use serde::{Deserialize, Serialize};

/// A single audio file in the library catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub genre: String,
    pub comment: String,

    pub track_number: u32,
    pub year: u32,
    pub duration_secs: u32,

    /// Mutable user-data, written back to file tags and the catalog.
    pub rating: u8, // 0 to 5
    pub play_count: u32,
    pub last_played: Option<i64>,

    /// File mtime (unix seconds) used for incremental rescans.
    pub file_mtime: i64,
}

/// A searchable library field. `All` is the free-text fallback that
/// matches against title, artist and album together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Title,
    Artist,
    Album,
    Genre,
    Comment,
    Year,
    Duration,
    Rating,
    PlayCount,
    All,
}

impl Field {
    /// The SQL column name backing this field in the `tracks` table.
    pub fn column(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Artist => "artist",
            Field::Album => "album",
            Field::Genre => "genre",
            Field::Comment => "comment",
            Field::Year => "year",
            Field::Duration => "duration_secs",
            Field::Rating => "rating",
            Field::PlayCount => "play_count",
            Field::All => "",
        }
    }

    /// The pre-folded shadow column used for accent-insensitive search.
    /// Returns `""` for numeric fields and `All` (handled separately).
    pub fn fold_column(self) -> &'static str {
        match self {
            Field::Title => "title_fold",
            Field::Artist => "artist_fold",
            Field::Album => "album_fold",
            Field::Genre => "genre_fold",
            Field::Comment => "comment_fold",
            _ => "",
        }
    }

    /// True for text fields compared with `LIKE`/`=`, false for numeric ones.
    pub fn is_text(self) -> bool {
        matches!(
            self,
            Field::Title
                | Field::Artist
                | Field::Album
                | Field::Genre
                | Field::Comment
                | Field::All
        )
    }

    /// Human-friendly aliases understood by the query parser (e.g. `ar:`, `artist:`).
    pub fn from_alias(s: &str) -> Option<Field> {
        Some(match s {
            "t" | "title" => Field::Title,
            "ar" | "artist" => Field::Artist,
            "al" | "album" => Field::Album,
            "g" | "genre" => Field::Genre,
            "c" | "comment" => Field::Comment,
            "y" | "year" => Field::Year,
            "d" | "dur" | "duration" | "length" => Field::Duration,
            "*" | "r" | "rating" => Field::Rating,
            "p" | "plays" | "play_count" | "playcount" | "count" => Field::PlayCount,
            _ => return None,
        })
    }
}

/// Comparison operators supported by the query language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Contains,
    NotContains,
    Eq,
    NotEq,
    Gt,
    Ge,
    Lt,
    Le,
}

impl CmpOp {
    /// Parse a leading operator prefix (`>=`, `<=`, `!=`, `>`, `<`, `=`).
    /// Returns the operator and the remaining value.
    pub fn split_prefix(input: &str) -> (CmpOp, &str) {
        if let Some(rest) = input.strip_prefix(">=") {
            (CmpOp::Ge, rest)
        } else if let Some(rest) = input.strip_prefix("<=") {
            (CmpOp::Le, rest)
        } else if let Some(rest) = input.strip_prefix("!=") {
            (CmpOp::NotEq, rest)
        } else if let Some(rest) = input.strip_prefix('>') {
            (CmpOp::Gt, rest)
        } else if let Some(rest) = input.strip_prefix('<') {
            (CmpOp::Lt, rest)
        } else if let Some(rest) = input.strip_prefix('=') {
            (CmpOp::Eq, rest)
        } else {
            (CmpOp::Contains, input)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortPreset {
    ArtistAlbumTrack,
    YearDesc,
    MostPlayed,
    HighestRated,
    Random,
    /// Pick one album at random, then play it in track order.
    RandomAlbum,
    /// Order by file path, then title (used by the files tab).
    Path,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartPlaylist {
    pub id: String,
    pub name: String,
    pub query: String,
    pub sort_preset: SortPreset,
}

/// One aggregated album row, used by the albums tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub artist: String,
    pub album: String,
    pub year: u32,
    pub track_count: u32,
    pub total_duration_secs: u32,
}

/// One aggregated artist row, used by the artists tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artist {
    pub name: String,
    pub album_count: u32,
    pub track_count: u32,
}

/// The unified intent system. Both the Android UI and desktop TUI fire these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PlayerIntent {
    // Playback & queue
    PlayTrack { id: String },
    Enqueue { id: String, next: bool },
    RemoveFromQueue { id: String },
    ClearQueue,
    JumpTo { id: String },
    TogglePlayPause,
    NextTrack,
    PreviousTrack,
    StopAfterCurrent,
    SetVolume { volume: f32 },

    // Metadata
    RateTrack { id: String, rating: u8 },
    IncrementPlayCount { id: String },

    // Smart playlists
    SavePlaylist { name: String },
    DeletePlaylist { id: String },
    ActivatePlaylist { id: String },

    // Library
    ScanLibrary { roots: Vec<String> },
}

/// A snapshot of the live queue, persisted so the session resumes cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub explicit_queue: Vec<String>,
    pub dynamic_queue: Vec<String>,
    pub history: Vec<String>,
    pub current_track: Option<String>,
}
