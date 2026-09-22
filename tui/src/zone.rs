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
    Switch(Option<(Vec<String>, u16)>), // IPs and WS port
    Intent(PlayerIntent),
}

pub fn spawn(
    rx: Receiver<ZoneCommand>,
    remote_state: Arc<Mutex<Option<ControlResponse>>>,
    store: Arc<currant_core::store::LibraryStore>,
) {
    std::thread::spawn(move || {
        let mut socket: Option<
            tungstenite::WebSocket<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>,
        > = None;

        loop {
            // Sleep/timeout: Fast polling when connected, sleep until command when disconnected.
            let timeout = if socket.is_some() {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(86400)
            };

            match rx.recv_timeout(timeout) {
                Ok(ZoneCommand::Switch(peer)) => {
                    socket = None; // Drop old connection
                    *remote_state.lock().unwrap() = None;

                    if let Some((ref ips, port)) = peer {
                        let mut tcp_stream = None;
                        let mut chosen_ip = String::new();
                        for ip in ips {
                            if let Ok(stream) = TcpStream::connect((ip.as_str(), port)) {
                                tcp_stream = Some(stream);
                                chosen_ip = ip.clone();
                                break;
                            }
                        }

                        if let Some(tcp_stream) = tcp_stream {
                            let url = format!("wss://{chosen_ip}:{port}/ws");
                            // Generous timeout for the TLS + WebSocket handshakes
                            let _ = tcp_stream.set_read_timeout(Some(Duration::from_millis(2000)));
                            let _ = tcp_stream.set_write_timeout(Some(Duration::from_millis(2000)));

                            let token = store.load_pairing_token();
                            let tls_config = currant_core::net::tls::rustls_client_config(&token);
                            let server_name =
                                rustls::pki_types::ServerName::try_from("currant.local")
                                    .unwrap()
                                    .to_owned();
                            let tls_conn =
                                rustls::ClientConnection::new(tls_config, server_name).unwrap();
                            let stream = rustls::StreamOwned::new(tls_conn, tcp_stream);

                            let req = tungstenite::http::Request::builder()
                                .uri(&url)
                                .header("Authorization", format!("Bearer {}", token))
                                .body(())
                                .unwrap();
                            if let Ok((mut ws, _)) = client::client(req, stream) {
                                // Drop the timeout back down to 50ms for the fast non-blocking poll loop
                                let inner_tcp = ws.get_mut().get_mut();
                                let _ = inner_tcp.set_read_timeout(Some(Duration::from_millis(50)));
                                let _ =
                                    inner_tcp.set_write_timeout(Some(Duration::from_millis(50)));

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
