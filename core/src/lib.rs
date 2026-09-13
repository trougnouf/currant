// ./core/src/lib.rs
pub mod controller;
pub mod matcher;
pub mod model;
pub mod scanner;
pub mod store;

#[cfg(feature = "android")]
uniffi::setup_scaffolding!();
