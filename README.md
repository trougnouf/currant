# cassis

Fast and powerful music player with a Rust core and thin frontends.

The terminal frontend (`cassis-tui`) runs standalone — no server, no daemon.
A control socket lets `cassis-ctl` drive playback from scripts or keybindings.

## features

- SQL-compiled query language: `artist:radiohead year:>=1997 *>=4 ~>5m -#pop`
- ratings and play counts written back to file tags (POPM), interop with Strawberry/Clementine/Amarok
- smart playlists saved from live searches
- random-album radio with continuation
- accent-insensitive search
- MP3, FLAC, OGG/Vorbis, WAV, Opus
- handles 100,000+ tracks without UI lag

Planned: Listenbrainz scrobbling, a Kotlin/Android client (Media3 + uniffi bindings to the core).

## build

```
cargo build --release
```

Opus playback depends on libopus and libogg being installed on the system.

## usage

```
cassis-tui            # launch the player
cassis-ctl status     # query playback state
cassis-ctl play-pause # toggle playback
```

See `SPECS.md` for the full spec: query syntax, keybindings, queue model, data flow.
