// Host-side VFT engine: command dispatch, transfer table, worker
// threads. Wire-format types live in the `vft-protocol` crate and are
// re-exported here for convenience.
//
// Phase B introduces the engine; Phase C (vsend / vrecv) and Phase D
// (per-portal engines) consume the rest of these re-exports, so allow
// them unused for now.
#![allow(unused_imports)]

pub mod path;
pub mod state;
pub mod worker;

pub use state::{Limits, VftEngine};
pub use worker::Wakeup;

pub use vft_protocol::*;
