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
    pub scale_factor: f32,
    pub max_image_bytes: u32,
    pub max_images: u32,
    /// Bitmask: 0x01 = Raw RGBA8, 0x02 = WebP.
    pub supported_image_encodings: u8,
    pub max_nesting_depth: u8,
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
        Ok(b) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        Err(_) => 1.0,
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
