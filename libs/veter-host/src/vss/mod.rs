// VSS host engine: APC envelope extractor + fragment reassembler for
// the Veter State Snapshot extension. See `doc/session-manager.md`
// §4.5 for the protocol-level role.
//
// Wire-format types live in the `vss-protocol` crate and are
// re-exported here for convenience.

pub mod state;

pub use state::{CompletedSnapshot, RejectReason, VssEngine};

pub use vss_protocol::*;
