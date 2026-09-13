// ./tui/src/ui.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering. Reads the `App` view model (built each frame) and lays out the
//! tabs, the active list and the now-playing bar.

use crate::app::{App, QueueKind, Tab, ViewPreset, fmt_duration, render_rating};
use cassis_core::model::Track;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Tabs};

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
        draw_help(f, f.area(), app);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(44), Constraint::Min(0)])
        .split(area);

    let titles = ["tracks", "albums", "artists", "queue", "files"]
        .iter()
        .map(|t| Line::from(*t))
        .collect::<Vec<_>>();
    let active = match app.tab {
        Tab::Tracks => 0,
        Tab::Albums => 1,
        Tab::Artists => 2,
        Tab::Queue => 3,
        Tab::Files => 4,
    };
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title("Cassis"))
        .select(active)
        .style(Style::default())
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_widget(tabs, cols[0]);

    let prompt = if app.in_search {
        "search>"
    } else {
        "search ('/ to type)"
    };
    let search = Paragraph::new(format!("{} {}", prompt, app.search))
        .block(Block::default().borders(Borders::ALL).title("filter"));
    f.render_widget(search, cols[1]);
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    match app.tab {
        Tab::Tracks => draw_tracks(f, app, area, app.tracks_view(), "tracks"),
        Tab::Files => draw_tracks(f, app, area, app.files_view(), "files"),
        Tab::Albums => draw_albums(f, app, area),
        Tab::Artists => draw_artists(f, app, area),
        Tab::Queue => draw_queue(f, app, area),
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
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(">> ");
    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn track_line(t: &Track, view: ViewPreset, playing: bool) -> Line<'_> {
    let marker = if playing { ">" } else { " " };
    let rating = render_rating(t.rating);
    let dur = fmt_duration(t.duration_secs);
    let spans: Vec<Span> = match view {
        ViewPreset::Minimal => vec![
            Span::raw(format!("{marker} ")),
            Span::raw(t.title.clone()),
            Span::raw(" "),
            Span::raw(dur),
        ],
        ViewPreset::Compact => vec![
            Span::raw(format!("{marker} {rating} ")),
            Span::raw(t.title.clone()),
            Span::raw(" - "),
            Span::raw(t.artist.clone()),
            Span::raw(" ["),
            Span::raw(t.album.clone()),
            Span::raw("] "),
            Span::raw(dur),
        ],
        ViewPreset::Full => vec![
            Span::raw(format!("{marker} {rating} ")),
            Span::raw(t.title.clone()),
            Span::raw(" - "),
            Span::raw(t.artist.clone()),
            Span::raw(" ["),
            Span::raw(t.album.clone()),
            Span::raw("] "),
            Span::raw(if t.year > 0 {
                t.year.to_string()
            } else {
                String::new()
            }),
            Span::raw(" "),
            Span::raw(t.genre.clone()),
            Span::raw(" "),
            Span::raw(dur),
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
                .title(format!("albums - {}", albums.len())),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
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
                .title(format!("artists - {}", artists.len())),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
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
            let mark = match r.kind {
                QueueKind::NowPlaying => ">",
                QueueKind::Explicit => "+",
                QueueKind::Dynamic => "~",
            };
            ListItem::new(format!("{mark} {} - {}", r.title, r.artist))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("queue - {} (x: remove)", rows.len())),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
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

    let np = if let Some(t) = &app.now_playing {
        let status = if app.is_playing { "playing" } else { "paused" };
        let stop = if app.stop_after { " [stop after]" } else { "" };
        format!(
            "{status}: {} - {} [{}] {} {}/{}{}",
            t.title,
            t.artist,
            t.album,
            render_rating(t.rating),
            fmt_duration(pos_secs),
            fmt_duration(t.duration_secs),
            stop
        )
    } else {
        "stopped. press Enter on a track to play.".to_string()
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
    let line = format!("{np}  | radio: {radio}  | {vol}");
    let paragraph = Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("now playing - {} tracks", app.track_count)),
    );
    f.render_widget(paragraph, chunks[0]);
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().add_modifier(Modifier::REVERSED))
            .percent(pct as u16),
        np_area[1],
    );

    let hint = "Tab:tabs  /:search  Enter:play  q:queue  n:next  x:remove  s:stop-after  1-5:rate  c:view  r:sort  m/R:radio  +/-:vol  </>:seek  p:> <  Ctrl+C:quit";
    let status = if app.status.is_empty() {
        hint.to_string()
    } else {
        app.status.clone()
    };
    f.render_widget(Paragraph::new(status), chunks[1]);
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title("help");
    let mut text = String::from(
        "\
/search            filter the current tab (Esc to leave)
Tab / Shift+Tab    switch tabs
j k / arrows       move selection    PgUp/PgDn jump
Enter             play (track/file/album/artist/queue row)
q                 enqueue (append)        n play next
x                 remove from queue (queue tab)
s                 stop after current
1-5 / 0           rate (0 clears)
c                 cycle columns (view preset)
r                 cycle sort            m set radio  R random-album radio
p > <             play/pause, next, previous
+/-               volume up/down
</> (h/l)         seek backward/forward 5s    H/L seek 30s
P                 save current search as a smart playlist
g1-9              activate saved playlist by index
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
        text.push_str("\nsaved playlists (g+N to activate):\n");
        for (i, pl) in app.smart_playlists.iter().take(9).enumerate() {
            text.push_str(&format!("  g{}  {}  [{}]\n", i + 1, pl.name, pl.query));
        }
    }
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}
