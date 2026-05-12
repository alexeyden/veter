//! veter — host engines + renderer, exposed as a library.
//!
//! The library face exists so workspace crates that don't run a GUI
//! event loop (notably `veterd`, the persistent session manager in
//! `doc/session-manager.md`) can link against the same vt100 / PRT /
//! VGE / VFT state machinery the local GUI binary uses. Such consumers
//! get an authoritative host implementation without re-deriving its
//! invariants.
//!
//! The `[[bin]]` target in `Cargo.toml` is the GUI binary itself
//! (`src/main.rs`) and consumes this library directly as `veter::*`.
//!
//! Visibility rule of thumb: modules whose state is essential to a
//! veterd snapshot/replay path live here as `pub mod`. Modules that
//! are purely about the local GUI (winit window setup, the
//! `App` event-loop handler) stay in `src/main.rs`.

pub mod clipboard;
pub mod prt;
pub mod pty;
pub mod renderer;
pub mod vft;
pub mod vge;
