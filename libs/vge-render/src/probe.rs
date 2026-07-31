//! The VGE probe handshake: ask the terminal for its capabilities and
//! cell pixel dimensions. Extracted from vcat and extended to surface
//! the fields an interactive client (vplay) needs.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use vge_protocol::apc::ApcStream;
use vge_protocol::codec::Reader;
use vge_protocol::command::Command;
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::*;

use crate::tty::{poll_stdin_until, read_stdin};

#[derive(Debug, Clone, Copy)]
pub struct ProbeData {
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
    /// Device pixels per logical pixel (HiDPI). `None` when the data
    /// came from [`from_environment`] rather than a probe response:
    /// it is a live value with no non-round-trip source. Nothing needs
    /// it to draw — `cell_pixel_*` are already device pixels — so a
    /// blind client can work without it.
    pub scale_factor: Option<f32>,
    pub max_image_bytes: u32,
    pub max_images: u32,
    /// Bitmask: 0x01 = Raw RGBA8, 0x02 = WebP.
    pub supported_image_encodings: u8,
    pub max_nesting_depth: u8,
}

/// Recommended defaults from `doc/vector-graphics-extension.md` §11.
/// A client that cannot read replies and finds no `VETER_LIMITS` in its
/// environment MUST assume these rather than guessing larger.
const DEFAULT_MAX_IMAGE_BYTES: u32 = 32 * 1024 * 1024;
const DEFAULT_MAX_IMAGES: u32 = 1024;
const DEFAULT_IMAGE_ENCODINGS: u8 = 0x01; // Raw only — the safe subset.
const DEFAULT_MAX_NESTING_DEPTH: u8 = 16;

/// Assemble capabilities with no round-trip at all, from `TIOCGWINSZ`
/// (live cell dimensions) and the environment veter exports (static
/// caps). See `doc/vector-graphics-extension.md` §11.1.
///
/// Returns `None` unless `$VETER` is set — that variable is the only
/// evidence available without a reply that the terminal speaks VGE —
/// and the ioctl reports usable pixel dimensions. Callers fall back to
/// [`run_probe`], which is still the authority for an interactive
/// client that can read its own input.
pub fn from_environment() -> Option<ProbeData> {
    std::env::var("VETER").ok().filter(|v| !v.is_empty())?;
    let ws = crate::tty::winsize()?;
    if ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    let limits = std::env::var("VETER_LIMITS").unwrap_or_default();
    Some(ProbeData {
        cell_pixel_width: ws.ws_xpixel / ws.ws_col,
        cell_pixel_height: ws.ws_ypixel / ws.ws_row,
        scale_factor: None,
        max_image_bytes: limit_key(&limits, "mib").unwrap_or(DEFAULT_MAX_IMAGE_BYTES as u64) as u32,
        max_images: limit_key(&limits, "mi").unwrap_or(DEFAULT_MAX_IMAGES as u64) as u32,
        supported_image_encodings: limit_key(&limits, "enc")
            .unwrap_or(DEFAULT_IMAGE_ENCODINGS as u64) as u8,
        max_nesting_depth: limit_key(&limits, "nest")
            .unwrap_or(DEFAULT_MAX_NESTING_DEPTH as u64) as u8,
    })
}

/// Look up one `key=value` pair in a `VETER_LIMITS` string. Unknown
/// keys are ignored by construction, so the host can add caps without
/// a format break; an unparseable value is treated as absent so the
/// caller falls back to the §11 default rather than to zero.
fn limit_key(limits: &str, key: &str) -> Option<u64> {
    limits
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}

/// [`from_environment`] if it can answer, otherwise [`run_probe`].
///
/// **This is for clients that cannot read replies.** An interactive
/// client should call [`run_probe`] instead, even though this is
/// faster, because the environment is not proof that the terminal on
/// the other end of stdout speaks VGE. `VETER` is inherited by every
/// descendant, including ones behind an intermediary — `veter → tmux →
/// client` — that does not relay APC envelopes. A probe that times out
/// detects that correctly; an inherited variable does not. The
/// non-zero-pixel requirement in [`from_environment`] filters out
/// intermediaries that rebuild the winsize, but not those that forward
/// it, so this remains a "best available evidence" answer rather than
/// proof.
pub fn probe_or_environment(timeout: Duration) -> Result<Option<ProbeData>> {
    match from_environment() {
        Some(data) => Ok(Some(data)),
        None => run_probe(timeout),
    }
}

/// Send a `Probe` and wait for the terminal's `ProbeResponse`, up to
/// `timeout`. Returns `Ok(None)` on timeout (terminal likely does not
/// speak VGE).
pub fn run_probe(timeout: Duration) -> Result<Option<ProbeData>> {
    let env = build_envelope(&[(Command::Probe, 1)]);
    {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&env)?;
        stdout.flush()?;
    }

    let mut apc = ApcStream::with_marker(*MARKER_T2C);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        if !poll_stdin_until(deadline)? {
            return Ok(None);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        let out = apc.feed(&buf[..n]);
        if let Some(payload) = out.payloads.into_iter().next() {
            return Ok(Some(parse_probe_payload(&payload)?));
        }
    }
}

/// Parse a single `ProbeResponse` envelope payload. Tolerates short
/// bodies from older hosts: any field past `cell_pixel_height` that the
/// host didn't send falls back to a sensible default.
pub fn parse_probe_payload(payload: &[u8]) -> Result<ProbeData> {
    let mut r = Reader::new(payload);
    let _version = r
        .u8()
        .map_err(|_| anyhow!("probe payload: missing version"))?;
    let _payload_len = r
        .u32()
        .map_err(|_| anyhow!("probe payload: missing length"))?;
    let frame_type = r
        .u8()
        .map_err(|_| anyhow!("probe payload: missing frame type"))?;
    if frame_type != RSP_PROBE {
        bail!(
            "expected ProbeResponse (0x{:02X}), got 0x{:02X}",
            RSP_PROBE,
            frame_type
        );
    }
    let _req_id = r
        .u32()
        .map_err(|_| anyhow!("probe payload: missing request_id"))?;
    let _body_len = r
        .u32()
        .map_err(|_| anyhow!("probe payload: missing body_len"))?;
    let _proto = r
        .u16()
        .map_err(|_| anyhow!("probe body: protocol_version"))?;
    let cw = r
        .u16()
        .map_err(|_| anyhow!("probe body: cell_pixel_width"))?;
    let ch = r
        .u16()
        .map_err(|_| anyhow!("probe body: cell_pixel_height"))?;

    // Optional trailing fields (§2.1). Read with graceful fallback so a
    // host that advertises a shorter body still parses.
    let scale_factor = match r.take(4) {
        Ok(b) => Some(f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        Err(_) => None,
    };
    let _max_elements = r.u32().unwrap_or(0);
    let _max_commands_per_element = r.u32().unwrap_or(0);
    let _max_text_bytes = r.u32().unwrap_or(0);
    let max_image_bytes = r.u32().unwrap_or(0);
    let max_images = r.u32().unwrap_or(0);
    let supported_image_encodings = r.u8().unwrap_or(0x01);
    let max_nesting_depth = r.u8().unwrap_or(0);

    Ok(ProbeData {
        cell_pixel_width: cw,
        cell_pixel_height: ch,
        scale_factor,
        max_image_bytes,
        max_images,
        supported_image_encodings,
        max_nesting_depth,
    })
}

#[cfg(test)]
mod tests {
    use super::limit_key;

    #[test]
    fn limit_key_reads_known_keys() {
        let s = "mib=33554432,mi=1024,enc=3,nest=16,mwb=1048576";
        assert_eq!(limit_key(s, "mib"), Some(33_554_432));
        assert_eq!(limit_key(s, "enc"), Some(3));
        assert_eq!(limit_key(s, "mwb"), Some(1_048_576));
    }

    #[test]
    fn limit_key_ignores_unknown_and_malformed() {
        // Unknown keys are how the host adds caps without a format
        // break, so they must not disturb the ones we do know.
        let s = "mib=17,somethingnew=x,mi=,enc=notanumber";
        assert_eq!(limit_key(s, "mib"), Some(17));
        assert_eq!(limit_key(s, "somethingnew"), None);
        // Empty and non-numeric values read as absent, so the caller
        // falls back to the §11 default instead of to zero — a zero
        // cap would make every upload fail.
        assert_eq!(limit_key(s, "mi"), None);
        assert_eq!(limit_key(s, "enc"), None);
        assert_eq!(limit_key("", "mib"), None);
    }
}
