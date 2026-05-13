// Host-side VFT engine: command dispatch, transfer table, worker
// threads. Wire-format types live in the `vft-protocol` crate and are
// re-exported here for convenience.
//
// The file-picker (`pick_open_path`) and post-finalize "open in
// default app" hooks are gated behind the `gui` feature. Headless
// consumers (veterd) leave the feature off; the picker returns
// `Cancelled` and the open-after step is a no-op, which is the
// right semantics for a daemon — the real picker lives on the
// renderer side.
#![allow(unused_imports)]

pub mod path;
pub mod state;
pub mod worker;

pub use state::{Limits, VftEngine};
pub use worker::Wakeup;

pub use vft_protocol::*;
