//! Shared client-side scaffolding for `vsend` and `vrecv`.
//!
//! Each binary owns its own command flow but reuses:
//!
//!   * `tty`       — raw-mode guard + nonblocking poll/read helpers.
//!   * `probe`     — VFT and VGE probe round-trips, plus the DSR-CPR
//!                   cursor-row query so the progress bar knows which
//!                   row to anchor itself at.
//!   * `stream`    — a stdin reader thread that demultiplexes VGE
//!                   (`vge`) and VFT (`vft`) host envelopes onto a
//!                   single channel of typed `HostFrame` values.
//!   * `progress`  — a `ProgressUI` trait with VGE-driven and ASCII
//!                   fallback implementations.

pub mod probe;
pub mod progress;
pub mod stream;
pub mod tty;
