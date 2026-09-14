// ./core/src/control.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Wire protocol for remote control of a running currant instance.
//!
//! A frontend that owns a `PlayerController` (the TUI today, a standalone
//! daemon tomorrow) listens on a Unix domain socket. A CLI client connects,
//! sends one `ControlRequest` as a single line of JSON, and reads back one
//! `ControlResponse`. The server dispatches `PlayerIntent`s through the
//! normal `dispatch` path, so every remote action is identical to a key
//! press in the TUI.

use crate::model::{PlayerIntent, QueueSnapshot, Track};
use serde::{Deserialize, Serialize};

/// A request from a control client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlRequest {
    /// Dispatch a player intent (play, pause, next, etc.).
    Intent { intent: PlayerIntent },
    /// Query the current playback state and queue.
    Status,
}

/// The response to a control request, sent back as one JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub is_playing: bool,
    pub volume: f32,
    pub current_track: Option<Track>,
    pub queue: QueueSnapshot,
    /// Present only if the request failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
