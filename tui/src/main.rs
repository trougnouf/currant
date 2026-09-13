// ./tui/src/main.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Framboise TUI entry point: opens the catalog, restores the live queue,
//! scans the configured roots, spawns the audio backend and runs the loop.

mod app;
mod audio;
#[cfg(feature = "opus")]
mod opus;
mod ui;

use app::App;
use crossterm::{
    ExecutableCommand,
    event::{self, Event},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use framboise_core::controller::PlayerController;
use framboise_core::scanner::{ScanProgress, default_roots, scan_roots};
use framboise_core::store::LibraryStore;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn catalog_path() -> std::path::PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("framboise");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("library.db")
}

fn main() -> Result<(), io::Error> {
    let store =
        Arc::new(LibraryStore::open(&catalog_path()).expect("failed to open library catalog"));
    let mut controller = PlayerController::new(store.clone());

    // Restore the previous session's queue and volume, then rescan (incremental).
    if let Some(snap) = store.load_queue_snapshot() {
        controller.restore_queue(snap);
    }
    if let Some(v) = store.load_volume() {
        controller.volume = v;
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

    let controller = Arc::new(Mutex::new(controller));
    crate::audio::spawn(controller.clone());

    let mut app = App::new();
    app.set_scan_progress(progress);
    app.status = "scanning...".into();

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let result = run(&mut terminal, &mut app, &controller);

    // Persist the live queue and volume on exit.
    let snap = controller.lock().unwrap().queue_snapshot();
    store.save_queue_snapshot(&snap);
    store.save_volume(controller.lock().unwrap().volume);

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    controller: &Arc<Mutex<PlayerController>>,
) -> Result<(), io::Error> {
    loop {
        // Refresh the view model from the controller (brief lock).
        {
            let c = controller.lock().unwrap();
            app.refresh(&c);
        }

        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(30))?
            && let Event::Key(key) = event::read()?
        {
            let quit = {
                let mut c = controller.lock().unwrap();
                app.handle_key(key, &mut c)
            };
            if quit {
                return Ok(());
            }
        }
    }
}
