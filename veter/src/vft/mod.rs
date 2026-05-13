// VFT engine lives in the `veter-host` crate so headless consumers
// (veterd) can use it without dragging GUI dependencies. The veter
// binary re-exports everything here for backwards compatibility with
// existing `crate::vft::*` call sites.

pub use veter_host::vft::*;
