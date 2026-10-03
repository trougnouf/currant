// File: ./core/src/net/stream.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! A seekable reader over HTTP `Range` requests. Lets the audio backend feed
//! a remote file to decoders (symphonia, opus) that expect a local, seekable
//! stream.

use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

/// A seekable byte source (local file or remote HTTP stream) that can be
/// handed to a decoder. Rust allows only one non-auto trait per trait object,
/// so `Read` and `Seek` are combined here; the auto traits are supertraits,
/// which makes `Box<dyn Seekable>` itself `Send + Sync`.
pub trait Seekable: Read + Seek + Send + Sync {}
impl<T: Read + Seek + Send + Sync + ?Sized> Seekable for T {}

/// Size of the byte window fetched per `Range` request: large enough to
/// amortise the round trip over many decoder reads, small enough to keep
/// seek latency low.
const WINDOW_SIZE: u64 = 1024 * 1024; // 1 MiB

/// Reads a remote file over HTTP using `Range` requests. `seek` just moves an
/// internal cursor; reads are served from an in-memory window, and a fresh
/// `GET` is issued only when the cursor leaves the current window. Holding
/// the response inside the reader keeps the decoder free of connection
/// lifetimes while cutting round trips by a factor of the window size.
pub struct HttpSeekableReader {
    url: String,
    auth_header: String,
    agent: ureq::Agent,
    cursor: u64,
    /// Total length from the opening `Content-Range` probe. Zero when the
    /// peer didn't report one; range requests stay bounded by the window
    /// either way.
    total: u64,
    /// Bytes of the current window, starting at `window_start`.
    window: Vec<u8>,
    /// File offset at which `window` begins.
    window_start: u64,
}

impl HttpSeekableReader {
    /// Open the remote file. A zero-length `GET` request securely learns the total
    /// length from the `Content-Range` header, bypassing tiny_http HEAD limitations.
    pub fn new(ips: &[String], port: u16, track_id: &str, token: String) -> io::Result<Self> {
        let uri = format!("/stream/{track_id}");
        let tls_config = crate::net::tls::ureq_client_config();
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .tls_config(tls_config)
                .build(),
        );
        let auth_header = crate::net::auth::generate_header(&token, &uri);

        let mut last_err = io::Error::new(io::ErrorKind::NotConnected, "no IPs provided");

        for ip in ips {
            let url = format!("https://{ip}:{port}{uri}");
            match agent
                .get(&url)
                .header("Authorization", &auth_header)
                .header("Range", "bytes=0-0")
                .call()
            {
                Ok(res) => {
                    let total = res
                        .headers()
                        .get("Content-Range")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.split('/').next_back())
                        .and_then(|t| t.parse::<u64>().ok())
                        .unwrap_or(0);

                    return Ok(Self {
                        url,
                        auth_header,
                        agent,
                        cursor: 0,
                        total,
                        window: Vec::new(),
                        window_start: 0,
                    });
                }
                Err(e) => {
                    last_err = io::Error::other(e.to_string());
                }
            }
        }
        Err(last_err)
    }
}

impl HttpSeekableReader {
    /// Fetch the window covering `self.cursor` into `self.window`.
    fn fetch_window(&mut self) -> io::Result<()> {
        let start = self.cursor;
        let end = if self.total > 0 {
            (start + WINDOW_SIZE - 1).min(self.total - 1)
        } else {
            start + WINDOW_SIZE - 1
        };
        let mut response = self
            .agent
            .get(&self.url)
            .header("Authorization", &self.auth_header)
            .header("Range", &format!("bytes={start}-{end}"))
            .call()
            .map_err(|e| io::Error::other(e.to_string()))?;
        // A 416 means the cursor is past the end of the file: report EOF.
        if response.status() == 416 {
            // Mark a conservative EOF so later reads don't re-probe.
            if self.total == 0 {
                self.total = start.max(1);
            }
            self.window.clear();
            self.window_start = start;
            return Ok(());
        }
        // A 200 means the server ignored the range: only valid from byte 0.
        if response.status() == 200 && start > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "server ignored the Range request",
            ));
        }
        let mut reader = response.body_mut().as_reader();
        self.window.clear();
        reader.read_to_end(&mut self.window)?;
        self.window_start = start;
        Ok(())
    }
}

impl Read for HttpSeekableReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.total > 0 && self.cursor >= self.total {
            return Ok(0); // EOF
        }
        let in_window = self.window_start <= self.cursor
            && self.cursor < self.window_start + self.window.len() as u64;
        if !in_window {
            self.fetch_window()?;
        }
        if self.window.is_empty() {
            return Ok(0); // EOF
        }
        let offset = (self.cursor - self.window_start) as usize;
        let n = buf.len().min(self.window.len() - offset);
        buf[..n].copy_from_slice(&self.window[offset..offset + n]);
        self.cursor += n as u64;
        Ok(n)
    }
}

impl Seek for HttpSeekableReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target: i128 = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.cursor as i128 + n as i128,
            SeekFrom::End(n) => {
                if self.total == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "seek from end needs a known file length",
                    ));
                }
                self.total as i128 + n as i128
            }
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of file",
            ));
        }
        self.cursor = if self.total > 0 {
            (target as u64).min(self.total)
        } else {
            target as u64
        };
        Ok(self.cursor)
    }
}
