// ./tui/src/control.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Unix domain socket server for remote control. Listens for `ControlRequest`
//! JSON lines from `currant-ctl` (or any client), dispatches them through the
//! controller, and writes back a `ControlResponse` snapshot.

use currant_core::control::{ControlRequest, ControlResponse};
use currant_core::controller::PlayerController;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Resolve the socket path. Prefers `$XDG_RUNTIME_DIR`, falls back to `/tmp`.
pub fn socket_path() -> PathBuf {
    let dir = dirs::runtime_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("currant.sock")
}

/// Bind the socket, removing any stale leftover. Returns the listener or
/// an error (logged, non-fatal — the TUI runs fine without remote control).
fn bind(path: &std::path::Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Restrict to the current user.
    let _ = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    Ok(listener)
}

/// Spawn the control server thread. Returns immediately.
pub fn spawn(controller: Arc<Mutex<PlayerController>>) {
    let path = socket_path();

    std::thread::spawn(move || {
        let listener = match bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("currant: control socket failed at {}: {e}", path.display());
                return;
            }
        };

        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let controller = controller.clone();
            std::thread::spawn(move || handle(stream, &controller));
        }

        let _ = std::fs::remove_file(&path);
    });
}

fn handle(stream: std::os::unix::net::UnixStream, controller: &Mutex<PlayerController>) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    if reader.read_line(&mut line).is_err() {
        return;
    }

    let request: ControlRequest = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let resp = ControlResponse {
                is_playing: false,
                volume: 0.0,
                current_track: None,
                position_ms: 0,
                stop_after: None,
                queue: Default::default(),
                error: Some(format!("bad request: {e}")),
            };
            write_response(&stream, &resp);
            return;
        }
    };

    let resp = {
        let mut c = controller.lock().unwrap();
        if let ControlRequest::Intent { intent } = request {
            c.dispatch(intent);
        }
        ControlResponse {
            is_playing: c.is_playing,
            volume: c.volume,
            current_track: c.current_track_ref(),
            position_ms: c.playback_state.position_ms(),
            stop_after: c.stop_after.clone(),
            queue: c.queue_snapshot(),
            error: None,
        }
    };

    write_response(&stream, &resp);
}

fn write_response(stream: &std::os::unix::net::UnixStream, resp: &ControlResponse) {
    let Ok(json) = serde_json::to_string(resp) else {
        return;
    };
    let mut writer = stream;
    let _ = writeln!(writer, "{json}");
}
