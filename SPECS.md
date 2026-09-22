# Currant specifications

> This document is the source of truth for Currant's behavior, data model, and architecture. Update it whenever introducing a new feature, syntax token, setting, or architectural shift. Keep it concise, behavioral, and accurate.

---

## 1. Architecture

Currant is a fast offline music player with a Rust core and thin frontends (TUI now, Android later).

### 1.1. Crates

*   **`currant-core`** — pure logic: catalog (SQLite), query engine, scanner, controller, scrobble, control protocol. No audio, no UI.
*   **`currant-tui`** — terminal frontend (ratatui + rodio). Owns audio playback and rendering. Exposes a control socket for `currant-ctl`.
*   **`currant-ctl`** — remote control CLI. Connects to the TUI's control socket and dispatches `PlayerIntent`s or queries playback state.
*   **Android (future)** — Kotlin + Media3/ExoPlayer, bound to core via uniffi.

### 1.2. Data flow

*   **LibraryStore** — embedded SQLite (WAL mode). Source of truth for the catalog. Two connections: one read-only (queries never block on writes), one for mutations.
*   **PlayerController** — owns the live queue (in memory). Receives `PlayerIntent`s from frontends, applies state mutations, queries the store. Queue is snapshotted to the store on exit and restored on startup.
*   **Audio backend** — frontend-owned. The TUI spawns a thread that polls the controller's `current_track` and `is_playing` fields, decodes the file, and feeds rodio. When `current_track` is `None` and `is_playing` is true, the audio thread calls `determine_next_track()` to pull from the queue. When paused, the thread also corks the OS-level output stream, so desktop media indicators (e.g. KDE's active playback indicator) reflect the paused state.
*   **Scanner** — runs in a background thread. Reports progress via lock-free atomics (`ScanProgress`). Incremental: skips files whose mtime is unchanged since the last scan. New and changed tracks are upserted in batches of 500 inside a single transaction, so a full scan commits one fsync per batch instead of per track. Prunes removed files in a single transaction.
*   **Directory watcher** — an optional background thread using `notify` monitors root directories for changes. Updates are debounced and fed into the incremental scanner, keeping the UI in sync without manual rescans.
*   **Control socket & MPRIS** — the TUI listens on a Unix domain socket (`$XDG_RUNTIME_DIR/currant.sock`, mode 0600) for `currant-ctl`. Currant also implements MPRIS (Linux), SMTC (Windows), and Media Remote (macOS) using `souvlaki` for OS desktop integration (lock screen, tray icon, media keys).

### 1.3. Performance

*   The UI must never lag, even with 100,000+ tracks.
*   Queries are paginated (800-track window). The window reloads only when the selection scrolls outside it.
*   The cursor is centered vertically in the list area.
*   SQLite indexes on artist, album, title, genre, year, path, and a composite index on (album_artist_fold, album_fold, track_number, title_fold) for the default sort. Sort uses pre-folded shadow columns so ordering is accent- and case-insensitive, matching search.
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

The `kv` table stores: `queue_snapshot` (JSON), `roots` (JSON array), `volume` (float), `smart_playlists` (JSON array), `scrobble_token` (string, empty = scrobbling disabled), `pairing_token` (string, UUID generated on first launch), `schema_version` (integer).

On open, the store compares the stored `schema_version` against the current one. If it is older, the library tables (`tracks`, `track_sources`, `tombstones`, `tracks_with_local`) are dropped and the version bumped, forcing a fresh scan. The `kv` table is never touched, so user settings (roots, volume, token, playlists) survive the wipe. This guarantees deduplication correctness across breaking catalog changes (e.g. the switch from physical-path to metadata track IDs).

---

## 3. Query language

Evaluated instantly during search input. Compiles to SQL `WHERE` clauses.

### 3.1. Syntax

| Syntax | Meaning |
|---|---|
| `free text` | title, artist, album, or comment contains the text |
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

`ArtistAlbumTrack` (default), `YearDesc`, `MostPlayed`, `HighestRated`, `Random`, `RandomAlbum`, `Path` (files tab). All text sort keys use folded shadow columns for accent- and case-insensitive ordering.

---

## 4. Playback and queue

### 4.1. Queue model

*   **Explicit queue** — tracks the user specifically enqueued (play next = front, enqueue = back). Takes priority.
*   **Dynamic queue** — generated from the active search/filter. Refilled in batches of 50 when empty (random/radio modes only). In ordered mode the queue is not auto-refilled — playback stops when the explicit queue runs out. Recently played tracks are excluded. The active search is synced to the dynamic source when the user plays a track from the tracks/files tab, so Next always stays within the filtered set.
*   **History** — capped at 200 tracks, for the "previous" button and de-duplication.
*   **Radio** — toggled with `R`: random album → random → off. Default is random album. When off, playing a track from the filtered list enqueues the rest of that list in order; Next advances through it and stops at the end. When on (random or random album), the dynamic queue auto-refills from the filtered set.

### 4.2. Position, seeking, and ReplayGain

*   The audio thread publishes the playback position in milliseconds via a shared `PlaybackState` (lock-free atomics) owned by the `PlayerController` in core. The UI reads it each frame; the control protocol reports it in `ControlResponse`.
*   Seeking is requested through the same `PlaybackState`: `SeekTo` is resolved by the controller, which stores the target in milliseconds, and the audio thread calls `Player::try_seek` on the next loop iteration.
*   Symphonia-decoded formats (FLAC, MP3, OGG/Vorbis, WAV) seek natively via rodio. Opus files seek by adjusting the sample offset in the pre-decoded buffer.
*   The now-playing bar shows `position / duration` and a progress bar (Gauge widget).
*   ReplayGain is supported and applied at playback time. When enabled, it calculates a volume multiplier using `REPLAYGAIN_TRACK_GAIN` metadata tags extracted by lofty right before playback.

### 4.3. Intents

All frontends fire `PlayerIntent` into the controller:

*   `PlayTrack` — clear queue, enqueue the track, play immediately.
*   `Enqueue { next: bool }` — add to explicit queue (front = play next, back = append).
*   `RemoveFromQueue` — remove from explicit and dynamic queues.
*   `ClearQueue` — clear both queues.
*   `JumpTo` — skip to a track already in the queue without clearing the rest. Tracks before it go to history.
*   `TogglePlayPause`, `NextTrack`, `PreviousTrack`, `SkipAlbum`, `StopAfter { id }`. `StopAfter` marks a specific track (empty id = current track); playback halts when that track finishes. Pressing `StopAfter` on the same track toggles it off. `SkipAlbum` drops contiguous tracks of the current album from the queues and skips.
*   `SetVolume` — 0.0 to 1.0, persisted.
*   `SeekTo { position_ms }` — seek the current track to a position in milliseconds. The controller resolves it against its shared `PlaybackState` (see 4.2), so it works identically for local playback and when forwarded to a remote zone.
*   `RestoreSnapshot { queue, position_ms, is_playing }` — replaces the active queue and playback state. Used to hand off a session from one zone to another (see 8.4).
*   `RateTrack` — 0-5, persisted to catalog and file tag.
*   `SavePlaylist` / `ActivatePlaylist` / `DeletePlaylist` / `MovePlaylist { id, up }` — smart playlist management. `MovePlaylist` reorders (swaps with neighbor), changing the `g1`-`g9` index mapping.
*   `ScanLibrary` — set roots and trigger a scan.

### 4.4. Scrobbling

*   `Scrobbler` trait: `report(track, event)`.
*   Events: `NowPlaying` (on track start), `Submitted` (on track completion past threshold: half duration or 4 min, whichever is shorter).
*   Listenbrainz is implemented (ureq HTTP).
*   Scrobbling never blocks playback; errors are ignored. Each report runs on a short-lived thread with a bounded HTTP timeout, so a slow or unreachable server cannot stall the audio thread.
*   The Listenbrainz token is persisted in the `kv` store (`scrobble_token`). The TUI wires a `ListenbrainzScrobbler` on startup when the token is non-empty, and re-wires it live when the token is changed in the settings pane. An empty token disables scrobbling.

---

## 5. Supported formats

| Format | Decoder | Notes |
|---|---|---|
| MP3 | symphonia via rodio | |
| FLAC | symphonia via rodio | |
| OGG/Vorbis | symphonia via rodio | |
| WAV | symphonia via rodio | |
| Opus | libopus (opt-in `opus` feature) | rodio's symphonia 0.5 has no opus codec; bundled libopus decoder with seek support |

Metadata is read and written by lofty 0.25.

---

## 6. TUI

### 6.1. Tabs

`tracks` / `albums` / `artists` / `queue` / `playlists` / `files` — cycled with `Tab` / `Shift+Tab`.

### 6.2. Key bindings

| Key | Action |
|---|---|
| `/` | enter search mode (Esc to leave) |
| `Tab` / `Shift+Tab` | switch tabs |
| `j` `k` / arrows | move selection (PgUp/PgDn jump 20) |
| `Home` / `End` | first / last |
| `Enter` | play selection (track, album, artist) / jump to queue row / activate playlist |
| `e` | enqueue (append to queue) |
| `f` | play next (front of queue) |
| `x` | remove from queue (queue tab) / delete playlist (playlists tab) |
| `d` | show track details (path, metadata, etc.) |
| `o` | open the settings pane (scan roots, listenbrainz token, default volume) |
| `v` | expand album/artist to browse tracks (albums/artists tab, `v` again to collapse) |
| `Ctrl+J` | jump to the currently playing track in the current tab |
| `S` | stop after selected track (queue tab) or current track (elsewhere) |
| `0`-`5` | rate track (0 clears) |
| `c` | cycle columns (minimal / compact / full), persisted |
| `s` | cycle sort |
| `r` `R` | toggle radio (random album / random / off) |
| `p` | play / pause |
| `n` `>` `.` | next track |
| `N` | skip to the next album |
| `<` `,` | previous track |
| `+` `-` | volume up / down |
| `Left` `Right` / `h` `l` | seek backward / forward 5s |
| `H` `L` | seek backward / forward 30s |
| `P` | save current search as a smart playlist |
| `z` | cycle active zone (local, then each discovered peer in turn) |
| `T` | transfer the active session (queue, track, position) to the next zone |
| `g1`-`g9` | activate saved playlist by index |
| `J` `K` | move playlist down / up (playlists tab) |
| `?` | help overlay (keybindings, search syntax, playlists, about/support) — scroll with j/k/arrows/PgUp/PgDn |
| `q` / `Ctrl+C` | quit |
| `Esc` | close help / details overlay, exit search mode |

### 6.3. View presets

Track rows use fixed-width columns so fields align vertically. Column widths are computed from the visible items: each field gets its natural max width when everything fits; on overflow, remaining space is distributed to flex fields by ratio (artist:album:title:genre = 3:3:3:1). Year and duration are fixed-width. Widths are debounced (1s) so columns stay stable while scrolling. Jumps (jump-to-playing, auto-jump on track change, skip album) adopt immediately since they reposition rather than scroll. With the "live columns" setting enabled, the debounce is skipped entirely and widths follow the visible rows in real time.

*   **minimal** — artist, track number + title, duration
*   **compact** — + album, rating
*   **full** — + year, genre

The active preset is persisted in the `kv` store (`view_preset`) whenever it is cycled and restored on launch; an unknown or missing value falls back to compact.

### 6.4. Settings pane

Opened with `o` as a centered overlay. It lists the editable settings as rows: one row per scan root, an "+ add" row, the Listenbrainz token, the mesh pairing token, the default volume, and the replaygain, watch-roots, and live-columns toggles. `Up`/`Down` move the selection; `Enter` or `Space` toggles a bool row, or edits the selected row (an input line appears); `x` removes the selected scan root; `Esc` cancels an edit, or — when not editing — saves and closes.

*   **Scan roots** — the directories the scanner walks. Editing a root replaces it; `x` (or an empty value) removes it; "+ add" appends one. If the roots differ from when the pane opened, saving persists them and triggers an incremental rescan in the background. When none are saved yet, the pane is seeded with the default roots (XDG audio dir / `~/Music`).
*   **Listenbrainz token** — the API token for scrobbling. Saving re-wires the scrobbler live (empty disables it).
*   **Mesh pairing token** — the shared secret that authorizes peers on the local network (see §8.1). Generated on first launch; copy it to other devices to merge meshes. Saving takes effect immediately because the network layer reads the token from the store on every request and connection attempt.
*   **Default volume** — 0-100, clamped. Saving applies it to the current session and persists it.
*   **ReplayGain** — (default off) normalizes volume based on ReplayGain tags.
*   **Watch roots** — (default off) monitors scan roots for file system changes and auto-rescans.
*   **Live columns** — (default off) skips the column-width debounce so widths follow the visible rows in real time while scrolling.

---

## 7. CLI (`currant-ctl`)

Remote control for a running Currant instance. Connects to the TUI's control socket, sends a `ControlRequest`, and prints the resulting playback state. Does not play audio itself — the TUI (or a future daemon) owns the audio backend.

### 7.1. Protocol

One JSON line per request, one JSON line per response. `ControlRequest` is tagged with `type` (`"Intent"` or `"Status"`). `ControlResponse` contains `is_playing`, `volume`, `current_track` (full `Track` or null), `position_ms`, `stop_after`, `queue` (`QueueSnapshot`), and an optional `error` field.

### 7.2. Commands

| Command | Maps to | Notes |
|---|---|---|
| `play-pause` | `TogglePlayPause` | |
| `next` | `NextTrack` | |
| `prev` | `PreviousTrack` | |
| `skip-album` | `SkipAlbum` | |
| `stop-after [id]` | `StopAfter { id }` | empty id = current track; toggles off if same |
| `clear` | `ClearQueue` | |
| `volume <0-100>` | `SetVolume` | percentage, clamped |
| `play <id>` | `PlayTrack` | play immediately |
| `enqueue <id>` | `Enqueue { next: false }` | append to queue |
| `play-next <id>` | `Enqueue { next: true }` | front of queue |
| `rate <id> <0-5>` | `RateTrack` | |
| `status` | (no intent) | query current state |

### 7.3. Standalone daemon (future)

A `currant-daemon` process (like cfait's `cfait daemon`) would own the `PlayerController` + audio backend without a TUI, exposing the same control socket. The TUI would become a client of the daemon. This is deferred until headless playback is needed; the current in-TUI socket does not paint into a corner.

### 7.4. Android

Android does not use the control socket. The app owns `PlayerController` in-process (via uniffi) and exposes remote control through Android's MediaSession API (notification, lock screen, Bluetooth, `adb shell`).


## 8. Networking & Mesh Audio (Currant-Net)

Currant instances operate as peers in a decentralized mesh. An instance dynamically assumes any combination of three roles: **Library Provider** (shares files), **Playback Target** (owns a queue and outputs audio), and **Controller** (UI).

### 8.1. Unified Topology & Discovery
*   **Identity:** On first launch, each instance generates a UUID `instance_id` stored in the `kv` table and cached in memory for the life of the process.
*   **Discovery (mDNS):** Instances automatically discover each other on the local network via Zeroconf/mDNS.
*   **Protocol:** Every instance runs a lightweight HTTP server.
    *   `/ws` - WebSocket endpoint for JSON command/state sync (`PlayerIntent` and `ControlResponse`).
    *   `/stream/:logical_id` - HTTP endpoint for media streaming (supports `Range` requests).
    *   `/sync` - HTTP endpoint for exchanging catalog deltas.
*   **Authentication:** Devices are paired via a shared 128-bit pairing token, generated on first launch and stored in `kv` (`pairing_token`); it is viewable and editable in the settings pane. The token deterministically seeds two Ed25519 key pairs — one for a root CA and one for the leaf certificate (SAN `currant.local`) it signs — which both peers derive locally, so no certificate exchange is ever needed. The leaf stays a non-CA: webpki rejects CA certificates as end entities.
    *   **Control layer (`/ws`):** strict TLS. Each side wraps the WebSocket upgrade in a rustls connection that verifies the peer's token-derived certificate against the locally derived root, ignoring the hostname/IP (an IP may roam); the `Authorization: Bearer <token>` header is then sent inside the verified tunnel. A man-in-the-middle cannot complete the handshake without the token, so the state-mutating control channel is immune to MITM and replay.
    *   **Media layer (`/sync`, `/stream`):** unverified TLS. The tunnel is encrypted (blocking passive eavesdropping) but not verified, so the token is never transmitted over it. Each read-only request instead carries a static `Authorization: Currant <hmac-sha256(uri)>` header keyed with the token, bound to the URI to prevent cross-endpoint use. Replay is harmless because these endpoints are read-only.
    *   The token is read from the store on each request/connection attempt, so rotating it in the settings pane takes effect immediately without a restart (it changes both the HMAC key and the derived certificate).

### 8.2. Global Catalog & Smart Deduplication
To avoid transferring files that already exist locally (regardless of folder structures), Currant deduplicates tracks using metadata hashes.
*   **Logical IDs:** The core `tracks` table represents *Logical Tracks*. Its `id` is the FNV-1a hash of the folded metadata (`album_artist + album + track_number + title`).
*   **Physical Sources:** A new `track_sources` table maps `logical_id -> (instance_id, full_path, format_tier)`. `format_tier` distinguishes Lossless (FLAC/WAV) from Lossy (Opus/OGG/MP3).
*   **Source Resolution Priority:** When a Playback Target requests a track, it picks the optimal physical source based on a new setting `prefer_remote_lossy` (default: `true`):
    1.  **Local Source Available:** Always play local. Zero network transfer. (Lossless > Lossy).
    2.  **Only Remote Sources Available:**
        *   If `prefer_remote_lossy` is `true`: Remote Lossy > Remote Lossless (minimizes bandwidth).
        *   If `prefer_remote_lossy` is `false`: Remote Lossless > Remote Lossy.

### 8.3. Strict Delta Synchronization
To guarantee minimal data transfer, the SQLite database is **never** transferred whole. Syncs use High-Water Mark (HWM) timestamping.
*   **Timestamps:** The `tracks` table tracks modifications via `updated_at`.
*   **Tombstones:** A new `tombstones` table tracks `(logical_id, deleted_at)` when tracks are removed.
*   **Delta Sync Payload:** When Node A connects to Node B, it requests `GET /sync?since=<last_sync_timestamp>`. Node B returns a compact JSON payload containing *only* the tracks modified/added, and the IDs deleted, since that exact timestamp.
*   **Conflict Resolution:** For user data (`rating`, `play_count`), the highest `last_played` timestamp wins, ensuring offline plays sync safely across devices.

### 8.4. Playback Targets & Zones
*   The `PlayerController` state (Queue, Now Playing, Volume) belongs to the **Playback Target**.
*   **Independent Queues:** A PC and a Phone maintain independent queues by default.
*   **Remote Control:** The UI can switch its active "Zone". If the Phone selects the PC Zone, the Phone UI forwards all `PlayerIntent` keypresses over WebSocket to the PC.
*   **Zone Cycling:** `z` in the TUI cycles the active zone through the discovered peers (sorted by instance id), wrapping back to local. Switching zones resets the remote state; the zone client thread reconnects and requests a status snapshot from the new target.
*   **Remote UI State:** While a zone is active, the TUI renders the remote target's state instead of the local one: Now Playing, queue (explicit + dynamic), volume, play/pause, position, and stop-after come from the target's `ControlResponse` (polled over the WebSocket). Seek and stop-after intents are forwarded like any other keypress. Local playback is left untouched; switching back to local restores the local view.
*   **Zone Handoff:** `T` (Shift+T) transfers the active session to the next zone. The UI pauses the current zone, cycles to the next peer, and sends a `RestoreSnapshot` intent carrying the exact queue, track, position, and play state. This achieves seamless multi-room handoff.
*   **Scrobbling:** Only the active Playback Target executes scrobbles, preventing duplicated API calls.

### 8.5. Audio Streaming & UI Indicators
*   **Pull-based HTTP:** Streaming is handled via `GET /stream/:logical_id`. The responding node streams the file using `Accept-Ranges: bytes`.
*   **Decoding:** The consuming node wraps the HTTP stream in a seekable reader. *(Note: The custom `opus.rs` decoder must be updated to stream chunks rather than decoding the full file into memory upfront).*
*   **UI Hint:** In the TUI, tracks without a local source (requiring network streaming) are prefixed with a network symbol (e.g., `~`) indicating playback will consume Wi-Fi/mobile bandwidth.

## 9. Future work

Not yet implemented, listed for priority tracking:

*   Android app (Kotlin + Media3, uniffi bindings)
*   "Send tracks" (Android share intent + desktop)
*   Gapless playback
