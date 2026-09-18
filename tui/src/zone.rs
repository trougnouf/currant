// File: ./tui/src/zone.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! WebSocket client thread for remote zone control.

use currant_core::control::{ControlRequest, ControlResponse};
use currant_core::model::PlayerIntent;
use std::net::TcpStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tungstenite::{Message, client};

pub enum ZoneCommand {
    Switch(Option<(String, u16)>), // IP and WS port
    Intent(PlayerIntent),
}

pub fn spawn(
    rx: Receiver<ZoneCommand>,
    remote_state: Arc<Mutex<Option<ControlResponse>>>,
    store: Arc<currant_core::store::LibraryStore>,
) {
    std::thread::spawn(move || {
        let mut current_peer: Option<(String, u16)>;
        let mut socket: Option<tungstenite::WebSocket<TcpStream>> = None;

        loop {
            // Sleep/timeout: Fast polling when connected, sleep until command when disconnected.
            let timeout = if socket.is_some() {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(86400)
            };

            match rx.recv_timeout(timeout) {
                Ok(ZoneCommand::Switch(peer)) => {
                    current_peer = peer;
                    socket = None; // Drop old connection
                    *remote_state.lock().unwrap() = None;

                    if let Some((ref ip, port)) = current_peer {
                        let url = format!("ws://{ip}:{port}/ws");
                        if let Ok(stream) = TcpStream::connect((ip.as_str(), port)) {
                            // Timeouts ensure reads/writes don't hang if the peer vanishes
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
                            let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
                            let req = tungstenite::http::Request::builder()
                                .uri(&url)
                                .header(
                                    "Authorization",
                                    format!("Bearer {}", store.load_pairing_token()),
                                )
                                .body(())
                                .unwrap();
                            if let Ok((mut ws, _)) = client::client(req, stream) {
                                // Request initial state
                                if let Ok(json) = serde_json::to_string(&ControlRequest::Status) {
                                    let _ = ws.write(Message::text(json));
                                }
                                socket = Some(ws);
                            }
                        }
                    }
                }
                Ok(ZoneCommand::Intent(intent)) => {
                    if let Some(ws) = socket.as_mut() {
                        let req = ControlRequest::Intent { intent };
                        if let Ok(json) = serde_json::to_string(&req) {
                            let _ = ws.write(Message::text(json));
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Poll status periodically
                    if let Some(ws) = socket.as_mut()
                        && let Ok(json) = serde_json::to_string(&ControlRequest::Status)
                        && ws.write(Message::text(json)).is_err()
                    {
                        socket = None;
                        *remote_state.lock().unwrap() = None;
                        continue;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }

            // Flush incoming messages
            if let Some(ws) = socket.as_mut() {
                loop {
                    match ws.read() {
                        Ok(Message::Text(text)) => {
                            if let Ok(resp) = serde_json::from_str::<ControlResponse>(&text) {
                                *remote_state.lock().unwrap() = Some(resp);
                            }
                        }
                        Ok(Message::Close(_)) => {
                            socket = None;
                            *remote_state.lock().unwrap() = None;
                            break;
                        }
                        Err(tungstenite::Error::Io(e))
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            break; // No more data right now
                        }
                        Err(_) => {
                            socket = None;
                            *remote_state.lock().unwrap() = None;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    });
}
