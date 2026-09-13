// ./core/src/store.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! The persistent library catalog, backed by an embedded SQLite database.
//!
//! SQLite holds the source of truth for the catalog and answers every search
//! via indexed, compiled SQL. The live playback queue lives in memory (see
//! `controller`) and is snapshotted here on exit.

use crate::matcher::{self, SqlParam};
use crate::model::{Album, Artist, QueueSnapshot, SmartPlaylist, SortPreset, Track};
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tracks (
    id            TEXT PRIMARY KEY,
    path          TEXT NOT NULL,
    title         TEXT NOT NULL,
    artist        TEXT NOT NULL,
    album         TEXT NOT NULL,
    genre         TEXT NOT NULL,
    comment       TEXT NOT NULL,
    track_number  INTEGER NOT NULL DEFAULT 0,
    year          INTEGER NOT NULL DEFAULT 0,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    rating        INTEGER NOT NULL DEFAULT 0,
    play_count    INTEGER NOT NULL DEFAULT 0,
    last_played   INTEGER,
    file_mtime    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_tracks_artist ON tracks(artist);
CREATE INDEX IF NOT EXISTS idx_tracks_album  ON tracks(album);
CREATE INDEX IF NOT EXISTS idx_tracks_title ON tracks(title);
CREATE INDEX IF NOT EXISTS idx_tracks_genre ON tracks(genre);
CREATE INDEX IF NOT EXISTS idx_tracks_year  ON tracks(year);
CREATE INDEX IF NOT EXISTS idx_tracks_path  ON tracks(path);
CREATE INDEX IF NOT EXISTS idx_tracks_sort  ON tracks(artist, album, track_number, title);

CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);
";

/// A page of filter results plus the total match count.
#[derive(Debug, Clone)]
pub struct FilterPage {
    pub tracks: Vec<Track>,
    pub total: u64,
}

pub struct LibraryStore {
    conn: Mutex<Connection>,
    /// A second read-only connection. With WAL mode this allows the UI to
    /// query the catalog while the scan thread writes without blocking.
    /// `None` for in-memory databases (tests).
    read_conn: Option<Mutex<Connection>>,
}

impl LibraryStore {
    /// Open (or create) the catalog at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;

        let read_conn = Connection::open(path)?;
        read_conn.pragma_update(None, "journal_mode", "WAL")?;
        read_conn.pragma_update(None, "query_only", "ON")?;
        read_conn.execute_batch(SCHEMA)?;

        Ok(Self {
            conn: Mutex::new(conn),
            read_conn: Some(Mutex::new(read_conn)),
        })
    }

    /// An in-memory catalog, useful for tests.
    pub fn open_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
            read_conn: None,
        })
    }

    /// Connection for reads. Uses the dedicated read connection when available
    /// so queries never block on scan writes (WAL mode).
    fn read_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        match &self.read_conn {
            Some(rc) => rc.lock().unwrap(),
            None => self.conn.lock().unwrap(),
        }
    }

    /// Connection for writes (upserts, deletes, kv updates).
    fn write_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    /// Insert or replace a track, preserving user-managed mutable columns
    /// (rating, play_count, last_played) when the file has not changed.
    pub fn upsert_track(&self, track: &Track) -> rusqlite::Result<()> {
        let conn = self.write_conn();
        // Preserve mutable metadata across rescans when mtime is unchanged.
        let preserved: Option<(u8, u32, Option<i64>)> = conn
            .query_row(
                "SELECT rating, play_count, last_played FROM tracks WHERE id = ?1 AND file_mtime = ?2",
                rusqlite::params![track.id, track.file_mtime],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (rating, play_count, last_played) = match preserved {
            Some(r) => r,
            None => (track.rating, track.play_count, track.last_played),
        };
        conn.execute(
            "INSERT OR REPLACE INTO tracks
                (id, path, title, artist, album, genre, comment,
                 track_number, year, duration_secs, rating, play_count, last_played, file_mtime)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            rusqlite::params![
                track.id,
                track.path,
                track.title,
                track.artist,
                track.album,
                track.genre,
                track.comment,
                track.track_number,
                track.year,
                track.duration_secs,
                rating,
                play_count,
                last_played,
                track.file_mtime,
            ],
        )?;
        Ok(())
    }

    pub fn get_track(&self, id: &str) -> Option<Track> {
        let conn = self.read_conn();
        conn.query_row(
            "SELECT id, path, title, artist, album, genre, comment,
                    track_number, year, duration_secs, rating, play_count, last_played, file_mtime
             FROM tracks WHERE id = ?1",
            rusqlite::params![id],
            row_to_track,
        )
        .ok()
    }

    pub fn get_path(&self, id: &str) -> Option<String> {
        let conn = self.read_conn();
        conn.query_row(
            "SELECT path FROM tracks WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    pub fn track_count(&self) -> u64 {
        let conn = self.read_conn();
        conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as u64
    }

    /// All known file paths, used by the scanner to prune removed files.
    pub fn all_paths(&self) -> Vec<String> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare("SELECT path FROM tracks").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    /// Map of path -> file_mtime, used by the scanner for incremental rescans.
    pub fn paths_with_mtime(&self) -> std::collections::HashMap<String, i64> {
        let conn = self.read_conn();
        let mut stmt = match conn.prepare("SELECT path, file_mtime FROM tracks") {
            Ok(s) => s,
            Err(_) => return std::collections::HashMap::new(),
        };
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap();
        let mut map = std::collections::HashMap::new();
        for (p, m) in rows.flatten() {
            map.insert(p, m);
        }
        map
    }

    /// Delete tracks whose paths are not in `keep`. Batched in a transaction
    /// so thousands of deletes commit in one pass instead of one fsync each.
    pub fn prune(&self, keep: &std::collections::HashSet<String>) -> usize {
        let conn = self.write_conn();
        let mut stmt = match conn.prepare("SELECT path FROM tracks") {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let paths: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        drop(stmt);

        let to_delete: Vec<&String> = paths.iter().filter(|p| !keep.contains(*p)).collect();
        if to_delete.is_empty() {
            return 0;
        }
        conn.execute_batch("BEGIN IMMEDIATE").ok();
        let mut deleted = 0;
        for path in to_delete {
            deleted += conn
                .execute(
                    "DELETE FROM tracks WHERE path = ?1",
                    rusqlite::params![path],
                )
                .unwrap_or(0);
        }
        conn.execute_batch("COMMIT").ok();
        deleted
    }

    /// Filter tracks by a compiled query, returning one page plus the total
    /// match count. This is what the TUI calls on every keystroke.
    pub fn filter(
        &self,
        expr: &matcher::SearchExpr,
        sort: SortPreset,
        limit: u32,
        offset: u32,
    ) -> FilterPage {
        let frag = expr.to_sql();
        let order = matcher::sort_to_order_by(sort);
        let conn = self.read_conn();

        let where_sql = if frag.where_clause.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", frag.where_clause)
        };

        let total: u64 = if frag.where_clause.is_empty() {
            conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0) as u64
        } else {
            let sql = format!("SELECT COUNT(*) FROM tracks {where_sql}");
            query_scalar_with_params(&conn, &sql, &frag.params)
        };

        let sql = format!(
            "SELECT id, path, title, artist, album, genre, comment,
                    track_number, year, duration_secs, rating, play_count, last_played, file_mtime
             FROM tracks {where_sql}
             ORDER BY {order}
             LIMIT ? OFFSET ?"
        );
        let tracks = select_tracks(&conn, &sql, &frag.params, limit, offset);

        FilterPage { tracks, total }
    }

    /// Random album: pick one album, return its tracks in track order.
    pub fn random_album_tracks(&self, expr: &matcher::SearchExpr) -> Vec<Track> {
        let frag = expr.to_sql();
        let conn = self.read_conn();
        let where_sql = if frag.where_clause.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", frag.where_clause)
        };

        // Pick a random album (artist, album) pair.
        let pick_sql = format!(
            "SELECT artist, album FROM tracks {where_sql} GROUP BY artist, album ORDER BY RANDOM() LIMIT 1"
        );
        let pair: Option<(String, String)> = if frag.where_clause.is_empty() {
            conn.query_row(&pick_sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()
        } else {
            conn.query_row(
                &pick_sql,
                params_from_iter(params_as_dyn(&frag.params)),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()
        };

        let Some((artist, album)) = pair else {
            return Vec::new();
        };

        let sql = "SELECT id, path, title, artist, album, genre, comment,
                          track_number, year, duration_secs, rating, play_count, last_played, file_mtime
                   FROM tracks WHERE artist = ?1 AND album = ?2
                   ORDER BY track_number, title";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query(rusqlite::params![artist, album]) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for t in rows.mapped(row_to_track).flatten() {
            out.push(t);
        }
        out
    }

    /// Aggregated album rows, optionally filtered by the same query.
    pub fn albums(&self, expr: &matcher::SearchExpr) -> Vec<Album> {
        let frag = expr.to_sql();
        let conn = self.read_conn();
        let where_sql = if frag.where_clause.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", frag.where_clause)
        };
        let sql = format!(
            "SELECT artist, album, MAX(year) AS year, COUNT(*) AS track_count,
                    SUM(duration_secs) AS total
             FROM tracks {where_sql}
             GROUP BY artist, album
             ORDER BY artist, album"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = if frag.where_clause.is_empty() {
            stmt.query([]).unwrap()
        } else {
            stmt.query(params_from_iter(params_as_dyn(&frag.params)))
                .unwrap()
        };
        let mut out = Vec::new();
        for a in rows
            .mapped(|row| {
                Ok(Album {
                    artist: row.get(0)?,
                    album: row.get(1)?,
                    year: row.get::<_, i64>(2).unwrap_or(0) as u32,
                    track_count: row.get::<_, i64>(3).unwrap_or(0) as u32,
                    total_duration_secs: row.get::<_, i64>(4).unwrap_or(0) as u32,
                })
            })
            .flatten()
        {
            out.push(a);
        }
        out
    }

    /// Aggregated artist rows, optionally filtered by the same query.
    pub fn artists(&self, expr: &matcher::SearchExpr) -> Vec<Artist> {
        let frag = expr.to_sql();
        let conn = self.read_conn();
        let where_sql = if frag.where_clause.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", frag.where_clause)
        };
        let sql = format!(
            "SELECT artist, COUNT(DISTINCT album) AS album_count, COUNT(*) AS track_count
             FROM tracks {where_sql}
             GROUP BY artist
             ORDER BY artist"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = if frag.where_clause.is_empty() {
            stmt.query([]).unwrap()
        } else {
            stmt.query(params_from_iter(params_as_dyn(&frag.params)))
                .unwrap()
        };
        let mut out = Vec::new();
        for a in rows
            .mapped(|row| {
                Ok(Artist {
                    name: row.get(0)?,
                    album_count: row.get::<_, i64>(1).unwrap_or(0) as u32,
                    track_count: row.get::<_, i64>(2).unwrap_or(0) as u32,
                })
            })
            .flatten()
        {
            out.push(a);
        }
        out
    }

    /// Tracks for a single album (artist + album), in track order.
    pub fn album_tracks(&self, artist: &str, album: &str) -> Vec<Track> {
        let conn = self.read_conn();
        let sql = "SELECT id, path, title, artist, album, genre, comment,
                          track_number, year, duration_secs, rating, play_count, last_played, file_mtime
                   FROM tracks WHERE artist = ?1 AND album = ?2
                   ORDER BY track_number, title";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        let rows = stmt.query(rusqlite::params![artist, album]).unwrap();
        for t in rows.mapped(row_to_track).flatten() {
            out.push(t);
        }
        out
    }

    /// Tracks for a single artist.
    pub fn artist_tracks(&self, artist: &str) -> Vec<Track> {
        let conn = self.read_conn();
        let sql = "SELECT id, path, title, artist, album, genre, comment,
                          track_number, year, duration_secs, rating, play_count, last_played, file_mtime
                   FROM tracks WHERE artist = ?1
                   ORDER BY album, track_number, title";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        let rows = stmt.query(rusqlite::params![artist]).unwrap();
        for t in rows.mapped(row_to_track).flatten() {
            out.push(t);
        }
        out
    }

    pub fn update_rating(&self, id: &str, rating: u8) {
        let conn = self.write_conn();
        let _ = conn.execute(
            "UPDATE tracks SET rating = ?1 WHERE id = ?2",
            rusqlite::params![rating, id],
        );
    }

    pub fn increment_play_count(&self, id: &str, last_played: i64) {
        let conn = self.write_conn();
        let _ = conn.execute(
            "UPDATE tracks SET play_count = play_count + 1, last_played = ?1 WHERE id = ?2",
            rusqlite::params![last_played, id],
        );
    }

    // --- key/value store (queue snapshot, settings) ---

    fn kv_set(&self, key: &str, value: &str) {
        let conn = self.write_conn();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        );
    }

    fn kv_get(&self, key: &str) -> Option<String> {
        let conn = self.read_conn();
        conn.query_row(
            "SELECT value FROM kv WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    /// Persist the live queue so the session resumes cleanly.
    pub fn save_queue_snapshot(&self, snap: &QueueSnapshot) {
        if let Ok(json) = serde_json::to_string(snap) {
            self.kv_set("queue_snapshot", &json);
        }
    }

    pub fn load_queue_snapshot(&self) -> Option<QueueSnapshot> {
        self.kv_get("queue_snapshot")
            .and_then(|v| serde_json::from_str(&v).ok())
    }

    /// Persist the configured scan roots.
    pub fn save_roots(&self, roots: &[String]) {
        if let Ok(json) = serde_json::to_string(roots) {
            self.kv_set("roots", &json);
        }
    }

    pub fn load_roots(&self) -> Vec<String> {
        self.kv_get("roots")
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }

    pub fn save_volume(&self, volume: f32) {
        self.kv_set("volume", &volume.to_string());
    }

    pub fn load_volume(&self) -> Option<f32> {
        self.kv_get("volume").and_then(|v| v.parse().ok())
    }

    pub fn save_smart_playlists(&self, playlists: &[SmartPlaylist]) {
        if let Ok(json) = serde_json::to_string(playlists) {
            self.kv_set("smart_playlists", &json);
        }
    }

    pub fn load_smart_playlists(&self) -> Vec<SmartPlaylist> {
        self.kv_get("smart_playlists")
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }
}

fn params_as_dyn(params: &[SqlParam]) -> Vec<&dyn rusqlite::ToSql> {
    params
        .iter()
        .map(|p| match p {
            SqlParam::Text(s) => s as &dyn rusqlite::ToSql,
            SqlParam::Int(n) => n as &dyn rusqlite::ToSql,
        })
        .collect()
}

fn query_scalar_with_params(conn: &Connection, sql: &str, params: &[SqlParam]) -> u64 {
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    stmt.query_row(params_from_iter(params_as_dyn(params)), |row| {
        row.get::<_, i64>(0)
    })
    .unwrap_or(0) as u64
}

fn select_tracks(
    conn: &Connection,
    sql: &str,
    params: &[SqlParam],
    limit: u32,
    offset: u32,
) -> Vec<Track> {
    let mut all_params: Vec<&dyn rusqlite::ToSql> = params_as_dyn(params);
    let l = limit as i64;
    let o = offset as i64;
    all_params.push(&l);
    all_params.push(&o);
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let rows = match stmt.query(params_from_iter(all_params)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    for t in rows.mapped(row_to_track).flatten() {
        out.push(t);
    }
    out
}

fn row_to_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    Ok(Track {
        id: row.get(0)?,
        path: row.get(1)?,
        title: row.get(2)?,
        artist: row.get(3)?,
        album: row.get(4)?,
        genre: row.get(5)?,
        comment: row.get(6)?,
        track_number: row.get::<_, i64>(7).unwrap_or(0) as u32,
        year: row.get::<_, i64>(8).unwrap_or(0) as u32,
        duration_secs: row.get::<_, i64>(9).unwrap_or(0) as u32,
        rating: row.get::<_, i64>(10).unwrap_or(0) as u8,
        play_count: row.get::<_, i64>(11).unwrap_or(0) as u32,
        last_played: row.get(12)?,
        file_mtime: row.get::<_, i64>(13).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::parse_query;
    use crate::model::SortPreset;

    fn t(id: &str, title: &str, artist: &str, album: &str, year: u32) -> Track {
        Track {
            id: id.into(),
            path: format!("/{id}"),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            genre: "jazz".into(),
            comment: String::new(),
            track_number: 1,
            year,
            duration_secs: 200,
            rating: 0,
            play_count: 0,
            last_played: None,
            file_mtime: 0,
        }
    }

    #[test]
    fn round_trip_and_filter() {
        let store = LibraryStore::open_memory().unwrap();
        store
            .upsert_track(&t("1", "So What", "Miles Davis", "Kind of Blue", 1959))
            .unwrap();
        store
            .upsert_track(&t(
                "2",
                "Freddie Freeloader",
                "Miles Davis",
                "Kind of Blue",
                1959,
            ))
            .unwrap();
        store
            .upsert_track(&t("3", "Giant Steps", "John Coltrane", "Giant Steps", 1960))
            .unwrap();

        let expr = parse_query("ar:miles");
        let page = store.filter(&expr, SortPreset::ArtistAlbumTrack, 100, 0);
        assert_eq!(page.total, 2);

        let expr = parse_query("year:>=1960");
        let page = store.filter(&expr, SortPreset::YearDesc, 100, 0);
        assert_eq!(page.total, 1);
        assert_eq!(page.tracks[0].title, "Giant Steps");

        let expr = parse_query("");
        let page = store.filter(&expr, SortPreset::ArtistAlbumTrack, 100, 0);
        assert_eq!(page.total, 3);

        let albums = store.albums(&parse_query(""));
        assert_eq!(albums.len(), 2);

        let artists = store.artists(&parse_query(""));
        assert_eq!(artists.len(), 2);

        let ra = store.random_album_tracks(&parse_query(""));
        assert!(!ra.is_empty());
    }

    #[test]
    fn preserves_rating_across_rescan() {
        let store = LibraryStore::open_memory().unwrap();
        let mut track = t("1", "So What", "Miles Davis", "Kind of Blue", 1959);
        store.upsert_track(&track).unwrap();
        store.update_rating("1", 5);

        // Rescan with same mtime: rating must be preserved.
        store.upsert_track(&track).unwrap();
        let got = store.get_track("1").unwrap();
        assert_eq!(got.rating, 5);

        // Rescan with new mtime: rating resets to the (zero) incoming value.
        track.file_mtime = 1000;
        store.upsert_track(&track).unwrap();
        let got = store.get_track("1").unwrap();
        assert_eq!(got.rating, 0);
    }

    #[test]
    fn queue_snapshot_round_trip() {
        let store = LibraryStore::open_memory().unwrap();
        let snap = QueueSnapshot {
            explicit_queue: vec!["1".into()],
            dynamic_queue: vec!["2".into()],
            history: vec![],
            current_track: Some("1".into()),
        };
        store.save_queue_snapshot(&snap);
        let loaded = store.load_queue_snapshot().unwrap();
        assert_eq!(loaded.current_track.as_deref(), Some("1"));
        assert_eq!(loaded.dynamic_queue, vec!["2".to_string()]);
    }
}
