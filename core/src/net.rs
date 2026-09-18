// File: ./core/src/net.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Mesh networking: HTTP server for streaming/sync, WebSocket for control, and mDNS for discovery.

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Response, Server};
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
pub fn start_network(local_instance_id: String) -> Arc<Mutex<NetworkState>> {
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

    thread::spawn(move || {
        for request in http_server.incoming_requests() {
            // TODO(net): Implement /sync and /stream handlers
            let _ = request.respond(Response::from_string("Currant Node"));
        }
    });

    // 2. Start WebSocket Server (Remote Control & State Sync)
    let ws_listener = TcpListener::bind("0.0.0.0:0").expect("Failed to bind WS server");
    let ws_port = ws_listener.local_addr().unwrap().port();

    thread::spawn(move || {
        for stream in ws_listener.incoming().flatten() {
            thread::spawn(move || {
                if let Ok(mut websocket) = accept(stream) {
                    // TODO(net): Handle incoming PlayerIntents
                    loop {
                        // Keep connection alive, drop if read fails
                        if websocket.read().is_err() {
                            break;
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
                        let ip = info
                            .get_addresses()
                            .iter()
                            .next()
                            .map(|ip| ip.to_ip_addr().to_string())
                            .unwrap_or_default();

                        let mut s = state_clone.lock().unwrap();
                        s.peers.insert(
                            id.to_string(),
                            Peer {
                                instance_id: id.to_string(),
                                ip,
                                http_port: http_p,
                                ws_port: ws_p,
                            },
                        );
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
