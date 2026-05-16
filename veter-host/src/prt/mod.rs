// PRT host engine: portal tree, per-portal vt100 instances, command
// dispatch. Renderer code (femtovg / TerminalRenderer) lives in the
// `veter` binary's `prt::render`; this module is GUI-free so headless
// consumers (veterd) can link it.
//
// Wire-format types live in the `prt-protocol` crate and are
// re-exported here for convenience.
#![allow(unused_imports)]

pub mod portal;
pub mod snapshot;
pub mod state;

pub use portal::{Portal, PortalAnchor, PortalSet};
pub use snapshot::SnapshotError;
pub use state::{FocusKind, Limits, PrtEngine, PrtState};

pub use prt_protocol::*;
