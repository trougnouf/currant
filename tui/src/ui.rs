// ./tui/src/ui.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering. Reads the `App` view model (built each frame) and lays out the
//! tabs, the active list and the now-playing bar.

use crate::app::{App, QueueKind, Tab, ViewPreset, display_title, fmt_duration, render_rating};
use cassis_core::model::Track;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Tabs};

/// Color palette for the TUI.
mod theme {
    use ratatui::style::Color;
    pub const ACCENT: Color = Color::Cyan;
    pub const ACCENT_DIM: Color = Color::DarkGray;
    pub const TITLE: Color = Color::Yellow;
    pub const NOW_PLAYING: Color = Color::Green;
    pub const PAUSED: Color = Color::Yellow;
    pub const STOP_AFTER: Color = Color::Red;
    pub const RATING: Color = Color::Magenta;
    pub const DURATION: Color = Color::DarkGray;
    pub const ARTIST: Color = Color::Blue;
    pub const ALBUM: Color = Color::Cyan;
    pub const YEAR: Color = Color::DarkGray;
    pub const GENRE: Color = Color::Green;
    pub const PATH: Color = Color::DarkGray;
    pub const QUEUE_NOW: Color = Color::Green;
    pub const QUEUE_EXPLICIT: Color = Color::Yellow;
    pub const QUEUE_DYNAMIC: Color = Color::DarkGray;
    pub const GAUGE: Color = Color::Cyan;
    pub const HINT: Color = Color::DarkGray;
    pub const POPUP_BORDER: Color = Color::Cyan;
}

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tabs + search
            Constraint::Min(0),    // list
            Constraint::Length(5), // now playing + progress + status
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_list(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    if app.help {
        let area = centered(f.area(), 70, 80);
        f.render_widget(Clear, area);
        draw_help(f, area, app);
    }
    if let Some(track) = &app.details {
        let area = centered(f.area(), 70, 60);
        f.render_widget(Clear, area);
        draw_details(f, area, track);
    }
}

/// A centered rect inside `area` with the given width/height percentages.
fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(popup[1])[1]
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(60), Constraint::Min(0)])
        .split(area);

    let titles = ["tracks", "albums", "artists", "queue", "playlists", "files"]
        .iter()
        .map(|t| Line::from(*t))
        .collect::<Vec<_>>();
    let active = match app.tab {
        Tab::Tracks => 0,
        Tab::Albums => 1,
        Tab::Artists => 2,
        Tab::Queue => 3,
        Tab::Playlists => 4,
        Tab::Files => 5,
    };
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Cassis")
                .title_style(Style::default().fg(theme::TITLE).bold()),
        )
        .select(active)
        .style(Style::default())
        .highlight_style(
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, cols[0]);

    let prompt = if app.in_search {
        "search>"
    } else {
        "search ('/ to type)"
    };
    let search = Paragraph::new(format!("{} {}", prompt, app.search)).block(
        Block::default()
            .borders(Borders::ALL)
            .title("filter")
            .title_style(Style::default().fg(theme::TITLE)),
    );
    f.render_widget(search, cols[1]);
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    match app.tab {
        Tab::Tracks => draw_tracks(f, app, area, app.tracks_view(), "tracks"),
        Tab::Files => draw_tracks(f, app, area, app.files_view(), "files"),
        Tab::Albums => draw_albums(f, app, area),
        Tab::Artists => draw_artists(f, app, area),
        Tab::Queue => draw_queue(f, app, area),
        Tab::Playlists => draw_playlists(f, app, area),
    }
}

fn draw_tracks(
    f: &mut Frame,
    app: &App,
    area: Rect,
    (items, selected, total, offset): (&[Track], usize, u64, usize),
    title: &str,
) {
    let title = format!("{title} - {} of {} (from #{})", items.len(), total, offset);
    let playing_id = app.now_playing.as_ref().map(|t| &t.id);
    let rows: Vec<ListItem> = items
        .iter()
        .map(|t| ListItem::new(track_line(t, app.view, playing_id == Some(&t.id))))
        .collect();
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn track_line(t: &Track, view: ViewPreset, playing: bool) -> Line<'_> {
    let marker = if playing { ">" } else { " " };
    let marker_style = if playing {
        Style::default().fg(theme::NOW_PLAYING).bold()
    } else {
        Style::default()
    };
    let rating = render_rating(t.rating);
    let title = display_title(t);
    let title_style = if playing {
        Style::default().fg(theme::NOW_PLAYING).bold()
    } else {
        Style::default().bold()
    };
    let dur = fmt_duration(t.duration_secs);
    let dur_style = Style::default().fg(theme::DURATION);
    let artist_style = Style::default().fg(theme::ARTIST);
    let album_style = Style::default().fg(theme::ALBUM);
    let year_style = Style::default().fg(theme::YEAR);
    let genre_style = Style::default().fg(theme::GENRE);
    let spans: Vec<Span> = match view {
        ViewPreset::Minimal => vec![
            Span::styled(format!("{marker} "), marker_style),
            Span::styled(title, title_style),
            Span::raw(" "),
            Span::styled(dur, dur_style),
        ],
        ViewPreset::Compact => vec![
            Span::styled(
                format!("{marker} {rating} "),
                marker_style.fg(theme::RATING),
            ),
            Span::styled(title, title_style),
            Span::raw(" - "),
            Span::styled(t.artist.clone(), artist_style),
            Span::raw(" ["),
            Span::styled(t.album.clone(), album_style),
            Span::raw("] "),
            Span::styled(dur, dur_style),
        ],
        ViewPreset::Full => vec![
            Span::styled(
                format!("{marker} {rating} "),
                marker_style.fg(theme::RATING),
            ),
            Span::styled(title, title_style),
            Span::raw(" - "),
            Span::styled(t.artist.clone(), artist_style),
            Span::raw(" ["),
            Span::styled(t.album.clone(), album_style),
            Span::raw("] "),
            Span::styled(
                if t.year > 0 {
                    t.year.to_string()
                } else {
                    String::new()
                },
                year_style,
            ),
            Span::raw(" "),
            Span::styled(t.genre.clone(), genre_style),
            Span::raw(" "),
            Span::styled(dur, dur_style),
        ],
    };
    Line::from(spans)
}

fn draw_albums(f: &mut Frame, app: &App, area: Rect) {
    let (albums, selected) = app.albums_view();
    let rows: Vec<ListItem> = albums
        .iter()
        .map(|a| {
            ListItem::new(format!(
                "{} - {} [{}] ({} tracks, {})",
                a.artist,
                a.album,
                if a.year > 0 {
                    a.year.to_string()
                } else {
                    "?".into()
                },
                a.track_count,
                fmt_duration(a.total_duration_secs)
            ))
        })
        .collect();
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("albums - {}", albums.len()))
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_artists(f: &mut Frame, app: &App, area: Rect) {
    let (artists, selected) = app.artists_view();
    let rows: Vec<ListItem> = artists
        .iter()
        .map(|a| {
            ListItem::new(format!(
                "{} ({} albums, {} tracks)",
                a.name, a.album_count, a.track_count
            ))
        })
        .collect();
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("artists - {}", artists.len()))
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_queue(f: &mut Frame, app: &App, area: Rect) {
    let (rows, selected) = app.queue_view();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let (mark, color) = match r.kind {
                QueueKind::NowPlaying => (">", theme::QUEUE_NOW),
                QueueKind::Explicit => ("+", theme::QUEUE_EXPLICIT),
                QueueKind::Dynamic => ("~", theme::QUEUE_DYNAMIC),
            };
            Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color).bold()),
                Span::raw(r.title.clone()),
                Span::raw(" - "),
                Span::styled(r.artist.clone(), Style::default().fg(theme::ARTIST)),
            ])
            .into()
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("queue - {} (Enter:jump to  x:remove)", rows.len()))
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_playlists(f: &mut Frame, app: &App, area: Rect) {
    let (playlists, selected) = app.playlists_view();
    let sort_name = |s: cassis_core::model::SortPreset| match s {
        cassis_core::model::SortPreset::ArtistAlbumTrack => "artist/album",
        cassis_core::model::SortPreset::YearDesc => "year",
        cassis_core::model::SortPreset::MostPlayed => "most played",
        cassis_core::model::SortPreset::HighestRated => "highest rated",
        cassis_core::model::SortPreset::Random => "random",
        cassis_core::model::SortPreset::RandomAlbum => "random album",
        cassis_core::model::SortPreset::Path => "path",
    };
    let items: Vec<ListItem> = playlists
        .iter()
        .enumerate()
        .map(|(i, pl)| {
            ListItem::new(format!(
                "g{}  {}  [{}]  sort: {}",
                i + 1,
                pl.name,
                pl.query,
                sort_name(pl.sort_preset)
            ))
        })
        .collect();
    let title = if playlists.is_empty() {
        "playlists - empty (P to save current search)".to_string()
    } else {
        format!(
            "playlists - {} (Enter: activate  x: delete)",
            playlists.len()
        )
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(Style::default().fg(theme::TITLE)),
        )
        .highlight_style(Style::default().bg(theme::ACCENT).fg(Color::Black))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Length(1)])
        .split(area);

    let np_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(chunks[0].inner(Margin::new(1, 1)));

    let pos_ms = app.position_ms();
    let duration_secs = app
        .now_playing
        .as_ref()
        .map(|t| t.duration_secs)
        .unwrap_or(0);
    let pos_secs = (pos_ms / 1000) as u32;
    let pct = if duration_secs > 0 {
        (pos_ms as f64 / (duration_secs as f64 * 1000.0) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let np_line = if let Some(t) = &app.now_playing {
        let status_color = if app.is_playing {
            theme::NOW_PLAYING
        } else {
            theme::PAUSED
        };
        let stop = if app.stop_after { " [stop after]" } else { "" };
        vec![
            Span::styled(
                format!("{}: ", if app.is_playing { "playing" } else { "paused" }),
                Style::default().fg(status_color).bold(),
            ),
            Span::styled(t.title.clone(), Style::default().bold()),
            Span::raw(" - "),
            Span::styled(t.artist.clone(), Style::default().fg(theme::ARTIST)),
            Span::raw(" ["),
            Span::styled(t.album.clone(), Style::default().fg(theme::ALBUM)),
            Span::raw("] "),
            Span::styled(render_rating(t.rating), Style::default().fg(theme::RATING)),
            Span::raw(" "),
            Span::styled(
                format!(
                    "{}/{}",
                    fmt_duration(pos_secs),
                    fmt_duration(t.duration_secs)
                ),
                Style::default().fg(theme::DURATION),
            ),
            Span::styled(stop, Style::default().fg(theme::STOP_AFTER).bold()),
        ]
    } else {
        vec![Span::raw("stopped. press Enter on a track to play.")]
    };
    let radio = app
        .radio_sort
        .map(|s| match s {
            cassis_core::model::SortPreset::RandomAlbum => "random album",
            cassis_core::model::SortPreset::Random => "random",
            _ => "ordered",
        })
        .unwrap_or("-");
    let vol = format!("vol: {:0.0}%", app.volume * 100.0);
    let mut line = np_line;
    line.push(Span::raw("  | "));
    line.push(Span::styled(
        format!("radio: {radio}"),
        Style::default().fg(theme::ACCENT),
    ));
    line.push(Span::raw("  | "));
    line.push(Span::styled(vol, Style::default().fg(theme::ACCENT)));
    let paragraph = Paragraph::new(Line::from(line)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("now playing - {} tracks", app.track_count))
            .title_style(Style::default().fg(theme::TITLE)),
    );
    f.render_widget(paragraph, chunks[0]);
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(theme::GAUGE).bg(theme::ACCENT_DIM))
            .percent(pct as u16),
        np_area[1],
    );

    let status = if app.status.is_empty() {
        tab_hint(app.tab)
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(theme::HINT)),
        chunks[1],
    );
}

/// Context-sensitive keybinding hint for the bottom status line.
fn tab_hint(tab: Tab) -> String {
    let universal =
        "  Tab:tabs  /:search  p:play  >:next <:prev  +/-:vol  h/l:seek  ?:help  Ctrl+C:quit";
    let actions = match tab {
        Tab::Tracks | Tab::Files => {
            "Enter:play  q:queue  n:next  x:remove  0-5:rate  d:details  c:view  r:sort  m/R:radio  s:stop-after"
        }
        Tab::Albums => {
            "Enter:play album  q:queue album  n:next  d:details  c:view  r:sort  m/R:radio"
        }
        Tab::Artists => {
            "Enter:play artist  q:queue artist  n:next  d:details  c:view  r:sort  m/R:radio"
        }
        Tab::Queue => "Enter:jump to  x:remove  s:stop-after  d:details  c:view",
        Tab::Playlists => "Enter:activate  x:delete  P:save current search as playlist",
    };
    format!("{actions}{universal}")
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("help")
        .title_style(Style::default().fg(theme::TITLE))
        .border_style(Style::default().fg(theme::POPUP_BORDER));
    let mut text = String::from(
        "\
/search            filter the current tab (Esc to leave)
Tab / Shift+Tab    switch tabs
j k / arrows       move selection    PgUp/PgDn jump
Enter             play / activate playlist (playlists tab)
q                 enqueue (append)        n play next
x                 remove from queue (queue tab) / delete playlist (playlists tab)
s                 stop after current
1-5 / 0           rate (0 clears)
c                 cycle columns (view preset)
r                 cycle sort            m set radio  R random-album radio
p > <             play/pause, next, previous
+/-               volume up/down
</> (h/l)         seek backward/forward 5s    H/L seek 30s
P                 save current search as a smart playlist
g1-9              activate saved playlist by index (or use playlists tab)
d                 show track details (path, metadata, etc.)
Ctrl+C / Esc      quit

search syntax:
  free text           matches title, artist, album
  ar:pink            artist contains 'pink'
  al:=kind of blue   album equals exactly
  t:-love            title does not contain 'love'
  #jazz              genre contains 'jazz'
  year:>=1990        year >= 1990
  *>=4               rating >= 4 stars
  ~>5m               duration > 5 minutes
  p:0                play count = 0 (never played)
  -term              exclude (NOT)
  a | b              either (OR)
  (a b)              grouping (implicit AND)
",
    );
    if !app.smart_playlists.is_empty() {
        text.push_str("\nsaved playlists (g+N or playlists tab):\n");
        for (i, pl) in app.smart_playlists.iter().take(9).enumerate() {
            text.push_str(&format!("  g{}  {}  [{}]\n", i + 1, pl.name, pl.query));
        }
    }
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

fn draw_details(f: &mut Frame, area: Rect, track: &Track) {
    let label = Style::default().fg(theme::ACCENT);
    let val = Style::default();
    let lines = vec![
        Line::from(vec![
            Span::styled("title:  ", label),
            Span::styled(&track.title, val),
        ]),
        Line::from(vec![
            Span::styled("artist: ", label),
            Span::styled(&track.artist, val),
        ]),
        Line::from(vec![
            Span::styled("album:  ", label),
            Span::styled(&track.album, val),
        ]),
        Line::from(vec![
            Span::styled("album_artist: ", label),
            Span::styled(&track.album_artist, val),
        ]),
        Line::from(vec![
            Span::styled("genre:  ", label),
            Span::styled(&track.genre, val),
        ]),
        Line::from(vec![
            Span::styled("comment: ", label),
            Span::styled(&track.comment, val),
        ]),
        Line::from(vec![
            Span::styled("track_number: ", label),
            Span::styled(track.track_number.to_string(), val),
            Span::styled("    year: ", label),
            Span::styled(
                if track.year > 0 {
                    track.year.to_string()
                } else {
                    "?".into()
                },
                val,
            ),
            Span::styled("    duration: ", label),
            Span::styled(fmt_duration(track.duration_secs), val),
        ]),
        Line::from(vec![
            Span::styled("rating: ", label),
            Span::styled(
                render_rating(track.rating),
                Style::default().fg(theme::RATING),
            ),
            Span::styled("    play_count: ", label),
            Span::styled(track.play_count.to_string(), val),
            Span::styled("    last_played: ", label),
            Span::styled(
                track
                    .last_played
                    .map(|ts| format!("unix:{ts}"))
                    .unwrap_or("never".into()),
                val,
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("path: ", label),
            Span::styled(&track.path, Style::default().fg(theme::PATH)),
        ]),
        Line::from(vec![
            Span::styled("id:   ", label),
            Span::styled(&track.id, val),
        ]),
        Line::from(vec![
            Span::styled("mtime: ", label),
            Span::styled(track.file_mtime.to_string(), val),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("track details (any key to close)")
        .title_style(Style::default().fg(theme::TITLE))
        .border_style(Style::default().fg(theme::POPUP_BORDER));
    f.render_widget(Paragraph::new(lines).block(block), area);
}
