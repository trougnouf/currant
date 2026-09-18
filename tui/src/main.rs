// ./tui/src/main.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Currant TUI entry point: opens the catalog, restores the live queue,
//! scans the configured roots, spawns the audio backend and runs the loop.

mod app;
mod audio;
mod control;
#[cfg(feature = "opus")]
mod opus;
mod ui;
mod zone;

use app::App;
use crossterm::{
    ExecutableCommand,
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use currant_core::controller::PlayerController;
use currant_core::model::PlayerIntent;
use currant_core::scanner::{ScanProgress, default_roots, scan_roots};
use currant_core::scrobble::ListenbrainzScrobbler;
use currant_core::store::LibraryStore;
use notify::{RecursiveMode, Watcher};
use ratatui::{Terminal, backend::CrosstermBackend};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Spawn a background thread that watches the scan roots and rescans when
/// files change. `roots_rx` delivers the roots to watch (empty = disabled);
/// `progress_tx` publishes the progress of each triggered rescan.
fn spawn_watcher(
    store: Arc<LibraryStore>,
    roots_rx: std::sync::mpsc::Receiver<Vec<String>>,
    progress_tx: std::sync::mpsc::Sender<Arc<ScanProgress>>,
) {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx).ok();

        let mut current_roots = Vec::new();
        loop {
            // Apply root changes (an empty list disables watching).
            while let Ok(new_roots) = roots_rx.try_recv() {
                if let Some(w) = &mut watcher {
                    for root in &current_roots {
                        let _ = w.unwatch(std::path::Path::new(root));
                    }
                    for root in &new_roots {
                        let _ = w.watch(std::path::Path::new(root), RecursiveMode::Recursive);
                    }
                }
                current_roots = new_roots;
            }

            // Wait for a file event, then debounce the burst before rescanning.
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(_)) => {
                    std::thread::sleep(Duration::from_millis(1000));
                    while rx.try_recv().is_ok() {}
                    let progress = ScanProgress::new();
                    let _ = progress_tx.send(progress.clone());
                    scan_roots(&store, &current_roots, &progress);
                }
                Ok(Err(_)) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    });
}

fn catalog_path() -> std::path::PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("currant");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("library.db")
}

fn main() -> Result<(), io::Error> {
    let store =
        Arc::new(LibraryStore::open(&catalog_path()).expect("failed to open library catalog"));

    let controller = Arc::new(Mutex::new(PlayerController::new(store.clone())));

    // Boot up the Mesh Networking Daemon
    let network_state = currant_core::net::start_network(store.clone(), controller.clone());

    // Restore the previous session's queue and volume, then rescan (incremental).
    if let Some(snap) = store.load_queue_snapshot() {
        controller.lock().unwrap().restore_queue(snap);
    }
    if let Some(v) = store.load_volume() {
        controller.lock().unwrap().volume = v;
    }

    // Wire the scrobbler if a token is configured (empty = disabled).
    let token = store.load_scrobble_token();
    if !token.is_empty() {
        controller
            .lock()
            .unwrap()
            .set_scrobbler(Arc::new(ListenbrainzScrobbler::new(token)));
    }

    let roots = store.load_roots();
    let roots = if roots.is_empty() {
        default_roots()
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
    } else {
        roots
    };

    // Scan in the background so the TUI starts immediately with live progress.
    let progress = ScanProgress::new();
    {
        let store = store.clone();
        let progress = progress.clone();
        let roots = roots.clone();
        std::thread::spawn(move || {
            scan_roots(&store, &roots, &progress);
        });
    }

    // Directory watching: the watcher thread rescans when files change.
    let (watcher_tx, watcher_rx) = std::sync::mpsc::channel();
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
    let (zone_tx, zone_rx) = std::sync::mpsc::channel();
    let remote_state = Arc::new(Mutex::new(None));
    zone::spawn(zone_rx, remote_state.clone(), store.clone());
    let _ = watcher_tx.send(if store.load_watch_roots() {
        roots.clone()
    } else {
        Vec::new()
    });
    spawn_watcher(store.clone(), watcher_rx, progress_tx);

    let playback = controller.lock().unwrap().playback_state.clone();
    crate::audio::spawn(controller.clone(), playback.clone(), network_state.clone());
    crate::control::spawn(controller.clone());

    let mut app = App::new();
    app.watcher_tx = Some(watcher_tx);
    app.network_state = Some(network_state);
    app.zone_tx = Some(zone_tx);
    app.remote_state = Some(remote_state);
    app.watcher_progress_rx = Some(progress_rx);
    app.set_scan_progress(progress);
    app.set_playback(playback);
    app.live_columns = store.load_live_columns();
    app.status = "scanning...".into();

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let result = run(&mut terminal, &mut app, &controller);

    // Persist the live queue and volume on exit.
    let snap = controller.lock().unwrap().queue_snapshot();
    store.save_queue_snapshot(&snap);
    store.save_volume(controller.lock().unwrap().volume);

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    io::stdout().execute(DisableMouseCapture)?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    controller: &Arc<Mutex<PlayerController>>,
) -> Result<(), io::Error> {
    // OS media integration (MPRIS on Linux, SMTC on Windows, Media Remote on
    // macOS). Events arrive on a channel and are dispatched in the loop.
    let mut controls = MediaControls::new(PlatformConfig {
        dbus_name: "currant",
        display_name: "Currant",
        hwnd: None,
    })
    .ok();
    let (mpris_tx, mpris_rx) = std::sync::mpsc::channel();
    if let Some(controls) = &mut controls {
        let _ = controls.attach(move |e| {
            let _ = mpris_tx.send(e);
        });
    }

    let mut last_track_id: Option<String> = None;
    let mut last_state = false;
    let mut last_progress_update = Instant::now();

    loop {
        // Refresh the view model from the controller (brief lock).
        {
            let mut c = controller.lock().unwrap();

            // Dispatch OS media events.
            while let Ok(event) = mpris_rx.try_recv() {
                match event {
                    MediaControlEvent::Toggle => c.dispatch(PlayerIntent::TogglePlayPause),
                    MediaControlEvent::Play if !c.is_playing => {
                        c.dispatch(PlayerIntent::TogglePlayPause)
                    }
                    MediaControlEvent::Pause if c.is_playing => {
                        c.dispatch(PlayerIntent::TogglePlayPause)
                    }
                    MediaControlEvent::Next => c.dispatch(PlayerIntent::NextTrack),
                    MediaControlEvent::Previous => c.dispatch(PlayerIntent::PreviousTrack),
                    MediaControlEvent::Stop => c.dispatch(PlayerIntent::ClearQueue),
                    _ => {}
                }
            }

            app.refresh(&c);

            if let Some(controls) = &mut controls {
                let playing = c.is_playing;
                let current_id = c.current_track.clone();

                if current_id != last_track_id {
                    if let Some(track) = c.current_track_ref() {
                        controls
                            .set_metadata(MediaMetadata {
                                title: Some(track.title.as_str()),
                                artist: Some(track.artist.as_str()),
                                album: Some(track.album.as_str()),
                                duration: Some(Duration::from_secs(track.duration_secs as u64)),
                                ..Default::default()
                            })
                            .ok();
                    } else {
                        controls.set_metadata(MediaMetadata::default()).ok();
                    }
                    last_track_id = current_id;
                }

                let progress = Some(MediaPosition(Duration::from_millis(app.position_ms())));
                if playing != last_state {
                    controls
                        .set_playback(if playing {
                            MediaPlayback::Playing { progress }
                        } else {
                            MediaPlayback::Paused { progress }
                        })
                        .ok();
                    last_state = playing;
                    last_progress_update = Instant::now();
                } else if playing && last_progress_update.elapsed() >= Duration::from_secs(1) {
                    // Keep the desktop position bar moving while playing.
                    controls
                        .set_playback(MediaPlayback::Playing { progress })
                        .ok();
                    last_progress_update = Instant::now();
                }
            }
        }

        // Update column widths with debounce before rendering.
        let size = terminal.size()?;
        app.update_col_widths(size.width as usize, size.height as usize);

        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(key) => {
                    let quit = {
                        let mut c = controller.lock().unwrap();
                        app.handle_key(key, &mut c)
                    };
                    if quit {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => {
                    let mut c = controller.lock().unwrap();
                    app.handle_mouse(mouse, &mut c, size.into());
                }
                _ => {}
            }
        }
    }
}
