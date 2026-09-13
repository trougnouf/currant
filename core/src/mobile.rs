use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: String, // UUID or fast hash of path
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub comment: String,

    pub track_number: u32,
    pub year: u32,
    pub duration_secs: u32,

    // Mutable user-data
    pub rating: u8, // 0 to 5
    pub play_count: u32,
    pub last_played: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortPreset {
    ArtistAlbumTrack,
    YearDesc,
    MostPlayed,
    HighestRated,
    Random,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartPlaylist {
    pub id: String,
    pub name: String,
    pub query: String,
    pub sort_preset: SortPreset,
}

/// The unified intent system. Both the Android UI and Desktop TUI fire these.
#[derive(Debug, Clone)]
pub enum PlayerIntent {
    // Playback & Queue
    PlayTrack { id: String },
    Enqueue { id: String, next: bool },
    RemoveFromQueue { id: String },
    ClearQueue,
    TogglePlayPause,
    NextTrack,
    PreviousTrack,
    StopAfterCurrent,

    // Metadata
    RateTrack { id: String, rating: u8 },
    IncrementPlayCount { id: String },

    // Library
    ScanLibrary { roots: Vec<String> },
}
