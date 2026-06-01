// VSS engine state lives in the `veter-host` crate so headless
// consumers (vsd) can use it without dragging in GUI dependencies.
// The veter binary re-exports the state surface here for parity with
// the other extension modules (prt, vge, vft).

pub use veter_host::vss::*;
