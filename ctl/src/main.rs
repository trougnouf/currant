// ./ctl/src/main.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! `cassis-ctl` — remote control for a running Cassis instance.
//!
//! Connects to the control socket exposed by the TUI, sends a `PlayerIntent`
//! (or a status query), and prints the resulting playback state.

use cassis_core::control::{ControlRequest, ControlResponse};
use cassis_core::model::PlayerIntent;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

fn socket_path() -> PathBuf {
    let dir = dirs::runtime_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("cassis.sock")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        return ExitCode::from(2);
    }

    let request = match build_request(&args[1], &args[2..]) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("{msg}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    match send(request) {
        Ok(resp) => {
            if let Some(err) = &resp.error {
                eprintln!("error: {err}");
                return ExitCode::from(1);
            }
            print_status(&resp);
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("cassis-ctl: {e}");
            eprintln!("is the TUI running?");
            ExitCode::from(1)
        }
    }
}

fn build_request(cmd: &str, args: &[String]) -> Result<ControlRequest, String> {
    let intent = match (cmd, args.len()) {
        ("play-pause" | "toggle", 0) => PlayerIntent::TogglePlayPause,
        ("next", 0) => PlayerIntent::NextTrack,
        ("skip-album", 0) => PlayerIntent::SkipAlbum,
        ("prev" | "previous", 0) => PlayerIntent::PreviousTrack,
        ("stop-after", 0) => PlayerIntent::StopAfter { id: String::new() },
        ("stop-after", 1) => PlayerIntent::StopAfter {
            id: args[0].clone(),
        },
        ("clear", 0) => PlayerIntent::ClearQueue,
        ("status", 0) => return Ok(ControlRequest::Status),
        ("volume", 1) => {
            let pct: f32 = args[0]
                .parse()
                .map_err(|_| "volume must be a number 0–100")?;
            PlayerIntent::SetVolume {
                volume: (pct / 100.0).clamp(0.0, 1.0),
            }
        }
        ("play", 1) => PlayerIntent::PlayTrack {
            id: args[0].clone(),
        },
        ("enqueue", 1) => PlayerIntent::Enqueue {
            id: args[0].clone(),
            next: false,
        },
        ("play-next", 1) => PlayerIntent::Enqueue {
            id: args[0].clone(),
            next: true,
        },
        ("rate", 2) => {
            let rating: u8 = args[1].parse().map_err(|_| "rating must be 0–5")?;
            PlayerIntent::RateTrack {
                id: args[0].clone(),
                rating,
            }
        }
        _ => return Err(format!("unknown command or wrong arguments: {cmd}")),
    };
    Ok(ControlRequest::Intent { intent })
}

fn send(request: ControlRequest) -> std::io::Result<ControlResponse> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)?;
    let json = serde_json::to_string(&request).map_err(std::io::Error::other)?;
    {
        let mut writer = &stream;
        writeln!(writer, "{json}")?;
    }
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let resp: ControlResponse = serde_json::from_str(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(resp)
}

fn print_status(resp: &ControlResponse) {
    let state = if resp.is_playing { "playing" } else { "paused" };
    let vol = (resp.volume * 100.0).round() as i32;
    match &resp.current_track {
        Some(t) => {
            println!("{state}  vol {vol}%  {}", t.title);
            println!(
                "  {} — {} (track {}, {})",
                t.album,
                t.artist,
                t.track_number,
                format_duration(t.duration_secs)
            );
        }
        None => println!("{state}  vol {vol}%  (nothing loaded)"),
    }

    let queued: usize = resp.queue.explicit_queue.len() + resp.queue.dynamic_queue.len();
    if queued > 0 {
        println!("  {queued} tracks in queue");
    }
}

fn format_duration(secs: u32) -> String {
    let m = secs / 60;
    let s = secs % 60;
    format!("{m}:{s:02}")
}

fn print_usage() {
    eprintln!("usage: cassis-ctl <command> [args]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  play-pause          toggle playback");
    eprintln!("  next                skip to next track");
    eprintln!("  skip-album          skip the rest of the current album");
    eprintln!("  prev                go to previous track");
    eprintln!("  stop-after [id]     stop after the current or given track");
    eprintln!("  clear               clear the queue");
    eprintln!("  volume <0-100>      set volume percentage");
    eprintln!("  play <id>           play a track immediately");
    eprintln!("  enqueue <id>        add a track to the end of the queue");
    eprintln!("  play-next <id>      add a track to the front of the queue");
    eprintln!("  rate <id> <0-5>     rate a track");
    eprintln!("  status              show current playback state");
}
