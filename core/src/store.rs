// ./core/src/store.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! The persistent library catalog, backed by an embedded SQLite database.
//!
//! SQLite holds the source of truth for the catalog and answers every search
//! via indexed, compiled SQL. The live playback queue lives in memory (see
//! `controller`) and is snapshotted here on exit.

use crate::matcher::{self, SqlParam};
use crate::model::{Album, Artist, QueueSnapshot, SmartPlaylist, SortPreset, SyncPayload, Track};
use crate::text;
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tracks (
    id            TEXT PRIMARY KEY,
    path          TEXT NOT NULL,
    title         TEXT NOT NULL,
    artist        TEXT NOT NULL,
    album_artist  TEXT NOT NULL,
    album         TEXT NOT NULL,
    genre         TEXT NOT NULL,
    comment       TEXT NOT NULL,
    title_fold    TEXT NOT NULL DEFAULT '',
    artist_fold   TEXT NOT NULL DEFAULT '',
    album_artist_fold TEXT NOT NULL DEFAULT '',
    album_fold    TEXT NOT NULL DEFAULT '',
    genre_fold    TEXT NOT NULL DEFAULT '',
    comment_fold  TEXT NOT NULL DEFAULT '',
    track_number  INTEGER NOT NULL DEFAULT 0,
    year          INTEGER NOT NULL DEFAULT 0,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    rating        INTEGER NOT NULL DEFAULT 0,
    play_count    INTEGER NOT NULL DEFAULT 0,
    last_played   INTEGER,
    file_mtime    INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS track_sources (
    logical_id TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    path TEXT NOT NULL,
    format_tier INTEGER NOT NULL,
    file_mtime INTEGER NOT NULL,
    PRIMARY KEY (logical_id, instance_id, path),
    FOREIGN KEY (logical_id) REFERENCES tracks(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS tombstones (
    logical_id TEXT PRIMARY KEY,
    deleted_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tracks_artist ON tracks(artist);
CREATE INDEX IF NOT EXISTS idx_tracks_album_artist ON tracks(album_artist);
CREATE INDEX IF NOT EXISTS idx_tracks_album  ON tracks(album);
CREATE INDEX IF NOT EXISTS idx_tracks_title ON tracks(title);
CREATE INDEX IF NOT EXISTS idx_tracks_genre ON tracks(genre);
CREATE INDEX IF NOT EXISTS idx_tracks_year  ON tracks(year);
CREATE INDEX IF NOT EXISTS idx_tracks_path  ON tracks(path);
CREATE INDEX IF NOT EXISTS idx_tracks_sort  ON tracks(album_artist_fold, album_fold, track_number, title_fold);
CREATE INDEX IF NOT EXISTS idx_tracks_title_fold  ON tracks(title_fold);
CREATE INDEX IF NOT EXISTS idx_tracks_artist_fold ON tracks(artist_fold);
CREATE INDEX IF NOT EXISTS idx_tracks_album_fold  ON tracks(album_fold);

CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);

CREATE VIEW IF NOT EXISTS tracks_with_local AS
SELECT t.*,
       EXISTS(
           SELECT 1 FROM track_sources s
           WHERE s.logical_id = t.id
             AND s.instance_id = (SELECT value FROM kv WHERE key = 'instance_id')
       ) AS is_local
FROM tracks t;
";

/// Register the `fold` scalar function for accent-insensitive, case-insensitive
/// text matching. Must be called on every connection that runs search queries.
fn register_fold(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "fold",
        1,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_UTF8,
        |ctx| {
            let s = ctx.get::<String>(0)?;
            Ok(text::fold(&s))
        },
    )
}

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
    instance_id: String,
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

        // Simple wipe on breaking schema updates to guarantee deduplication correctness.
        // It keeps the `kv` table untouched so user settings aren't lost.
        conn.execute_batch("CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);")?;
        let db_version: i64 = conn
            .query_row(
                "SELECT value FROM kv WHERE key = 'schema_version'",
                [],
                |r| {
                    let val: String = r.get(0)?;
                    val.parse()
                        .map_err(|_| rusqlite::Error::ExecuteReturnedResults)
                },
            )
            .unwrap_or(0);

        if db_version < 2 {
            conn.execute_batch(
                "
                DROP TABLE IF EXISTS tracks;
                DROP TABLE IF EXISTS track_sources;
                DROP TABLE IF EXISTS tombstones;
                DROP VIEW IF EXISTS tracks_with_local;
            ",
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO kv (key, value) VALUES ('schema_version', '2')",
                [],
            )?;
        }

        conn.execute_batch(SCHEMA)?;
        register_fold(&conn)?;
        migrate(&conn);

        let read_conn = Connection::open(path)?;
        read_conn.pragma_update(None, "journal_mode", "WAL")?;
        read_conn.pragma_update(None, "query_only", "ON")?;
        read_conn.execute_batch(SCHEMA)?;
        register_fold(&read_conn)?;

        let instance_id = Self::instance_id_on(&conn);
        Ok(Self {
            conn: Mutex::new(conn),
            read_conn: Some(Mutex::new(read_conn)),
            instance_id,
        })
    }

    /// An in-memory catalog, useful for tests.
    pub fn open_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        register_fold(&conn)?;
        let instance_id = Self::instance_id_on(&conn);
        Ok(Self {
            conn: Mutex::new(conn),
            read_conn: None,
            instance_id,
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

    /// The local instance's stable mesh identity. Generated once and cached in
    /// the `kv` table so it survives restarts.
    pub fn local_instance_id(&self) -> String {
        self.instance_id.clone()
    }

    /// Resolve (or create) the instance id on an already-open connection.
    /// Called once from the constructors, which cache the result on the
    /// struct so the `kv` table is read at most once per process.
    fn instance_id_on(conn: &Connection) -> String {
        let id: Option<String> = conn
            .query_row("SELECT value FROM kv WHERE key = 'instance_id'", [], |r| {
                r.get(0)
            })
            .ok();
        if let Some(id) = id {
            id
        } else {
            let new_id = uuid::Uuid::new_v4().to_string();
            let _ = conn.execute(
                "INSERT INTO kv (key, value) VALUES ('instance_id', ?1)",
                rusqlite::params![&new_id],
            );
            new_id
        }
    }

    /// Insert or replace a track, preserving user-managed mutable columns
    /// (rating, play_count, last_played) when the file has not changed.
    /// Insert or replace multiple tracks in a single transaction. Drastically
    /// improves performance during full scans.
    pub fn upsert_batch(&self, tracks: &[Track]) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let now = unix_now();
        let instance_id = self.instance_id.clone();

        {
            let mut stmt_preserve = tx.prepare_cached(
                "SELECT rating, play_count, last_played FROM tracks WHERE id = ?1 AND file_mtime = ?2",
            )?;
            let mut stmt_track = tx.prepare_cached(UPSERT_TRACK_SQL)?;
            let mut stmt_source = tx.prepare_cached(
                "INSERT OR REPLACE INTO track_sources (logical_id, instance_id, path, format_tier, file_mtime) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;

            for track in tracks {
                let preserved: Option<(u8, u32, Option<i64>)> = stmt_preserve
                    .query_row(rusqlite::params![track.id, track.file_mtime], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .optional()?;
                let (rating, play_count, last_played) = match preserved {
                    Some(r) => r,
                    None => (track.rating, track.play_count, track.last_played),
                };

                let title_fold = text::fold(&track.title);
                let artist_fold = text::fold(&track.artist);
                let album_artist_fold = text::fold(&track.album_artist);
                let album_fold = text::fold(&track.album);
                let genre_fold = text::fold(&track.genre);
                let comment_fold = text::fold(&track.comment);

                stmt_track.execute(rusqlite::params![
                    track.id,
                    track.path,
                    track.title,
                    track.artist,
                    track.album_artist,
                    track.album,
                    track.genre,
                    track.comment,
                    title_fold,
                    artist_fold,
                    album_artist_fold,
                    album_fold,
                    genre_fold,
                    comment_fold,
                    track.track_number,
                    track.year,
                    track.duration_secs,
                    rating,
                    play_count,
                    last_played,
                    track.file_mtime,
                    now,
                ])?;

                let format_tier = if track.path.ends_with(".flac") || track.path.ends_with(".wav") {
                    1
                } else {
                    0
                };
                stmt_source.execute(rusqlite::params![
                    track.id,
                    instance_id,
                    track.path,
                    format_tier,
                    track.file_mtime,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
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
        let title_fold = text::fold(&track.title);
        let artist_fold = text::fold(&track.artist);
        let album_artist_fold = text::fold(&track.album_artist);
        let album_fold = text::fold(&track.album);
        let genre_fold = text::fold(&track.genre);
        let comment_fold = text::fold(&track.comment);
        let now = unix_now();
        conn.execute(
            UPSERT_TRACK_SQL,
            rusqlite::params![
                track.id,
                track.path,
                track.title,
                track.artist,
                track.album_artist,
                track.album,
                track.genre,
                track.comment,
                title_fold,
                artist_fold,
                album_artist_fold,
                album_fold,
                genre_fold,
                comment_fold,
                track.track_number,
                track.year,
                track.duration_secs,
                rating,
                play_count,
                last_played,
                track.file_mtime,
                now,
            ],
        )?;

        let instance_id = self.instance_id.clone();
        let format_tier = if track.path.ends_with(".flac") || track.path.ends_with(".wav") {
            1
        } else {
            0
        };
        conn.execute(
            "INSERT OR REPLACE INTO track_sources (logical_id, instance_id, path, format_tier, file_mtime) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                track.id,
                instance_id,
                track.path,
                format_tier,
                track.file_mtime,
            ],
        )?;

        Ok(())
    }

    pub fn get_track(&self, id: &str) -> Option<Track> {
        let conn = self.read_conn();
        conn.query_row(
            "SELECT id, path, title, artist, album_artist, album, genre, comment,
                    track_number, year, duration_secs, rating, play_count, last_played, file_mtime, updated_at, is_local
             FROM tracks_with_local WHERE id = ?1",
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
        let now = unix_now();
        for path in to_delete {
            // Capture the logical ID so we can emit a tombstone, letting peers
            // drop the track too once they receive the next delta sync.
            if let Ok(logical_id) = conn.query_row(
                "SELECT id FROM tracks WHERE path = ?1",
                rusqlite::params![path],
                |row| row.get::<_, String>(0),
            ) {
                let rows = conn
                    .execute(
                        "DELETE FROM tracks WHERE path = ?1",
                        rusqlite::params![path],
                    )
                    .unwrap_or(0);
                if rows > 0 {
                    let _ = conn.execute(
                        "INSERT OR REPLACE INTO tombstones (logical_id, deleted_at) VALUES (?1, ?2)",
                        rusqlite::params![logical_id, now],
                    );
                    deleted += rows;
                }
            }
        }
        conn.execute_batch("COMMIT").ok();
        deleted
    }

    /// Find the 0-based row position of `id` in the filtered, sorted list.
    /// Returns `None` if the track is not in the filtered set, or if the
    /// sort is random (position is non-deterministic).
    pub fn track_position(
        &self,
        id: &str,
        expr: &matcher::SearchExpr,
        sort: SortPreset,
    ) -> Option<u64> {
        if matches!(sort, SortPreset::Random | SortPreset::RandomAlbum) {
            return None;
        }
        let frag = expr.to_sql();
        let conn = self.read_conn();
        let where_sql = if frag.where_clause.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", frag.where_clause)
        };
        let order = matcher::sort_to_order_by(sort);
        let sql = format!(
            "SELECT pos FROM (
                SELECT id, ROW_NUMBER() OVER (ORDER BY {order}) AS pos
                FROM tracks {where_sql}
            ) WHERE id = ?"
        );
        let mut all_params: Vec<&dyn rusqlite::ToSql> = params_as_dyn(&frag.params);
        all_params.push(&id);
        let mut stmt = conn.prepare(&sql).ok()?;
        stmt.query_row(params_from_iter(all_params), |row| row.get::<_, i64>(0))
            .ok()
            .map(|v| v as u64 - 1)
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
            "SELECT id, path, title, artist, album_artist, album, genre, comment,
                    track_number, year, duration_secs, rating, play_count, last_played, file_mtime, updated_at, is_local
             FROM tracks_with_local {where_sql}
             ORDER BY {order}
             LIMIT ? OFFSET ?"
        );
        let tracks = select_tracks(&conn, &sql, &frag.params, limit, offset);

        FilterPage { tracks, total }
    }

    /// Track IDs from the filtered, sorted list. Lighter than `filter` when
    /// only the ordering is needed (e.g., seeding the play queue).
    pub fn filter_ids(
        &self,
        expr: &matcher::SearchExpr,
        sort: SortPreset,
        limit: u32,
        offset: u32,
    ) -> Vec<String> {
        let frag = expr.to_sql();
        let order = matcher::sort_to_order_by(sort);
        let conn = self.read_conn();
        let where_sql = if frag.where_clause.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", frag.where_clause)
        };
        let sql = format!("SELECT id FROM tracks {where_sql} ORDER BY {order} LIMIT ? OFFSET ?");
        let mut all_params: Vec<&dyn rusqlite::ToSql> = params_as_dyn(&frag.params);
        let l = limit as i64;
        let o = offset as i64;
        all_params.push(&l);
        all_params.push(&o);
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query(params_from_iter(all_params)) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.mapped(|row| row.get::<_, String>(0))
            .flatten()
            .collect()
    }

    /// Random album: pick one album, return its tracks in track order.
    pub fn random_album_tracks(
        &self,
        expr: &matcher::SearchExpr,
        exclude: Option<(&str, &str)>,
    ) -> Vec<Track> {
        let frag = expr.to_sql();
        let conn = self.read_conn();

        // Build the album-pick query. An optional (album_artist, album) pair
        // is excluded so e.g. SkipAlbum doesn't re-pick the album just skipped.
        let mut exclude_clause = String::new();
        let mut exclude_params: Vec<SqlParam> = Vec::new();
        if let Some((ea, eal)) = exclude {
            exclude_clause = " AND NOT (album_artist = ? AND album = ?)".to_string();
            exclude_params = vec![
                SqlParam::Text(ea.to_string()),
                SqlParam::Text(eal.to_string()),
            ];
        }

        let where_sql = if frag.where_clause.is_empty() {
            if exclude_clause.is_empty() {
                String::new()
            } else {
                format!("WHERE 1=1{exclude_clause}")
            }
        } else {
            format!("WHERE {}{exclude_clause}", frag.where_clause)
        };

        // Pick a random album by name.
        let pick_sql = format!(
            "SELECT album_artist, album FROM tracks {where_sql} GROUP BY album_artist, album ORDER BY RANDOM() LIMIT 1"
        );
        let mut all_params: Vec<SqlParam> = frag.params.clone();
        all_params.extend(exclude_params);
        let album_info: Option<(String, String)> = if all_params.is_empty() {
            conn.query_row(&pick_sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()
        } else {
            conn.query_row(
                &pick_sql,
                params_from_iter(params_as_dyn(&all_params)),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()
        };

        let Some((album_artist, album)) = album_info else {
            return Vec::new();
        };

        // Get tracks from that album, also constrained by the filter so the
        // continuation never pulls in tracks the user filtered out.
        let mut track_where = String::from("album_artist = ? AND album = ?");
        let mut track_params: Vec<SqlParam> =
            vec![SqlParam::Text(album_artist), SqlParam::Text(album)];
        if !frag.where_clause.is_empty() {
            track_where.push_str(" AND (");
            track_where.push_str(&frag.where_clause);
            track_where.push(')');
            track_params.extend(frag.params);
        }
        let sql = format!(
            "SELECT id, path, title, artist, album_artist, album, genre, comment,
                          track_number, year, duration_secs, rating, play_count, last_played, file_mtime, updated_at, is_local
             FROM tracks_with_local WHERE {track_where}
             ORDER BY track_number, title"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query(params_from_iter(params_as_dyn(&track_params))) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for t in rows.mapped(row_to_track).flatten() {
            out.push(t);
        }
        out
    }

    /// Remaining tracks of an album that come *after* the given track number,
    /// in track order, constrained by the active filter. Used to continue an
    /// album the user started mid-way.
    pub fn album_tracks_after(
        &self,
        album_artist: &str,
        album: &str,
        after: u32,
        expr: &matcher::SearchExpr,
    ) -> Vec<Track> {
        let frag = expr.to_sql();
        let conn = self.read_conn();
        let mut where_clause = String::from("album_artist = ? AND album = ? AND track_number > ?");
        let mut params: Vec<SqlParam> = vec![
            SqlParam::Text(album_artist.into()),
            SqlParam::Text(album.into()),
            SqlParam::Int(after as i64),
        ];
        if !frag.where_clause.is_empty() {
            where_clause.push_str(" AND (");
            where_clause.push_str(&frag.where_clause);
            where_clause.push(')');
            params.extend(frag.params);
        }
        let sql = format!(
            "SELECT id, path, title, artist, album_artist, album, genre, comment,
                          track_number, year, duration_secs, rating, play_count, last_played, file_mtime, updated_at, is_local
             FROM tracks_with_local WHERE {where_clause}
             ORDER BY track_number, title"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query(params_from_iter(params_as_dyn(&params))) {
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
            "SELECT * FROM (
                SELECT
                    album_artist AS artist,
                    album, MAX(year) AS year, COUNT(*) AS track_count,
                    SUM(duration_secs) AS total
                 FROM tracks {where_sql}
                 GROUP BY album_artist, album
            ) ORDER BY fold(artist), fold(album)"
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
            "SELECT * FROM (
                SELECT album_artist AS artist, COUNT(DISTINCT album) AS album_count, COUNT(*) AS track_count
                 FROM tracks {where_sql}
                 GROUP BY album_artist
            ) ORDER BY fold(artist)"
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

    /// Tracks for a single album, in track order.
    pub fn album_tracks(&self, album_artist: &str, album: &str) -> Vec<Track> {
        let conn = self.read_conn();
        let sql = "SELECT id, path, title, artist, album_artist, album, genre, comment,
                          track_number, year, duration_secs, rating, play_count, last_played, file_mtime, updated_at, is_local
                   FROM tracks_with_local WHERE album_artist = ?1 AND album = ?2
                   ORDER BY track_number, title";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        let rows = stmt.query(rusqlite::params![album_artist, album]).unwrap();
        for t in rows.mapped(row_to_track).flatten() {
            out.push(t);
        }
        out
    }

    /// Tracks for a single artist.
    pub fn artist_tracks(&self, artist: &str) -> Vec<Track> {
        let conn = self.read_conn();
        let sql = "SELECT id, path, title, artist, album_artist, album, genre, comment,
                          track_number, year, duration_secs, rating, play_count, last_played, file_mtime, updated_at, is_local
                   FROM tracks_with_local WHERE artist = ?1
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

    /// Albums for a single artist, aggregated by album name.
    pub fn artist_albums(&self, artist: &str) -> Vec<Album> {
        let conn = self.read_conn();
        let sql = "SELECT
                album_artist AS artist,
                album, MAX(year) AS year, COUNT(*) AS track_count,
                SUM(duration_secs) AS total
             FROM tracks WHERE album_artist = ?1
             GROUP BY album_artist, album
             ORDER BY MIN(album), album";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        let rows = stmt.query(rusqlite::params![artist]).unwrap();
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

    pub fn save_replaygain(&self, enabled: bool) {
        self.kv_set("replaygain", if enabled { "true" } else { "false" });
    }

    pub fn load_replaygain(&self) -> bool {
        self.kv_get("replaygain")
            .map(|v| v == "true")
            .unwrap_or(false)
    }

    pub fn save_watch_roots(&self, enabled: bool) {
        self.kv_set("watch_roots", if enabled { "true" } else { "false" });
    }

    pub fn load_watch_roots(&self) -> bool {
        self.kv_get("watch_roots")
            .map(|v| v == "true")
            .unwrap_or(false)
    }

    pub fn save_live_columns(&self, enabled: bool) {
        self.kv_set("live_columns", if enabled { "true" } else { "false" });
    }

    pub fn load_live_columns(&self) -> bool {
        self.kv_get("live_columns")
            .map(|v| v == "true")
            .unwrap_or(false)
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

    /// Persist the Listenbrainz scrobble token. An empty token disables
    /// scrobbling.
    pub fn save_scrobble_token(&self, token: &str) {
        self.kv_set("scrobble_token", token);
    }

    pub fn load_scrobble_token(&self) -> String {
        self.kv_get("scrobble_token").unwrap_or_default()
    }

    pub fn save_pairing_token(&self, token: &str) {
        self.kv_set("pairing_token", token);
    }

    pub fn load_pairing_token(&self) -> String {
        self.kv_get("pairing_token").unwrap_or_else(|| {
            let t = uuid::Uuid::new_v4().to_string();
            self.kv_set("pairing_token", &t);
            t
        })
    }

    // --- mesh delta synchronization ---

    /// High-water mark (unix seconds) of the last completed sync from a peer.
    pub fn get_last_sync(&self, peer_id: &str) -> i64 {
        self.kv_get(&format!("sync_last_{peer_id}"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// Record the high-water mark after a successful sync from a peer.
    pub fn set_last_sync(&self, peer_id: &str, timestamp: i64) {
        self.kv_set(&format!("sync_last_{peer_id}"), &timestamp.to_string());
    }

    /// Collect the tracks and tombstones modified after `since`, ready to be
    /// serialized and served to a connecting peer.
    pub fn get_sync_payload(&self, since: i64) -> SyncPayload {
        let conn = self.read_conn();
        let mut tracks = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT id, path, title, artist, album_artist, album, genre, comment,
                    track_number, year, duration_secs, rating, play_count, last_played, file_mtime, updated_at, is_local
             FROM tracks_with_local WHERE updated_at > ?1",
        )
            && let Ok(rows) = stmt.query(rusqlite::params![since]) {
                tracks.extend(rows.mapped(row_to_track).flatten());
            }

        let mut tombstones = Vec::new();
        if let Ok(mut stmt) =
            conn.prepare("SELECT logical_id FROM tombstones WHERE deleted_at > ?1")
            && let Ok(mut rows) = stmt.query(rusqlite::params![since])
        {
            while let Ok(Some(row)) = rows.next() {
                if let Ok(id) = row.get::<_, String>(0) {
                    tombstones.push(id);
                }
            }
        }

        SyncPayload { tracks, tombstones }
    }

    /// Merge a peer's delta payload into the local catalog. Remote metadata
    /// wins when newer; local play data is preserved unless the peer played
    /// the track more recently (highest `last_played` wins). Remote files are
    /// mapped into `track_sources` under the peer's `instance_id`.
    pub fn apply_sync_payload(&self, peer_id: &str, payload: &SyncPayload) {
        let mut conn = self.write_conn();
        let tx = conn.transaction().unwrap();

        for track in &payload.tracks {
            let current_updated: i64 = tx
                .query_row(
                    "SELECT updated_at FROM tracks WHERE id = ?1",
                    rusqlite::params![track.id],
                    |row| row.get(0),
                )
                .unwrap_or(-1);

            if track.updated_at > current_updated {
                let title_fold = text::fold(&track.title);
                let artist_fold = text::fold(&track.artist);
                let album_artist_fold = text::fold(&track.album_artist);
                let album_fold = text::fold(&track.album);
                let genre_fold = text::fold(&track.genre);
                let comment_fold = text::fold(&track.comment);
                let _ = tx.execute(
                    UPSERT_TRACK_SQL,
                    rusqlite::params![
                        track.id,
                        track.path,
                        track.title,
                        track.artist,
                        track.album_artist,
                        track.album,
                        track.genre,
                        track.comment,
                        title_fold,
                        artist_fold,
                        album_artist_fold,
                        album_fold,
                        genre_fold,
                        comment_fold,
                        track.track_number,
                        track.year,
                        track.duration_secs,
                        track.rating,
                        track.play_count,
                        track.last_played,
                        track.file_mtime,
                        track.updated_at
                    ],
                );
            } else {
                // Local metadata is newer; keep it, but adopt the peer's play
                // data if it is more recent.
                let current_last_played: i64 = tx
                    .query_row(
                        "SELECT last_played FROM tracks WHERE id = ?1",
                        rusqlite::params![track.id],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                if track.last_played.unwrap_or(0) > current_last_played {
                    let _ = tx.execute(
                        "UPDATE tracks SET play_count = ?1, last_played = ?2 WHERE id = ?3",
                        rusqlite::params![track.play_count, track.last_played, track.id],
                    );
                }
            }

            let format_tier = if track.path.ends_with(".flac") || track.path.ends_with(".wav") {
                1
            } else {
                0
            };
            let _ = tx.execute(
                "INSERT OR REPLACE INTO track_sources (logical_id, instance_id, path, format_tier, file_mtime) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![track.id, peer_id, track.path, format_tier, track.file_mtime],
            );
        }

        for tombstone in &payload.tombstones {
            let _ = tx.execute(
                "DELETE FROM track_sources WHERE logical_id = ?1 AND instance_id = ?2",
                rusqlite::params![tombstone, peer_id],
            );
        }

        tx.commit().unwrap();
    }

    /// Resolves the first remote peer that holds a source for this track,
    /// used to pick where to stream it from. `None` when no other instance
    /// has a copy.
    pub fn get_remote_source_peer(&self, logical_id: &str) -> Option<String> {
        let conn = self.read_conn();
        conn.query_row(
            "SELECT instance_id FROM track_sources
             WHERE logical_id = ?1
               AND instance_id != (SELECT value FROM kv WHERE key = 'instance_id')
             LIMIT 1",
            rusqlite::params![logical_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    /// The local physical path for a logical id, used by the `/stream`
    /// endpoint. `None` when this instance doesn't hold the file.
    pub fn get_local_source_path(&self, logical_id: &str) -> Option<String> {
        let conn = self.read_conn();
        conn.query_row(
            "SELECT path FROM track_sources
             WHERE logical_id = ?1
               AND instance_id = (SELECT value FROM kv WHERE key = 'instance_id')
             LIMIT 1",
            rusqlite::params![logical_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
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
        album_artist: row.get(4)?,
        album: row.get(5)?,
        genre: row.get(6)?,
        comment: row.get(7)?,
        track_number: row.get::<_, i64>(8).unwrap_or(0) as u32,
        year: row.get::<_, i64>(9).unwrap_or(0) as u32,
        duration_secs: row.get::<_, i64>(10).unwrap_or(0) as u32,
        rating: row.get::<_, i64>(11).unwrap_or(0) as u8,
        play_count: row.get::<_, i64>(12).unwrap_or(0) as u32,
        last_played: row.get(13)?,
        file_mtime: row.get::<_, i64>(14).unwrap_or(0),
        updated_at: row.get::<_, i64>(15).unwrap_or(0),
        is_local: row.get::<_, bool>(16).unwrap_or(true),
    })
}

/// Upsert a full track row. `ON CONFLICT DO UPDATE` (rather than
/// `INSERT OR REPLACE`) matters: a replace is a delete-then-insert, which
/// cascades to `track_sources` and silently drops the physical sources of
/// other instances.
const UPSERT_TRACK_SQL: &str = "
INSERT INTO tracks
    (id, path, title, artist, album_artist, album, genre, comment,
     title_fold, artist_fold, album_artist_fold, album_fold, genre_fold, comment_fold,
     track_number, year, duration_secs, rating, play_count, last_played, file_mtime, updated_at)
VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
ON CONFLICT(id) DO UPDATE SET
    path = excluded.path,
    title = excluded.title,
    artist = excluded.artist,
    album_artist = excluded.album_artist,
    album = excluded.album,
    genre = excluded.genre,
    comment = excluded.comment,
    title_fold = excluded.title_fold,
    artist_fold = excluded.artist_fold,
    album_artist_fold = excluded.album_artist_fold,
    album_fold = excluded.album_fold,
    genre_fold = excluded.genre_fold,
    comment_fold = excluded.comment_fold,
    track_number = excluded.track_number,
    year = excluded.year,
    duration_secs = excluded.duration_secs,
    rating = excluded.rating,
    play_count = excluded.play_count,
    last_played = excluded.last_played,
    file_mtime = excluded.file_mtime,
    updated_at = excluded.updated_at";

/// Add columns introduced after the initial schema. SQLite doesn't support
/// `ADD COLUMN IF NOT EXISTS`, so we check `pragma_table_info` first.
fn migrate(conn: &Connection) {
    let has_album_artist: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tracks') WHERE name = 'album_artist'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_album_artist {
        let _ = conn.execute(
            "ALTER TABLE tracks ADD COLUMN album_artist TEXT NOT NULL DEFAULT ''",
            [],
        );
        // Backfill: copy artist into album_artist for existing rows.
        let _ = conn.execute("UPDATE tracks SET album_artist = artist", []);
    }

    // Backfill mistagged files whose AlbumArtist tag is present but empty.
    let _ = conn.execute(
        "UPDATE tracks SET album_artist = artist WHERE album_artist = ''",
        [],
    );

    // Add pre-folded shadow columns for accent-insensitive search and sort.
    for (fold_col, src_col) in [
        ("title_fold", "title"),
        ("artist_fold", "artist"),
        ("album_artist_fold", "album_artist"),
        ("album_fold", "album"),
        ("genre_fold", "genre"),
        ("comment_fold", "comment"),
    ] {
        let has_col: bool = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM pragma_table_info('tracks') WHERE name = '{fold_col}'"
                ),
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_col {
            let _ = conn.execute(
                &format!("ALTER TABLE tracks ADD COLUMN {fold_col} TEXT NOT NULL DEFAULT ''"),
                [],
            );
            let _ = conn.execute(
                &format!("UPDATE tracks SET {fold_col} = fold({src_col})"),
                [],
            );
        }
    }

    // Rebuild the sort index on folded columns (old index used raw columns).
    let _ = conn.execute("DROP INDEX IF EXISTS idx_tracks_sort", []);
    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tracks_sort ON tracks(album_artist_fold, album_fold, track_number, title_fold)",
        [],
    );

    // Dynamic view for source resolution: exposes `is_local` (does this
    // instance own a physical copy of the track?) without repeating the
    // JOIN in every track query.
    let _ = conn.execute(
        "CREATE VIEW IF NOT EXISTS tracks_with_local AS
         SELECT t.*,
                EXISTS(
                    SELECT 1 FROM track_sources s
                    WHERE s.logical_id = t.id
                      AND s.instance_id = (SELECT value FROM kv WHERE key = 'instance_id')
                ) AS is_local
         FROM tracks t",
        [],
    );

    // Net-Mesh Migration
    let has_updated_at: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tracks') WHERE name = 'updated_at'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !has_updated_at {
        let _ = conn.execute(
            "ALTER TABLE tracks ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0",
            [],
        );

        let _ = conn.execute(
            "CREATE TABLE IF NOT EXISTS track_sources (
                logical_id TEXT NOT NULL,
                instance_id TEXT NOT NULL,
                path TEXT NOT NULL,
                format_tier INTEGER NOT NULL,
                file_mtime INTEGER NOT NULL,
                PRIMARY KEY (logical_id, instance_id, path),
                FOREIGN KEY (logical_id) REFERENCES tracks(id) ON DELETE CASCADE
            )",
            [],
        );

        let _ = conn.execute(
            "CREATE TABLE IF NOT EXISTS tombstones (
                logical_id TEXT PRIMARY KEY,
                deleted_at INTEGER NOT NULL
            )",
            [],
        );

        // Migrate physical path IDs to pure metadata hashes
        let mut stmt = conn
            .prepare("SELECT id, album_artist, album, track_number, title FROM tracks")
            .unwrap();
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| {
                let old_id: String = row.get(0)?;
                let artist: String = row.get(1)?;
                let album: String = row.get(2)?;
                let track_no: u32 = row.get(3)?;
                let title: String = row.get(4)?;
                let new_id = crate::scanner::metadata_id(&artist, &album, track_no, &title);
                Ok((old_id, new_id))
            })
            .unwrap()
            .flatten()
            .collect();
        drop(stmt);

        let _ = conn.execute_batch("BEGIN IMMEDIATE");
        for (old_id, new_id) in rows {
            if old_id == new_id {
                continue;
            }
            let _ = conn.execute(
                "UPDATE OR IGNORE tracks SET id = ?2 WHERE id = ?1",
                rusqlite::params![&old_id, &new_id],
            );
            let _ = conn.execute(
                "DELETE FROM tracks WHERE id = ?1",
                rusqlite::params![&old_id],
            );
        }
        let _ = conn.execute_batch("COMMIT");
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
            album_artist: artist.into(),
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
            updated_at: 0,
            is_local: true,
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

        let ra = store.random_album_tracks(&parse_query(""), None);
        assert!(!ra.is_empty());
    }

    #[test]
    fn accent_insensitive_search() {
        let store = LibraryStore::open_memory().unwrap();
        store
            .upsert_track(&t("1", "Café Bleu", "Beyoncé", "Fiançailles", 2020))
            .unwrap();
        store
            .upsert_track(&t("2", "Plain", "Miles Davis", "Kind of Blue", 1959))
            .unwrap();

        // Free text matches accented title without typing accents.
        let expr = parse_query("cafe");
        let page = store.filter(&expr, SortPreset::ArtistAlbumTrack, 100, 0);
        assert_eq!(page.total, 1);
        assert_eq!(page.tracks[0].title, "Café Bleu");

        // Free text matches accented artist.
        let expr = parse_query("beyonce");
        let page = store.filter(&expr, SortPreset::ArtistAlbumTrack, 100, 0);
        assert_eq!(page.total, 1);
        assert_eq!(page.tracks[0].artist, "Beyoncé");

        // Field-specific search with accents.
        let expr = parse_query("ar:beYonce");
        let page = store.filter(&expr, SortPreset::ArtistAlbumTrack, 100, 0);
        assert_eq!(page.total, 1);

        // Search with accents matches plain text too.
        let expr = parse_query("café");
        let page = store.filter(&expr, SortPreset::ArtistAlbumTrack, 100, 0);
        assert_eq!(page.total, 1);
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

    #[test]
    fn scrobble_token_round_trip() {
        let store = LibraryStore::open_memory().unwrap();
        // No token set yet: empty (scrobbling disabled).
        assert_eq!(store.load_scrobble_token(), "");
        store.save_scrobble_token("abc123");
        assert_eq!(store.load_scrobble_token(), "abc123");
        // Overwrite, then clear.
        store.save_scrobble_token("xyz789");
        assert_eq!(store.load_scrobble_token(), "xyz789");
        store.save_scrobble_token("");
        assert_eq!(store.load_scrobble_token(), "");
    }

    #[test]
    fn sort_is_case_and_accent_insensitive() {
        let store = LibraryStore::open_memory().unwrap();
        store
            .upsert_track(&t(
                "1",
                "Singularity",
                "Stephan Bodzin",
                "Powers of Ten",
                2015,
            ))
            .unwrap();
        store
            .upsert_track(&t("2", "Nightflower", "ATSUGÁ", "Alive", 2019))
            .unwrap();
        store
            .upsert_track(&t("3", "La Femme", "air", "Moon Safari", 1998))
            .unwrap();
        store
            .upsert_track(&t(
                "4",
                "Sabali",
                "Amadou et Mariam",
                "Welcome to Mali",
                2008,
            ))
            .unwrap();

        let page = store.filter(&parse_query(""), SortPreset::ArtistAlbumTrack, 100, 0);

        // Folded sort: "air" ~ "Air", "ATSUGÁ" ~ "atsuga", case and accent
        // do not affect order — lowercase artists interleave with uppercase.
        let names: Vec<&str> = page.tracks.iter().map(|t| t.artist.as_str()).collect();
        assert_eq!(
            names,
            ["air", "Amadou et Mariam", "ATSUGÁ", "Stephan Bodzin"]
        );
    }

    #[test]
    fn sync_conflict_resolution() {
        let store = LibraryStore::open_memory().unwrap();
        store
            .upsert_track(&t("1", "So What", "Miles Davis", "Kind of Blue", 1959))
            .unwrap();
        let base = store.get_track("1").unwrap().updated_at;

        // Peer reports a newer revision: remote metadata wins, and the peer's
        // physical source is registered under its instance id.
        let mut newer = t("1", "So What (Take 2)", "Miles Davis", "Kind of Blue", 1959);
        newer.path = "/peer/so_what.flac".into();
        newer.updated_at = base + 1000;
        newer.last_played = Some(base + 500);
        store.apply_sync_payload(
            "peer-1",
            &SyncPayload {
                tracks: vec![newer],
                tombstones: vec![],
            },
        );

        let got = store.get_track("1").unwrap();
        assert_eq!(got.title, "So What (Take 2)");
        assert_eq!(got.last_played, Some(base + 500));
        assert!(got.is_local); // the local copy still exists
        assert_eq!(store.get_remote_source_peer("1").as_deref(), Some("peer-1"));

        // Peer reports an older revision with fresher play data: metadata is
        // kept, play data is adopted (highest last_played wins).
        let mut older = t("1", "Stale Title", "Miles Davis", "Kind of Blue", 1959);
        older.path = "/peer/so_what.flac".into();
        older.updated_at = base; // not newer than what we already have
        older.last_played = Some(base + 900);
        store.apply_sync_payload(
            "peer-1",
            &SyncPayload {
                tracks: vec![older],
                tombstones: vec![],
            },
        );

        let got = store.get_track("1").unwrap();
        assert_eq!(got.title, "So What (Take 2)");
        assert_eq!(got.last_played, Some(base + 900));
    }

    #[test]
    fn sync_tombstones_drop_peer_sources() {
        let store = LibraryStore::open_memory().unwrap();
        let local = t("1", "So What", "Miles Davis", "Kind of Blue", 1959);
        store.upsert_track(&local).unwrap();

        // Peer syncs the same track: it gains a remote source.
        let mut peer = t("1", "So What", "Miles Davis", "Kind of Blue", 1959);
        peer.path = "/peer/so_what.flac".into();
        store.apply_sync_payload(
            "peer-1",
            &SyncPayload {
                tracks: vec![peer],
                tombstones: vec![],
            },
        );
        assert_eq!(store.get_remote_source_peer("1").as_deref(), Some("peer-1"));

        // Peer deletes the file: the tombstone drops only the peer's source.
        store.apply_sync_payload(
            "peer-1",
            &SyncPayload {
                tracks: vec![],
                tombstones: vec!["1".into()],
            },
        );
        assert_eq!(store.get_remote_source_peer("1"), None);

        // The local copy is untouched: track and local source survive.
        let got = store.get_track("1").unwrap();
        assert!(got.is_local);
        assert_eq!(store.get_local_source_path("1"), Some(local.path));
    }
}
