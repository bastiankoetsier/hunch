//! hunch: ask Jev (TypeSafe's System One model) for gut-check judgments.
//!
//! The binary in `main.rs` is a thin shell around this library so that both
//! unit tests and the integration tests in `tests/` can exercise the logic.

pub mod app;
pub mod cli;
pub mod config;
pub mod driver;
pub mod render;
pub mod wire;
