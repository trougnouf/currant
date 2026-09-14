// ./core/src/lib.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Currant core: library catalog, query engine, playback state and scrobbling.
pub mod control;
pub mod controller;
pub mod matcher;
pub mod metadata;
pub mod model;
pub mod scanner;
pub mod scrobble;
pub mod store;
pub mod text;
pub mod vorbis_ext;

#[cfg(feature = "android")]
uniffi::setup_scaffolding!();
