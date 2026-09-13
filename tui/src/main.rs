// ./tui/src/main.rs
use crossterm::{
    ExecutableCommand,
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use framboise_core::controller::PlayerController;
use framboise_core::matcher::parse_query;
use framboise_core::model::{PlayerIntent, SortPreset};
use framboise_core::scanner::scan_directory;
use framboise_core::store::LibraryStore;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use rodio::{Decoder, OutputStream, Sink};
use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), io::Error> {
    let store = Arc::new(Mutex::new(LibraryStore::new()));
    let controller = Arc::new(Mutex::new(PlayerController::new(store.clone())));

    // SCANNER
    let music_dir = Path::new("./music");
    if music_dir.exists() {
        let tracks = scan_directory(music_dir);
        let mut s = store.lock().await;
        for track in tracks {
            s.add_track(track);
        }
    }

    // AUDIO BACKGROUND THREAD
    let audio_controller = controller.clone();
    std::thread::spawn(move || {
        let (_stream, stream_handle) = OutputStream::try_default().unwrap();
        let sink = Sink::try_new(&stream_handle).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();

        loop {
            // Check state
            let (is_playing, track_is_none) = rt.block_on(async {
                let c = audio_controller.lock().await;
                (c.is_playing, c.current_track.is_none())
            });

            // Handle Pause/Play
            if !is_playing {
                sink.pause();
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            } else {
                sink.play();
            }

            // Handle track progression or explicit skips
            if track_is_none || sink.empty() {
                let next_track_opt = rt.block_on(async {
                    let mut c = audio_controller.lock().await;
                    let fallback_query = parse_query(""); // Default: play everything
                    c.determine_next_track(&fallback_query).await
                });

                if let Some(track_id) = next_track_opt {
                    let path = rt.block_on(async {
                        audio_controller
                            .lock()
                            .await
                            .store
                            .lock()
                            .await
                            .tracks
                            .get(&track_id)
                            .unwrap()
                            .path
                            .clone()
                    });

                    // Clear the sink in case we skipped manually
                    sink.clear();

                    if let Ok(file) = File::open(path)
                        && let Ok(decoder) = Decoder::new(BufReader::new(file))
                    {
                        sink.append(decoder);
                        sink.play();
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    });

    // TUI SETUP
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut search_query = String::new();
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    // MAIN EVENT LOOP
    loop {
        let c = controller.lock().await;
        let s = c.store.lock().await;

        let ast = parse_query(&search_query);
        let results = s.filter(&ast, SortPreset::ArtistAlbumTrack);
        let results_len = results.len();

        // Ensure list selection is within bounds
        if let Some(i) = list_state.selected() {
            if i >= results_len && results_len > 0 {
                list_state.select(Some(results_len - 1));
            } else if results_len == 0 {
                list_state.select(None);
            }
        } else if results_len > 0 {
            list_state.select(Some(0));
        }

        let playing_id = c.current_track.clone();

        // 1. Build List Items
        let items: Vec<ListItem> = results
            .iter()
            .map(|t| {
                let prefix = if Some(&t.id) == playing_id.as_ref() {
                    "▶ "
                } else {
                    "  "
                };
                let duration = format!("{:02}:{:02}", t.duration_secs / 60, t.duration_secs % 60);
                ListItem::new(format!(
                    "{}{} - {} [{}]",
                    prefix, t.title, t.artist, duration
                ))
            })
            .collect();

        // 2. Build Now Playing Text
        let np_text = if let Some(id) = &playing_id {
            if let Some(t) = s.tracks.get(id) {
                let status = if c.is_playing { "Playing" } else { "Paused" };
                format!(
                    "▶ {} - {} | Album: {} | [{}]",
                    t.title, t.artist, t.album, status
                )
            } else {
                "Unknown Track".to_string()
            }
        } else {
            "Stopped. Press Enter on a track to play.".to_string()
        };

        // RENDER
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Search
                    Constraint::Min(0),    // Library
                    Constraint::Length(3), // Now Playing
                ])
                .split(f.size()); // <--- using f.size() to support ratatui 0.26

            let search_block = Paragraph::new(search_query.clone())
                .block(Block::default().title("Search").borders(Borders::ALL));
            f.render_widget(search_block, chunks[0]);

            let list = List::new(items)
                .block(
                    Block::default()
                        .title("Library (Up/Down to navigate, Enter to play)")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol(">> ");
            f.render_stateful_widget(list, chunks[1], &mut list_state);

            let np_block = Paragraph::new(np_text).block(
                Block::default()
                    .title("Now Playing (Ctrl+P Play/Pause | Ctrl+N Next | Ctrl+B Prev)")
                    .borders(Borders::ALL),
            );
            f.render_widget(np_block, chunks[2]);
        })?;

        drop(s);
        drop(c);

        // INPUT HANDLING
        if event::poll(std::time::Duration::from_millis(16))?
            && let Event::Key(key) = event::read()?
        {
            // Handle Control Modifiers
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('c') => break,                // Quit
                    KeyCode::Char('u') => search_query.clear(), // Clear search
                    KeyCode::Char('p') => {
                        // Play / Pause
                        controller
                            .lock()
                            .await
                            .dispatch(PlayerIntent::TogglePlayPause)
                            .await;
                    }
                    KeyCode::Char('n') => {
                        // Skip track
                        let mut c = controller.lock().await;
                        c.current_track = None;
                    }
                    KeyCode::Char('b') => {
                        // Previous track
                        controller.lock().await.previous_track().await;
                    }
                    _ => {}
                }
                continue;
            }

            // Standard Keys
            match key.code {
                KeyCode::Esc => break,
                KeyCode::Up => {
                    let i = list_state.selected().unwrap_or(0);
                    list_state.select(Some(i.saturating_sub(1)));
                }
                KeyCode::Down => {
                    let i = list_state.selected().unwrap_or(0);
                    if i + 1 < results_len {
                        list_state.select(Some(i + 1));
                    }
                }
                KeyCode::Char(c) => search_query.push(c),
                KeyCode::Backspace => {
                    search_query.pop();
                }
                KeyCode::Enter => {
                    let ast = parse_query(&search_query);
                    let selected_id = {
                        let c = controller.lock().await;
                        let s = c.store.lock().await;
                        if let Some(idx) = list_state.selected() {
                            s.filter(&ast, SortPreset::ArtistAlbumTrack)
                                .get(idx)
                                .map(|t| t.id.clone())
                        } else {
                            None
                        }
                    };

                    if let Some(id) = selected_id {
                        let mut c = controller.lock().await;
                        c.dispatch(PlayerIntent::ClearQueue).await;
                        c.dispatch(PlayerIntent::Enqueue { id, next: true }).await;
                        c.current_track = None; // Force audio thread to skip to the new track immediately
                    }
                }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
