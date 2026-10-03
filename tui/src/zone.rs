// File: ./tui/src/zone.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! WebSocket client thread for remote zone control.

use currant_core::control::{ControlRequest, ControlResponse};
use currant_core::model::PlayerIntent;
use currant_core::store::LibraryStore;
use std::net::{IpAddr, TcpStream};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tungstenite::{Message, client};

/// The TLS-wrapped control WebSocket.
type Ws = tungstenite::WebSocket<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

pub enum ZoneCommand {
    /// Switch the active zone. `target` is the peer's (IPs, WS port), or
    /// `None` for local. `follow_up` is sent to the new target exactly once,
    /// as soon as the connection (re)establishes — the zone handoff rides
    /// here, so it survives connection failures and retries.
    Switch {
        target: Option<(Vec<String>, u16)>,
        follow_up: Option<PlayerIntent>,
    },
    /// Forward a keypress to the active zone. Dropped when not connected.
    Intent(PlayerIntent),
}

pub fn spawn(
    rx: Receiver<ZoneCommand>,
    remote_state: Arc<Mutex<Option<ControlResponse>>>,
    store: Arc<LibraryStore>,
) {
    thread::spawn(move || {
        let mut socket: Option<Ws> = None;
        let mut target: Option<(Vec<String>, u16)> = None;
        let mut follow_up: Option<PlayerIntent> = None;

        loop {
            // Fast polling when connected, short waits while (re)connecting,
            // sleep until a command when idle.
            let timeout = if socket.is_some() {
                Duration::from_millis(250)
            } else if target.is_some() {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(86400)
            };

            match rx.recv_timeout(timeout) {
                Ok(ZoneCommand::Switch {
                    target: new_target,
                    follow_up: new_follow_up,
                }) => {
                    socket = None;
                    *remote_state.lock().unwrap() = None;
                    target = new_target;
                    follow_up = new_follow_up;
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
                    // Poll status periodically.
                    if let Some(ws) = socket.as_mut()
                        && let Ok(json) = serde_json::to_string(&ControlRequest::Status)
                        && ws.write(Message::text(json)).is_err()
                    {
                        socket = None;
                        *remote_state.lock().unwrap() = None;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }

            // Flush incoming messages.
            if let Some(ws) = socket.as_mut() {
                loop {
                    match ws.read() {
                        Ok(Message::Text(text)) => {
                            if let Ok(resp) = serde_json::from_str::<ControlResponse>(&text) {
                                *remote_state.lock().unwrap() = Some(resp);
                            }
                        }
                        Ok(Message::Ping(data)) => {
                            let _ = ws.write(Message::Pong(data));
                        }
                        Ok(Message::Close(_)) => {
                            let _ = ws.close(None);
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

            // A target is set but we're not connected: (re)connect, with a
            // backoff between attempts.
            if socket.is_none()
                && let Some((ips, port)) = target.clone()
            {
                match connect(&ips, port, &store) {
                    Some(ws) => {
                        socket = Some(ws);
                        if let Some(ws) = socket.as_mut() {
                            if let Ok(json) = serde_json::to_string(&ControlRequest::Status) {
                                let _ = ws.write(Message::text(json));
                            }
                            // Deliver the handoff intent exactly once, right
                            // after the status request.
                            if let Some(intent) = follow_up.take() {
                                let req = ControlRequest::Intent { intent };
                                if let Ok(json) = serde_json::to_string(&req) {
                                    let _ = ws.write(Message::text(json));
                                }
                            }
                        }
                    }
                    None => thread::sleep(Duration::from_secs(5)),
                }
            }
        }
    });
}

/// Connect to a peer's control socket: rustls against the token-derived
/// root, Bearer token in the upgrade header. Tries each IP with bounded
/// timeouts.
fn connect(ips: &[String], port: u16, store: &LibraryStore) -> Option<Ws> {
    let token = store.load_pairing_token();
    let tls_config = currant_core::net::tls::rustls_client_config(&token);
    let Ok(server_name) = rustls::pki_types::ServerName::try_from("currant.local") else {
        return None;
    };
    let server_name = server_name.to_owned();

    for ip in ips {
        let Ok(ip_addr) = ip.parse::<IpAddr>() else {
            continue;
        };
        let addr = std::net::SocketAddr::new(ip_addr, port);
        let Ok(tcp) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
            continue;
        };
        // Generous timeout for the TLS + WebSocket handshakes.
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(2)));

        let Ok(tls_conn) = rustls::ClientConnection::new(tls_config.clone(), server_name.clone())
        else {
            continue;
        };
        let stream = rustls::StreamOwned::new(tls_conn, tcp);
        let url = format!("wss://{ip_addr}:{port}/ws");
        let Ok(req) = tungstenite::http::Request::builder()
            .uri(&url)
            .header("Authorization", format!("Bearer {token}"))
            .body(())
        else {
            continue;
        };
        let Ok((mut ws, _)) = client::client(req, stream) else {
            continue;
        };
        // Drop the timeout back down to 50ms for the fast non-blocking poll
        // loop.
        let inner_tcp = ws.get_mut().get_mut();
        let _ = inner_tcp.set_read_timeout(Some(Duration::from_millis(50)));
        let _ = inner_tcp.set_write_timeout(Some(Duration::from_millis(50)));
        return Some(ws);
    }
    None
}
