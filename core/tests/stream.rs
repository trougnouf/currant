// File: ./core/tests/stream.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end check for the windowed HTTP range reader: a minimal TLS server
//! that honours `Range` headers, and a client that must stream a multi-MiB
//! body correctly with far fewer requests than reads.

use currant_core::net::stream::HttpSeekableReader;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const TOKEN: &str = "smoke-test-token-0123456789";
const WINDOW: usize = 1024 * 1024;

/// A deterministic 3 MiB body: three full windows, so sequential reads must
/// cross window boundaries.
fn test_body() -> Vec<u8> {
    let mut v = Vec::with_capacity(3 * WINDOW);
    let mut x: u64 = 0x1234_5678_9abc_def0;
    while v.len() < 3 * WINDOW {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.extend_from_slice(&(x as u32).to_le_bytes());
    }
    v
}

/// Serve one HTTP request per connection: parse the `Range` header and answer
/// with 206 (window), 416 (past EOF) or 200 (no range), like `serve_stream`.
fn serve(body: &Arc<Vec<u8>>, counter: &Arc<AtomicU64>, tcp: std::net::TcpStream) {
    let cfg = currant_core::net::tls::server_config(TOKEN);
    let Ok(conn) = rustls::ServerConnection::new(cfg) else {
        return;
    };
    let mut s = rustls::StreamOwned::new(conn, tcp);

    // Read the request head (request line + headers).
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while s.read(&mut b).unwrap_or(0) == 1 {
        head.push(b[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let req = String::from_utf8_lossy(&head).to_string();
    let range = req
        .lines()
        .find_map(|l| l.trim().strip_prefix("Range:"))
        .map(str::trim)
        .unwrap_or_default();

    let len = body.len() as u64;
    let (status, cr, out) = match range.strip_prefix("bytes=") {
        None => (200, format!("bytes */{len}"), body.as_slice().to_vec()),
        Some(spec) => {
            let (s, e) = spec.split_once('-').unwrap_or(("0", ""));
            let start: u64 = s.parse().unwrap_or(0);
            let end = if e.is_empty() {
                len
            } else {
                e.parse::<u64>().unwrap_or(0).saturating_add(1).min(len)
            };
            if start >= len || end <= start {
                (416, format!("bytes */{len}"), Vec::new())
            } else {
                (
                    206,
                    format!("bytes {start}-{}/{}", end - 1, len),
                    body[start as usize..end as usize].to_vec(),
                )
            }
        }
    };
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Range: {cr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        out.len()
    );
    let _ = s.write_all(resp.as_bytes());
    let _ = s.write_all(&out);
    let _ = s.flush();
    counter.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn windowed_range_reads() {
    let body = Arc::new(test_body());
    let counter = Arc::new(AtomicU64::new(0));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // Detached on purpose: it parks on accept once the client is done, and
    // the test process exits with it.
    std::thread::spawn({
        let body = Arc::clone(&body);
        let counter = Arc::clone(&counter);
        move || {
            for tcp in listener.incoming().flatten() {
                serve(&body, &counter, tcp);
            }
        }
    });

    let mut reader =
        HttpSeekableReader::new(&["127.0.0.1".to_string()], port, "t1", TOKEN.to_string())
            .expect("probe");

    // Sequential read of the whole body in 8 KiB chunks (decoder-sized).
    let mut got = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(got, *body, "sequential reads must reproduce the body");

    // Seek backwards into a previously-fetched window: the reader only keeps
    // the current window, so this re-fetches — and must still be correct.
    reader.seek(SeekFrom::Start(1_500_000)).unwrap();
    let n = reader.read(&mut chunk).unwrap();
    assert_eq!(&chunk[..n], &body[1_500_000..1_500_000 + n]);

    // Seek further back across a window boundary: another re-fetch.
    reader.seek(SeekFrom::Start(500_000)).unwrap();
    let n = reader.read(&mut chunk).unwrap();
    assert_eq!(&chunk[..n], &body[500_000..500_000 + n]);

    // A follow-up read stays inside the current window: no new request.
    let before = counter.load(Ordering::SeqCst);
    let n = reader.read(&mut chunk).unwrap();
    assert_eq!(&chunk[..n], &body[508_192..508_192 + n]);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        before,
        "in-window reads must not fetch"
    );

    // EOF: at the end of the file.
    reader.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(reader.read(&mut chunk).unwrap(), 0);

    // The whole 3 MiB body must have cost a handful of requests, not hundreds.
    let requests = counter.load(Ordering::SeqCst);
    let reads = body.len() / 8192 + 4;
    assert!(
        requests <= 10,
        "windowing should batch reads: {requests} requests for {reads} reads"
    );
}
