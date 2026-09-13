# cassis

Fast and powerful music player with a Rust core and thin frontends.

The terminal frontend (`cassis-tui`) runs standalone — no server, no daemon.
A control socket lets `cassis-ctl` drive playback from scripts or keybindings.

![cassis-tui v0.0.1](https://commons.wikimedia.org/wiki/Special:FilePath/Cassis_music_player_v0.0.1_screenshot_(TUI).png)

## Features

- SQL-compiled query language: `artist:radiohead year:>=1997 *>=4 ~>5m -#pop`
- ratings and play counts written back to file tags (POPM), interop with Strawberry/Clementine/Amarok
- smart playlists saved from live searches
- random-album radio with continuation
- accent-insensitive search
- MP3, FLAC, OGG/Vorbis, WAV, Opus
- handles 100,000+ tracks without UI lag

Planned: Listenbrainz scrobbling, a Kotlin/Android client (Media3 + uniffi bindings to the core).

## Build

```
cargo build --release
```

Opus playback depends on libopus and libogg being installed on the system.

## Usage

```
cassis-tui            # launch the player
cassis-ctl status     # query playback state
cassis-ctl play-pause # toggle playback
```

See `SPECS.md` for the full spec: query syntax, keybindings, queue model, data flow.

## Support

If you enjoy using Cassis, consider supporting the developer:

- **Liberapay:** [https://liberapay.com/trougnouf](https://liberapay.com/trougnouf)
- **Ko-fi:** [https://ko-fi.com/trougnouf](https://ko-fi.com/trougnouf)
- **Bank (SEPA):** `BE77 9731 6116 6342`
- **Bitcoin:** `bc1qc3z9ctv34v0ufxwpmq875r89umnt6ggeclp979`
- **Litecoin:** `ltc1qv0xcmeuve080j7ad2cj2sd9d22kgqmlxfxvhmg`
- **Ethereum:** `0x0A5281F3B6f609aeb9D71D7ED7acbEc5d00687CB`
