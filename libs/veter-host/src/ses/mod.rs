// SES host engine: APC envelope extractor + command dispatch for the
// Session Extension. See `doc/session-extension.md` for the
// protocol-level role.
//
// Wire-format types live in the `ses-protocol` crate and are
// re-exported here for convenience.

pub mod state;

pub use state::SesEngine;

pub use ses_protocol::*;
