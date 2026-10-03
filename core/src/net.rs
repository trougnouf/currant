// File: ./core/src/net.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Mesh networking: HTTP server for streaming/sync, WebSocket for control, and mDNS for discovery.

pub mod auth;
pub mod stream;
pub mod tls;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Header, Response, Server, StatusCode};
use tungstenite::accept_hdr;
use tungstenite::handshake::server::{Request, Response as WsResponse};

pub const SERVICE_TYPE: &str = "_currant._tcp.local.";

#[derive(Debug, Clone)]
pub struct Peer {
    pub instance_id: String,
    pub name: String,
    pub ips: Vec<String>,
    pub http_port: u16,
    pub ws_port: u16,
}

pub struct NetworkState {
    pub peers: HashMap<String, Peer>,
    /// Peers with a catalog-sync retry thread in flight, so a re-resolution
    /// does not spawn a duplicate.
    syncing: HashSet<String>,
}

/// Spawns the HTTP server, WebSocket server, and mDNS daemon on background threads.
pub fn start_network(
    store: Arc<crate::store::LibraryStore>,
    controller: Arc<Mutex<crate::controller::PlayerController>>,
) -> Arc<Mutex<NetworkState>> {
    let local_instance_id = store.local_instance_id();
    let state = Arc::new(Mutex::new(NetworkState {
        peers: HashMap::new(),
        syncing: HashSet::new(),
    }));

    // 1. Start HTTP Server (Media streaming & Delta sync)
    // Binding to port 0 lets the OS pick an available port.
    let crypto = crate::net::tls::get_crypto(&store.load_pairing_token());
    let ssl_config = tiny_http::SslConfig {
        certificate: crypto.leaf_pem.clone(),
        private_key: crypto.key_pem.clone(),
    };
    let http_server = Server::https("0.0.0.0:0", ssl_config).expect("Failed to bind HTTPS server");
    let http_port = http_server
        .server_addr()
        .to_ip()
        .expect("HTTP server bound to IP socket")
        .port();

    let http_store = store.clone();
    thread::spawn(move || {
        for request in http_server.incoming_requests() {
            // One thread per request: a /stream response is held open for the
            // whole track and must not block /sync or other streams.
            let store = http_store.clone();
            thread::spawn(move || handle_http_request(&store, request));
        }
    });

    // 2. Start WebSocket Server (Remote Control & State Sync)
    let ws_listener = TcpListener::bind("0.0.0.0:0").expect("Failed to bind WS server");
    let ws_port = ws_listener.local_addr().unwrap().port();

    let ws_controller = controller.clone();
    thread::spawn(move || {
        for stream in ws_listener.incoming().flatten() {
            let c = ws_controller.clone();
            thread::spawn(move || {
                let c_for_cb = c.clone();
                let token = c_for_cb.lock().unwrap().store.load_pairing_token();
                let tls_config = crate::net::tls::server_config(&token);
                let tls_conn = rustls::ServerConnection::new(tls_config).unwrap();
                let stream = rustls::StreamOwned::new(tls_conn, stream);

                // The callback's error type is the HTTP response itself
                // (tungstenite writes it back to the client), so it cannot be
                // boxed.
                #[allow(clippy::result_large_err)]
                let callback = |req: &Request, response: WsResponse| {
                    let expected = format!("Bearer {}", token);
                    let auth = req
                        .headers()
                        .get("Authorization")
                        .and_then(|h| h.to_str().ok());
                    if auth == Some(&expected) {
                        Ok(response)
                    } else {
                        let mut err = tungstenite::http::Response::new(None);
                        *err.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
                        Err(err)
                    }
                };
                if let Ok(mut websocket) = accept_hdr(stream, callback) {
                    loop {
                        match websocket.read() {
                            Ok(tungstenite::Message::Text(text)) => {
                                if let Ok(req) =
                                    serde_json::from_str::<crate::control::ControlRequest>(&text)
                                {
                                    // Build the reply under the lock, write it
                                    // once released (mirrors control.rs).
                                    let json = {
                                        let mut ctrl = c.lock().unwrap();
                                        if let crate::control::ControlRequest::Intent { intent } =
                                            req
                                        {
                                            ctrl.dispatch(intent);
                                        }
                                        let resp = crate::control::ControlResponse {
                                            is_playing: ctrl.is_playing,
                                            volume: ctrl.volume,
                                            current_track: ctrl.current_track_ref(),
                                            position_ms: ctrl.playback_state.position_ms(),
                                            stop_after: ctrl.stop_after.clone(),
                                            queue: ctrl.queue_snapshot(),
                                            error: None,
                                        };
                                        serde_json::to_string(&resp).ok()
                                    };
                                    if let Some(json) = json {
                                        let _ = websocket.write(tungstenite::Message::text(json));
                                    }
                                }
                            }
                            Ok(tungstenite::Message::Ping(data)) => {
                                let _ = websocket.write(tungstenite::Message::Pong(data));
                            }
                            Ok(tungstenite::Message::Close(_)) => {
                                let _ = websocket.close(None);
                                break;
                            }
                            Err(_) => break,
                            _ => {}
                        }
                    }
                }
            });
        }
    });

    // 3. Start mDNS Discovery (Zeroconf)
    let mdns = ServiceDaemon::new().expect("Failed to create mDNS daemon");
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Currant".to_string());

    let properties = vec![
        ("id".to_string(), local_instance_id.clone()),
        ("name".to_string(), hostname),
        ("http".to_string(), http_port.to_string()),
        ("ws".to_string(), ws_port.to_string()),
    ];

    // No address here: `enable_addr_auto` makes mdns-sd advertise the host's
    // real interface IPs instead of a hardcoded one.
    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
        &local_instance_id,
        &format!("{}.local.", local_instance_id),
        (),
        http_port,
        properties.as_slice(),
    )
    .expect("Invalid mDNS service info")
    .enable_addr_auto();

    mdns.register(service_info.clone())
        .expect("Failed to register mDNS service");

    let receiver = mdns.browse(SERVICE_TYPE).expect("Failed to browse mDNS");
    let state_clone = state.clone();
    let local_id = local_instance_id.clone();

    // Shared HTTP agent for delta sync, with a bounded timeout so an
    // unreachable peer can never hold the discovery thread hostage.
    let tls_config = crate::net::tls::ureq_client_config();
    let sync_agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(std::time::Duration::from_secs(15)))
            .tls_config(tls_config)
            .build(),
    );

    let loop_store = store.clone();
    thread::spawn(move || {
        let mut receiver = receiver;
        loop {
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        if let Some(id) = info.get_property_val_str("id") {
                            if id == local_id {
                                continue; // Skip self
                            }
                            let name = info
                                .get_property_val_str("name")
                                .unwrap_or("Unknown")
                                .to_string();
                            let http_p = info
                                .get_property_val_str("http")
                                .and_then(|p| p.parse().ok())
                                .unwrap_or(0);
                            let ws_p = info
                                .get_property_val_str("ws")
                                .and_then(|p| p.parse().ok())
                                .unwrap_or(0);
                            // mDNS may report a docker-bridge IP, loopback, or IPv6. Keep all
                            // routable IPv4 addresses so clients can try them until one connects.
                            let ips: Vec<String> = info
                                .get_addresses()
                                .iter()
                                .filter(|a| !a.is_loopback() && a.is_ipv4())
                                .map(|a| a.to_string())
                                .collect();

                            if ips.is_empty() {
                                continue;
                            }

                            let sync_pending = {
                                let mut s = state_clone.lock().unwrap();
                                s.peers.insert(
                                    id.to_string(),
                                    Peer {
                                        instance_id: id.to_string(),
                                        name,
                                        ips: ips.clone(),
                                        http_port: http_p,
                                        ws_port: ws_p,
                                    },
                                );
                                s.syncing.insert(id.to_string())
                            };
                            loop_store.add_connected_peer(id);

                            // Pull the peer's catalog delta in the background, retrying
                            // with backoff until it succeeds or the peer leaves the
                            // mesh. `syncing` keeps this to one thread per peer.
                            if sync_pending {
                                let agent = sync_agent.clone();
                                let sync_store = loop_store.clone();
                                let sync_state = state_clone.clone();
                                let peer_id = id.to_string();
                                thread::spawn(move || {
                                    let token = sync_store.load_pairing_token();
                                    let mut backoff = std::time::Duration::from_secs(2);
                                    loop {
                                        // Re-read the peer each pass so address changes are
                                        // picked up; exit if it left the mesh.
                                        let Some(peer) = ({
                                            let s = sync_state.lock().unwrap();
                                            s.peers.get(&peer_id).cloned()
                                        }) else {
                                            let mut s = sync_state.lock().unwrap();
                                            s.syncing.remove(&peer_id);
                                            return;
                                        };
                                        let since = sync_store.get_last_sync(&peer_id);
                                        let uri = format!("/sync?since={since}");
                                        let auth_header =
                                            crate::net::auth::generate_header(&token, &uri);
                                        for ip in &peer.ips {
                                            let url =
                                                format!("https://{ip}:{}{uri}", peer.http_port);
                                            if let Ok(mut response) = agent
                                                .get(&url)
                                                .header("Authorization", &auth_header)
                                                .call()
                                                && let Ok(payload) = response
                                                    .body_mut()
                                                    .read_json::<crate::model::SyncPayload>(
                                                )
                                            {
                                                sync_store.apply_sync_payload(&peer_id, &payload);
                                                sync_store.set_last_sync(
                                                    &peer_id,
                                                    crate::store::unix_now(),
                                                );
                                                let mut s = sync_state.lock().unwrap();
                                                s.syncing.remove(&peer_id);
                                                return;
                                            }
                                        }
                                        thread::sleep(backoff);
                                        backoff =
                                            (backoff * 2).min(std::time::Duration::from_secs(60));
                                    }
                                });
                            }
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        let id = fullname.split('.').next().unwrap_or("");
                        let mut s = state_clone.lock().unwrap();
                        s.peers.remove(id);
                        loop_store.remove_connected_peer(id);
                    }
                    _ => {}
                }
            }
            // The browse channel closed: the mDNS daemon thread died. Rebuild
            // the daemon and re-register so discovery keeps working. The daemon
            // thread outlives its handle, so the handle is dropped here.
            eprintln!("currant: mDNS daemon exited, restarting");
            match ServiceDaemon::new().and_then(|daemon| {
                daemon
                    .register(service_info.clone())
                    .and_then(|_| daemon.browse(SERVICE_TYPE))
            }) {
                Ok(rx) => {
                    receiver = rx;
                }
                Err(e) => {
                    eprintln!("currant: mDNS restart failed: {e}");
                    thread::sleep(std::time::Duration::from_secs(5));
                }
            }
        }
    });

    state
}

/// Handle one HTTP request: verify auth, then route `/sync` to a catalog
/// delta, `/stream/:logical_id` to a byte-range stream, and anything else to
/// a plain banner.
fn handle_http_request(store: &crate::store::LibraryStore, request: tiny_http::Request) {
    let token = store.load_pairing_token();
    let uri = request.url();
    let req_auth = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str())
        .unwrap_or("");

    if !crate::net::auth::verify_header(&token, uri, req_auth) {
        let _ = request.respond(Response::empty(401));
        return;
    }

    if uri.starts_with("/sync") {
        let since = uri
            .split("since=")
            .nth(1)
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let payload = store.get_sync_payload(since);
        let json = serde_json::to_string(&payload).unwrap_or_default();
        let response = Response::from_string(json).with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        );
        let _ = request.respond(response);
        return;
    }

    if let Some(id) = uri.strip_prefix("/stream/") {
        let id = id.split('?').next().unwrap_or("");
        let response = serve_stream(store, id, request.headers());
        let _ = request.respond(response);
        return;
    }

    let _ = request.respond(Response::from_string("Currant Node"));
}

/// Serve one `GET /stream/:logical_id` request. 404 when this instance has no
/// physical copy, 416 for a range past EOF, 206 with the requested byte
/// window, 200 with the whole file otherwise.
fn serve_stream(
    store: &crate::store::LibraryStore,
    logical_id: &str,
    headers: &[Header],
) -> Response<Box<dyn Read + Send>> {
    let accept_ranges = Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap();
    let not_found = Response::new(
        StatusCode(404),
        vec![],
        Box::new(std::io::empty()) as Box<dyn Read + Send>,
        Some(0),
        None,
    );
    let Some(path) = store.get_local_source_path(logical_id) else {
        return not_found;
    };
    let Ok(file) = std::fs::File::open(&path) else {
        return not_found;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let range = headers
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str())
        .and_then(parse_range)
        .map(|(start, end)| {
            // Clamp the window to the file; an open-ended range runs to EOF.
            let end = end.map(|e| e.saturating_add(1)).unwrap_or(len).min(len);
            (start, end)
        });

    match range {
        // Unsatisfiable: start past EOF, or an empty/malformed window.
        Some((start, end)) if start >= len || end <= start => Response::new(
            StatusCode(416),
            vec![
                Header::from_bytes(&b"Content-Range"[..], format!("bytes */{len}").as_bytes())
                    .unwrap(),
            ],
            Box::new(std::io::empty()) as Box<dyn Read + Send>,
            Some(0),
            None,
        ),
        Some((start, end)) => {
            let mut file = BufReader::new(file);
            let _ = file.seek(SeekFrom::Start(start));
            Response::new(
                StatusCode(206),
                vec![
                    Header::from_bytes(
                        &b"Content-Range"[..],
                        format!("bytes {start}-{}/{len}", end - 1).as_bytes(),
                    )
                    .unwrap(),
                    accept_ranges,
                ],
                Box::new(file.take(end - start)) as Box<dyn Read + Send>,
                Some((end - start) as usize),
                None,
            )
        }
        None => Response::new(
            StatusCode(200),
            vec![accept_ranges],
            Box::new(file) as Box<dyn Read + Send>,
            Some(len as usize),
            None,
        ),
    }
}

/// Parse a `Range` header into `(start, Option<end>)`. Only the common
/// `bytes=N-` / `bytes=N-M` forms are understood; anything else is treated
/// as "no range".
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = end.trim().parse().ok();
    Some((start, end))
}
