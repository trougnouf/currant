// File: ./core/src/net.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Mesh networking: HTTP server for streaming/sync, WebSocket for control, and mDNS for discovery.

pub mod stream;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Header, Response, Server, StatusCode};
use tungstenite::accept;

pub const SERVICE_TYPE: &str = "_currant._tcp.local.";

#[derive(Debug, Clone)]
pub struct Peer {
    pub instance_id: String,
    pub ip: String,
    pub http_port: u16,
    pub ws_port: u16,
}

pub struct NetworkState {
    pub peers: HashMap<String, Peer>,
}

/// Spawns the HTTP server, WebSocket server, and mDNS daemon on background threads.
pub fn start_network(
    store: Arc<crate::store::LibraryStore>,
    controller: Arc<Mutex<crate::controller::PlayerController>>,
) -> Arc<Mutex<NetworkState>> {
    let local_instance_id = store.local_instance_id();
    let state = Arc::new(Mutex::new(NetworkState {
        peers: HashMap::new(),
    }));

    // 1. Start HTTP Server (Media streaming & Delta sync)
    // Binding to port 0 lets the OS pick an available port.
    let http_server = Server::http("0.0.0.0:0").expect("Failed to bind HTTP server");
    let http_port = http_server
        .server_addr()
        .to_ip()
        .expect("HTTP server bound to IP socket")
        .port();

    let http_store = store.clone();
    thread::spawn(move || {
        for request in http_server.incoming_requests() {
            if request.url().starts_with("/sync") {
                let since = request
                    .url()
                    .split("since=")
                    .nth(1)
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let payload = http_store.get_sync_payload(since);
                let json = serde_json::to_string(&payload).unwrap_or_default();
                let response = Response::from_string(json).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                let _ = request.respond(response);
                continue;
            }

            if let Some(id) = request.url().strip_prefix("/stream/") {
                let id = id.split('?').next().unwrap_or("");
                let response = serve_stream(&http_store, id, request.headers());
                let _ = request.respond(response);
                continue;
            }

            let _ = request.respond(Response::from_string("Currant Node"));
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
                if let Ok(mut websocket) = accept(stream) {
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
                            Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                            _ => {}
                        }
                    }
                }
            });
        }
    });

    // 3. Start mDNS Discovery (Zeroconf)
    let mdns = ServiceDaemon::new().expect("Failed to create mDNS daemon");
    let properties = vec![
        ("id".to_string(), local_instance_id.clone()),
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

    mdns.register(service_info)
        .expect("Failed to register mDNS service");

    let receiver = mdns.browse(SERVICE_TYPE).expect("Failed to browse mDNS");
    let state_clone = state.clone();
    let local_id = local_instance_id.clone();

    // Shared HTTP agent for delta sync, with a bounded timeout so an
    // unreachable peer can never hold the discovery thread hostage.
    let sync_agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(std::time::Duration::from_secs(15)))
            .build(),
    );

    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    if let Some(id) = info.get_property_val_str("id") {
                        if id == local_id {
                            continue; // Skip self
                        }
                        let http_p = info
                            .get_property_val_str("http")
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(0);
                        let ws_p = info
                            .get_property_val_str("ws")
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(0);
                        // Prefer a routable IPv4 address: mdns-sd may report a
                        // link-local IPv6 or a docker-bridge IP that would make
                        // the HTTP dial silently fail.
                        let ip = info
                            .get_addresses()
                            .iter()
                            .map(|a| a.to_ip_addr())
                            .find(|a| a.is_ipv4() && !a.is_loopback())
                            .or_else(|| info.get_addresses().iter().map(|a| a.to_ip_addr()).next())
                            .map(|a| a.to_string())
                            .unwrap_or_default();

                        {
                            let mut s = state_clone.lock().unwrap();
                            s.peers.insert(
                                id.to_string(),
                                Peer {
                                    instance_id: id.to_string(),
                                    ip: ip.clone(),
                                    http_port: http_p,
                                    ws_port: ws_p,
                                },
                            );
                        }

                        // Pull the peer's catalog delta in the background.
                        let agent = sync_agent.clone();
                        let sync_store = store.clone();
                        let peer_id = id.to_string();
                        thread::spawn(move || {
                            let since = sync_store.get_last_sync(&peer_id);
                            let url = format!("http://{ip}:{http_p}/sync?since={since}");
                            if let Ok(mut response) = agent.get(&url).call()
                                && let Ok(payload) =
                                    response.body_mut().read_json::<crate::model::SyncPayload>()
                            {
                                sync_store.apply_sync_payload(&peer_id, &payload);
                                sync_store.set_last_sync(&peer_id, crate::store::unix_now());
                            }
                        });
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let id = fullname.split('.').next().unwrap_or("");
                    let mut s = state_clone.lock().unwrap();
                    s.peers.remove(id);
                }
                _ => {}
            }
        }
    });

    state
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
