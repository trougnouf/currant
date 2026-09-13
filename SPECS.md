# Cassis specifications

> This document is the source of truth for Cassis's behavior, data model, and architecture. Update it whenever introducing a new feature, syntax token, setting, or architectural shift. Keep it concise, behavioral, and accurate.

---

## 1. Architecture

Cassis is a fast, offline-first music player with a Rust core and thin frontends (TUI now, Android later).

### 1.1. Crates

*   **`cassis-core`** — pure logic: catalog (SQLite), query engine, scanner, controller, scrobble. No audio, no UI.
*   **`cassis-tui`** — terminal frontend (ratatui + rodio). Owns audio playback and rendering.
*   **Android (future)** — Kotlin + Media3/ExoPlayer, bound to core via uniffi.

### 1.2. Data flow

*   **LibraryStore** — embedded SQLite (WAL mode). Source of truth for the catalog. Two connections: one read-only (queries never block on writes), one for mutations.
*   **PlayerController** — owns the live queue (in memory). Receives `PlayerIntent`s from frontends, applies state mutations, queries the store. Queue is snapshotted to the store on exit and restored on startup.
*   **Audio backend** — frontend-owned. The TUI spawns a thread that polls the controller's `current_track` and `is_playing` fields, decodes the file, and feeds rodio. When `current_track` is `None` and `is_playing` is true, the audio thread calls `determine_next_track()` to pull from the queue.
*   **Scanner** — runs in a background thread. Reports progress via lock-free atomics (`ScanProgress`). Incremental: skips files whose mtime is unchanged since the last scan. Prunes removed files in a single transaction.

### 1.3. Performance

*   The UI must never lag, even with 100,000+ tracks.
*   Queries are paginated (800-track window). The window reloads only when the selection scrolls outside it.
*   SQLite indexes on artist, album, title, genre, year, path, and a composite index on (artist, album, track_number, title) for the default sort.
*   The read connection serves the pre-scan catalog state via WAL while the scan thread writes.

---

## 2. The catalog

### 2.1. Track model

| Field | Type | Notes |
|---|---|---|
| id | TEXT PK | FNV-1a 64-bit hash of the canonical path |
| path | TEXT | absolute file path |
| title | TEXT | from tag, or filename stem |
| artist | TEXT | from tag, or "Unknown Artist" |
| album | TEXT | from tag, or "Unknown Album" |
| genre | TEXT | from tag |
| comment | TEXT | from tag |
| track_number | INTEGER | from tag |
| year | INTEGER | from tag (first 4 digits) |
| duration_secs | INTEGER | from properties |
| rating | INTEGER | 0-5, user-managed, written back to file tag |
| play_count | INTEGER | user-managed, written back to file tag |
| last_played | INTEGER | unix timestamp, or NULL |
| file_mtime | INTEGER | unix seconds, for incremental rescans |

### 2.2. Rating and play count persistence

*   Ratings use the MusicBee-style popularimeter (POPM frame for ID3v2, Vorbis Comments for FLAC/OGG/Opus) so they interoperate with Amarok/Clementine/Strawberry.
*   Rating 0 removes the popularimeter entry.
*   On rescan with unchanged mtime, rating/play_count/last_played are preserved from the existing catalog row. On changed mtime, they reset to the file's tagged values.

### 2.3. Key/value store

The `kv` table stores: `queue_snapshot` (JSON), `roots` (JSON array), `volume` (float), `smart_playlists` (JSON array).

---

## 3. Query language

Evaluated instantly during search input. Compiles to SQL `WHERE` clauses.

### 3.1. Syntax

| Syntax | Meaning |
|---|---|
| `free text` | title, artist, or album contains the text |
| `ar:text` / `artist:text` | artist contains text |
| `al:text` / `album:text` | album contains text |
| `t:text` / `title:text` | title contains text |
| `g:text` / `genre:text` | genre contains text |
| `c:text` / `comment:text` | comment contains text |
| `#jazz` | genre contains "jazz" (shorthand) |
| `year:>=1990` | year >= 1990 (also `>`, `<`, `<=`, `=`, `!=`) |
| `*>=4` | rating >= 4 (also `*3`, `*<=2`, etc.) |
| `~>5m` | duration > 5 minutes (also `~180s`, `~1h30m`) |
| `p:0` | play count = 0 (also `p:>=10`, etc.) |
| `-term` | NOT (exclude) |
| `a \| b` | OR |
| `a b` | implicit AND |
| `(a b)` | grouping |
| `"quoted text"` | quoted (spaces preserved) |

### 3.2. Operators

`contains` (default for text), `=` (exact, case-insensitive), `!=` (not equal), `>`, `>=`, `<`, `<=` (numeric fields).

### 3.3. Sort presets

`ArtistAlbumTrack` (default), `YearDesc`, `MostPlayed`, `HighestRated`, `Random`, `RandomAlbum`, `Path` (files tab).

---

## 4. Playback and queue

### 4.1. Queue model

*   **Explicit queue** — tracks the user specifically enqueued (play next = front, enqueue = back). Takes priority.
*   **Dynamic queue** — generated from the active smart playlist / search. Refilled in batches of 50 when empty. Recently played tracks are excluded.
*   **History** — capped at 200 tracks, for the "previous" button and de-duplication.
*   **Random album** — picks one album at random, plays it in track order. Activated with `R`.

### 4.2. Intents

All frontends fire `PlayerIntent` into the controller:

*   `PlayTrack` — clear queue, enqueue the track, play immediately.
*   `Enqueue { next: bool }` — add to explicit queue (front = play next, back = append).
*   `RemoveFromQueue` — remove from explicit and dynamic queues.
*   `ClearQueue` — clear both queues.
*   `TogglePlayPause`, `NextTrack`, `PreviousTrack`, `StopAfterCurrent`.
*   `SetVolume` — 0.0 to 1.0, persisted.
*   `RateTrack` — 0-5, persisted to catalog and file tag.
*   `SavePlaylist` / `ActivatePlaylist` / `DeletePlaylist` — smart playlist management.
*   `ScanLibrary` — set roots and trigger a scan.

### 4.3. Scrobbling

*   `Scrobbler` trait: `report(track, event)`.
*   Events: `NowPlaying` (on track start), `Submitted` (on track completion past threshold: half duration or 4 min, whichever is shorter).
*   Listenbrainz is implemented (ureq HTTP). Last.fm is a future addition.
*   Scrobbling never blocks playback; errors are ignored.

---

## 5. Supported formats

| Format | Decoder | Notes |
|---|---|---|
| MP3 | symphonia via rodio | |
| FLAC | symphonia via rodio | |
| OGG/Vorbis | symphonia via rodio | |
| WAV | symphonia via rodio | |
| Opus | libopus (opt-in `opus` feature) | rodio's symphonia 0.5 has no opus codec; fallback to bundled libopus decoder |

Metadata is read and written by lofty 0.25.

---

## 6. TUI

### 6.1. Tabs

`tracks` / `albums` / `artists` / `queue` / `files` — cycled with `Tab` / `Shift+Tab`.

### 6.2. Key bindings

| Key | Action |
|---|---|
| `/` | enter search mode (Esc to leave) |
| `Tab` / `Shift+Tab` | switch tabs |
| `j` `k` / arrows | move selection (PgUp/PgDn jump 20) |
| `Home` / `End` | first / last |
| `Enter` | play selection (track, album, artist, queue row) |
| `q` | enqueue (append to queue) |
| `n` | play next (front of queue) |
| `x` | remove from queue (queue tab) |
| `s` | stop after current |
| `0`-`5` | rate track (0 clears) |
| `c` | cycle columns (minimal / compact / full) |
| `r` | cycle sort |
| `m` | set radio from current search |
| `R` | random-album radio |
| `p` | play / pause |
| `>` `.` | next track |
| `<` `,` | previous track |
| `+` `-` | volume up / down |
| `P` | save current search as a smart playlist |
| `g1`-`g9` | activate saved playlist by index |
| `?` | help overlay (search syntax + playlists) |
| `Ctrl+C` / `Esc` | quit |

### 6.3. View presets

*   **minimal** — title + duration
*   **compact** — + rating, artist, album
*   **full** — + year, genre

---

## 7. Future work

Not yet implemented, listed for priority tracking:

*   Android app (Kotlin + Media3, uniffi bindings)
*   Last.fm scrobbler
*   "Send tracks" (Android share intent + desktop)
*   Settings UI for scan roots, scrobble token, volume default
*   Seek / position display
*   Playlist tab (dedicated tab for saved smart playlists)
*   Gapless playback
*   ReplayGain
