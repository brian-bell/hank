//! hank — Hank: a read-only TUI over a `bd` multi-repo hub.
//!
//! Library crate exposing the modules the binary and integration tests drive.
//! Only `main` resolves real XDG paths; everything here is I/O-injectable so it
//! is testable without touching the environment or a real `bd`.

pub mod app;
pub mod bd;
pub mod cache;
pub mod cli;
pub mod config;
pub mod hub;
pub mod refresh;
pub mod runtime;
pub mod snapshot;
pub mod ui_state;

#[cfg(test)]
mod perf_tests;
