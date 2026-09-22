// File: ./core/src/net/auth.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Static HMAC authentication for the read-only media endpoints. The token
//! never crosses the wire; only its HMAC over the request URI does.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::fmt::Write;

type HmacSha256 = Hmac<Sha256>;

/// Generates a static HMAC-SHA256 signature to authenticate read-only HTTP GET requests.
/// The signature covers the URI to prevent cross-endpoint replay attacks.
pub fn generate_header(token: &str, uri: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(token.as_bytes()).expect("HMAC can take key of any size");
    mac.update(uri.as_bytes());
    let result = mac.finalize().into_bytes();

    let mut hex_sig = String::with_capacity(result.len() * 2);
    for b in result {
        write!(&mut hex_sig, "{:02x}", b).unwrap();
    }

    format!("Currant {hex_sig}")
}

/// Verifies the HMAC-SHA256 signature in constant time.
pub fn verify_header(token: &str, uri: &str, header: &str) -> bool {
    let Some(sig_hex) = header.strip_prefix("Currant ") else {
        return false;
    };

    let mut mac =
        HmacSha256::new_from_slice(token.as_bytes()).expect("HMAC can take key of any size");
    mac.update(uri.as_bytes());
    // Decode hex without per-character allocations.
    if sig_hex.len() % 2 != 0 {
        return false;
    }
    let mut sig_bytes = vec![0u8; sig_hex.len() / 2];
    for (i, chunk) in sig_hex.as_bytes().chunks(2).enumerate() {
        let high = (chunk[0] as char).to_digit(16);
        let low = (chunk[1] as char).to_digit(16);
        if let (Some(h), Some(l)) = (high, low) {
            sig_bytes[i] = (h as u8) << 4 | (l as u8);
        } else {
            return false;
        }
    }

    // Constant-time comparison using the hmac crate.
    mac.verify_slice(&sig_bytes).is_ok()
}
