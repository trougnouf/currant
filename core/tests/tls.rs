// File: ./core/tests/tls.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end checks for the hybrid security design: the same pairing token
//! must derive identical crypto material on every peer, and both layers must
//! work against a real server (strict TLS for control, unverified TLS +
//! static HMAC for media).

use std::io::{Read, Write};

const TOKEN: &str = "smoke-test-token-0123456789";

// Determinism: same token -> same material (the whole design relies on this).
#[test]
fn deterministic_derivation() {
    let m1 = currant_core::net::tls::generate_crypto(TOKEN);
    let m2 = currant_core::net::tls::generate_crypto(TOKEN);
    assert_eq!(m1.leaf_pem, m2.leaf_pem, "leaf pem must be deterministic");
    assert_eq!(m1.leaf_der, m2.leaf_der, "leaf der must be deterministic");
    assert_eq!(m1.ca_der, m2.ca_der, "ca der must be deterministic");
}

// HMAC round trip, including the negative cases.
#[test]
fn hmac_auth() {
    let uri = "/sync?since=0";
    let header = currant_core::net::auth::generate_header(TOKEN, uri);
    assert!(header.starts_with("Currant "));
    assert!(currant_core::net::auth::verify_header(TOKEN, uri, &header));
    assert!(!currant_core::net::auth::verify_header(
        "wrong", uri, &header
    ));
    assert!(!currant_core::net::auth::verify_header(
        TOKEN, "/other", &header
    ));
    assert!(!currant_core::net::auth::verify_header(
        TOKEN, uri, "Bearer x"
    ));
}

// Media layer: tiny_http (rustls 0.20, Ed25519) <-> ureq (unverified TLS).
#[test]
fn media_layer() {
    let uri = "/sync?since=0";
    let media = currant_core::net::tls::generate_crypto(TOKEN);
    let ssl_config = tiny_http::SslConfig {
        certificate: media.leaf_pem,
        private_key: media.key_pem,
    };
    let server = tiny_http::Server::https("127.0.0.1:0", ssl_config).expect("bind https server");
    let port = server.server_addr().to_ip().unwrap().port();
    let server_thread = std::thread::spawn(move || {
        for request in server.incoming_requests().take(2) {
            let _ = request.respond(tiny_http::Response::from_string("ok"));
        }
    });
    let url = format!("https://127.0.0.1:{port}{uri}");
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .tls_config(currant_core::net::tls::ureq_client_config())
            .build(),
    );
    let body = agent
        .get(&url)
        .header(
            "Authorization",
            &currant_core::net::auth::generate_header(TOKEN, uri),
        )
        .call()
        .expect("get over unverified tls")
        .body_mut()
        .read_to_string()
        .unwrap();
    assert_eq!(body, "ok");
    // tiny_http omits Content-Length on HEAD responses; the reader already
    // falls back to unbounded ranges in that case, so only require success.
    let head = agent
        .head(&url)
        .header(
            "Authorization",
            &currant_core::net::auth::generate_header(TOKEN, uri),
        )
        .call()
        .expect("head over unverified tls");
    assert_eq!(head.status(), 200);
    server_thread.join().unwrap();
}

// Control layer: strict rustls server <-> client with PinVerifier.
#[test]
fn control_layer() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_thread = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let tls_config = currant_core::net::tls::server_config(TOKEN);
        let conn = rustls::ServerConnection::new(tls_config).expect("server conn");
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        stream.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"world");
    });
    let tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let tls_config = currant_core::net::tls::rustls_client_config(TOKEN);
    let name = rustls::pki_types::ServerName::try_from("currant.local")
        .unwrap()
        .to_owned();
    let conn = rustls::ClientConnection::new(tls_config, name).expect("client conn");
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let mut buf = [0u8; 5];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello");
    stream.write_all(b"world").unwrap();
    server_thread.join().unwrap();
}

// A client deriving from a different token must not complete the handshake.
#[test]
fn control_layer_rejects_wrong_token() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_thread = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let tls_config = currant_core::net::tls::server_config(TOKEN);
        let conn = rustls::ServerConnection::new(tls_config).expect("server conn");
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        let mut buf = [0u8; 1];
        let _ = stream.read(&mut buf); // expect alert/EOF, not data
    });
    let tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let bad_config = currant_core::net::tls::rustls_client_config("a-different-token");
    let name = rustls::pki_types::ServerName::try_from("currant.local")
        .unwrap()
        .to_owned();
    let conn = rustls::ClientConnection::new(bad_config, name).expect("client conn");
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "wrong token must not complete the handshake");
    server_thread.join().unwrap();
}
