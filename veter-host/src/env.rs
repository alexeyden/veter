//! Environment variables veter exports into every child process.
//!
//! An out-of-band client — one that is not the foreground program of
//! its pane — can never read anything the terminal sends back: the
//! reply lands in the pane's input queue, where the foreground program
//! is blocked in `read()`, and whoever the kernel wakes gets the bytes.
//! Everything such a client needs must therefore be reachable without a
//! round-trip. See `doc/vector-graphics-extension.md` §11.1.
//!
//! The split is deliberate:
//!
//! * **Live** values — cell pixel dimensions — come from `TIOCGWINSZ`,
//!   which stays correct across a font-size or DPI change. They are
//!   *not* duplicated here; an env var would go stale silently.
//! * **Static** values — the protocol caps, which are compiled-in
//!   constants — come from these variables, since no ioctl carries them.
//!
//! Both this module and the probe response read from the same `Limits`
//! defaults, so the two sources cannot drift.

/// Name of the marker variable. Its presence means "you are running
/// under veter"; its value is the terminal's version.
pub const VAR_VETER: &str = "VETER";

/// Name of the static-limits variable, encoded by [`limits_value`].
pub const VAR_VETER_LIMITS: &str = "VETER_LIMITS";

/// The static protocol caps a client cannot learn from any ioctl,
/// encoded as comma-separated `key=value` pairs.
///
/// Readers MUST ignore keys they don't recognise, so new caps can be
/// added without a format break, and MUST fall back to the recommended
/// defaults in `doc/vector-graphics-extension.md` §11 when the variable
/// is absent entirely.
///
/// | key    | meaning                              |
/// |--------|--------------------------------------|
/// | `mib`  | VGE `max_image_bytes`                |
/// | `mi`   | VGE `max_images`                     |
/// | `enc`  | VGE `supported_image_encodings` mask |
/// | `nest` | VGE `max_nesting_depth`              |
/// | `mwb`  | PRT `max_write_bytes`                |
pub fn limits_value() -> String {
    let vge = crate::vge::state::Limits::default();
    let prt = crate::prt::Limits::default();
    format!(
        "mib={},mi={},enc={},nest={},mwb={}",
        vge.max_image_bytes,
        vge.max_images,
        vge.supported_image_encodings,
        vge.max_nesting_depth,
        prt.max_write_bytes,
    )
}

/// The `(name, value)` pairs a host sets in every child's environment.
/// `version` is the terminal's own version string.
pub fn host_vars(version: &str) -> [(&'static str, String); 2] {
    [
        (VAR_VETER, version.to_string()),
        (VAR_VETER_LIMITS, limits_value()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_value_matches_the_probe_defaults() {
        // The env var and the probe response must never disagree —
        // a blind client trusts one, an interactive client the other.
        let vge = crate::vge::state::Limits::default();
        let prt = crate::prt::Limits::default();
        let v = limits_value();
        for expected in [
            format!("mib={}", vge.max_image_bytes),
            format!("mi={}", vge.max_images),
            format!("enc={}", vge.supported_image_encodings),
            format!("nest={}", vge.max_nesting_depth),
            format!("mwb={}", prt.max_write_bytes),
        ] {
            assert!(v.contains(&expected), "{v} is missing {expected}");
        }
    }

    #[test]
    fn limits_value_parses_as_key_value_pairs() {
        // Shape check: every pair splits on exactly one `=`, and no
        // value is empty. A malformed entry would silently give a
        // blind client a zero cap.
        for pair in limits_value().split(',') {
            let mut it = pair.splitn(2, '=');
            let k = it.next().unwrap();
            let val = it.next().unwrap_or_else(|| panic!("no `=` in {pair}"));
            assert!(!k.is_empty() && !val.is_empty(), "bad pair {pair}");
            assert!(val.parse::<u64>().is_ok(), "non-numeric value in {pair}");
        }
    }

    #[test]
    fn host_vars_carries_the_version() {
        let vars = host_vars("1.2.3");
        assert_eq!(vars[0], (VAR_VETER, "1.2.3".to_string()));
        assert_eq!(vars[1].0, VAR_VETER_LIMITS);
    }
}
