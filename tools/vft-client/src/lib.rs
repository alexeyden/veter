//! Shared client-side scaffolding for `vsend` and `vrecv`.
//!
//! Each binary owns its own command flow but reuses:
//!
//!   * `tty`       — raw-mode guard + nonblocking poll/read helpers.
//!   * `probe`     — VFT and VGE probe round-trips. The progress bar
//!                   needs no cursor query: it names its row with a
//!                   cursor-relative origin and lets the terminal
//!                   resolve it (VGE spec §9.4 bit3).
//!   * `stream`    — a stdin reader thread that demultiplexes VGE
//!                   (`vge`) and VFT (`vft`) host envelopes onto a
//!                   single channel of typed `HostFrame` values.
//!   * `progress`  — a `ProgressUI` trait with VGE-driven and ASCII
//!                   fallback implementations.

pub mod cancel;
pub mod probe;
pub mod progress;
pub mod stream;
pub mod tty;
