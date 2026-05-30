//! vge-render — shared client-side helpers for talking VGE to a
//! VGE-aware terminal.
//!
//! Extracted from `vcat` so `vcat` and `vplay` share one implementation
//! of: the raw-tty + poll/read helpers ([`tty`]), the probe handshake
//! ([`probe`]), image placement math ([`placement`]), chunked image
//! upload encoding ([`upload`]), and response (chunk-ack) parsing
//! ([`response`]). These crates are pure consumers of `vge-protocol`;
//! no terminal state or rendering lives here.

pub mod placement;
pub mod probe;
pub mod response;
pub mod tty;
pub mod upload;

pub use placement::{Placement, compute_placement};
pub use probe::{ProbeData, parse_probe_payload, run_probe};
pub use upload::{Encoding, choose_encoding, encode_payload};

/// True if any of the standard sshd-set env vars is present in this
/// process's environment. `SSH_CONNECTION` / `SSH_CLIENT` are set on
/// interactive logins; `SSH_TTY` is set when a tty is allocated. Any one
/// of them is enough to mean "this shell came in over ssh."
pub fn is_ssh_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}
