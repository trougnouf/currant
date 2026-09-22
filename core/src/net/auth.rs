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
    let expected_result = mac.finalize().into_bytes();

    // Decode hex
    let mut sig_bytes = Vec::with_capacity(sig_hex.len() / 2);
    let mut chars = sig_hex.chars();
    while let (Some(c1), Some(c2)) = (chars.next(), chars.next()) {
        if let (Ok(b1), Ok(b2)) = (
            u8::from_str_radix(&c1.to_string(), 16),
            u8::from_str_radix(&c2.to_string(), 16),
        ) {
            sig_bytes.push((b1 << 4) | b2);
        } else {
            return false;
        }
    }

    // Constant-time comparison to prevent timing attacks
    if sig_bytes.len() != expected_result.len() {
        return false;
    }
    let mut result = 0;
    for (x, y) in sig_bytes.iter().zip(expected_result.iter()) {
        result |= x ^ y;
    }
    result == 0
}
