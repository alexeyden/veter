// VFT and VGE probe round-trips, plus the DSR-CPR cursor-position
// query used to anchor a progress bar at the right row.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::tty::{poll_stdin_until, read_stdin};

#[derive(Debug, Clone, Copy)]
pub struct VftProbeData {
    pub protocol_version: u16,
    pub max_concurrent_transfers: u32,
    pub max_chunk_bytes: u32,
    pub max_path_bytes: u32,
    pub max_file_bytes: u64,
    pub features: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct VgeProbeData {
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
}

/// Send a VFT `Probe` envelope and read the host's `ProbeResponse`,
/// timing out if no response arrives.
pub fn run_vft_probe(timeout: Duration) -> Result<Option<VftProbeData>> {
    use vft_protocol::apc::ApcStream;
    use vft_protocol::encode::build_envelope;
    use vft_protocol::frame::MARKER_H2C;

    let env = build_envelope(&[(vft_protocol::Command::Probe, 1)]);
    {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&env)?;
        stdout.flush()?;
    }

    let mut apc = ApcStream::with_marker(*MARKER_H2C);
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
            return Ok(Some(parse_vft_probe(&payload)?));
        }
    }
}

fn parse_vft_probe(payload: &[u8]) -> Result<VftProbeData> {
    use vft_protocol::codec::Reader;
    use vft_protocol::frame::*;
    let mut r = Reader::new(payload);
    let _version = r.u8().map_err(|_| anyhow!("vft probe: missing version"))?;
    let _payload_len = r
        .u32()
        .map_err(|_| anyhow!("vft probe: missing payload_len"))?;
    let frame_type = r.u8().map_err(|_| anyhow!("vft probe: missing frame_type"))?;
    if frame_type != RSP_PROBE {
        bail!(
            "expected vft ProbeResponse (0x{:02X}), got 0x{:02X}",
            RSP_PROBE,
            frame_type
        );
    }
    let _req_id = r
        .u32()
        .map_err(|_| anyhow!("vft probe: missing request_id"))?;
    let _body_len = r
        .u32()
        .map_err(|_| anyhow!("vft probe: missing body_len"))?;
    let protocol_version = r.u16().map_err(|_| anyhow!("vft probe body: version"))?;
    let max_concurrent_transfers = r
        .u32()
        .map_err(|_| anyhow!("vft probe body: max_concurrent_transfers"))?;
    let max_chunk_bytes = r.u32().map_err(|_| anyhow!("vft probe body: max_chunk_bytes"))?;
    let max_path_bytes = r.u32().map_err(|_| anyhow!("vft probe body: max_path_bytes"))?;
    let max_file_bytes = r.u64().map_err(|_| anyhow!("vft probe body: max_file_bytes"))?;
    let features = r.u8().map_err(|_| anyhow!("vft probe body: features"))?;
    Ok(VftProbeData {
        protocol_version,
        max_concurrent_transfers,
        max_chunk_bytes,
        max_path_bytes,
        max_file_bytes,
        features,
    })
}

/// Send a VGE `Probe` envelope and read the terminal's `ProbeResponse`.
/// Returns `None` if the terminal does not advertise VGE within
/// `timeout` (which is normal — VFT works without VGE; we just fall
/// back to the ASCII progress bar).
pub fn run_vge_probe(timeout: Duration) -> Result<Option<VgeProbeData>> {
    use vge_protocol::apc::ApcStream;
    use vge_protocol::encode::build_envelope;
    use vge_protocol::frame::MARKER_T2C;

    let env = build_envelope(&[(vge_protocol::Command::Probe, 1)]);
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
            return Ok(Some(parse_vge_probe(&payload)?));
        }
    }
}

fn parse_vge_probe(payload: &[u8]) -> Result<VgeProbeData> {
    use vge_protocol::codec::Reader;
    use vge_protocol::frame::*;
    let mut r = Reader::new(payload);
    let _version = r.u8().map_err(|_| anyhow!("vge probe: missing version"))?;
    let _payload_len = r
        .u32()
        .map_err(|_| anyhow!("vge probe: missing payload_len"))?;
    let frame_type = r.u8().map_err(|_| anyhow!("vge probe: missing frame_type"))?;
    if frame_type != RSP_PROBE {
        bail!(
            "expected vge ProbeResponse (0x{:02X}), got 0x{:02X}",
            RSP_PROBE,
            frame_type
        );
    }
    let _req_id = r
        .u32()
        .map_err(|_| anyhow!("vge probe: missing request_id"))?;
    let _body_len = r
        .u32()
        .map_err(|_| anyhow!("vge probe: missing body_len"))?;
    let _proto = r.u16().map_err(|_| anyhow!("vge probe body: protocol_version"))?;
    let cw = r.u16().map_err(|_| anyhow!("vge probe body: cell_pixel_width"))?;
    let ch = r.u16().map_err(|_| anyhow!("vge probe body: cell_pixel_height"))?;
    Ok(VgeProbeData {
        cell_pixel_width: cw,
        cell_pixel_height: ch,
    })
}

/// Read the current cursor row by emitting `ESC [ 6 n` and parsing the
/// `ESC [ <row> ; <col> R` reply. The row is 1-indexed.
pub fn read_cursor_row(timeout: Duration) -> Result<Option<u32>> {
    {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"\x1b[6n")?;
        stdout.flush()?;
    }
    let deadline = Instant::now() + timeout;
    let mut accum: Vec<u8> = Vec::with_capacity(32);
    let mut buf = [0u8; 64];
    loop {
        if !poll_stdin_until(deadline)? {
            return Ok(None);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        accum.extend_from_slice(&buf[..n]);
        if let Some(row) = parse_cursor_position(&accum)? {
            return Ok(Some(row));
        }
    }
}

/// Look for `ESC [ <row> ; <col> R` somewhere in `buf`. Returns the
/// 1-indexed row if found.
fn parse_cursor_position(buf: &[u8]) -> Result<Option<u32>> {
    let Some(esc_pos) = buf.iter().position(|&b| b == 0x1B) else {
        return Ok(None);
    };
    if esc_pos + 1 >= buf.len() {
        return Ok(None);
    }
    if buf[esc_pos + 1] != b'[' {
        return Ok(None);
    }
    let body_start = esc_pos + 2;
    let r_off = match buf[body_start..].iter().position(|&b| b == b'R') {
        Some(off) => off,
        None => return Ok(None),
    };
    let body = &buf[body_start..body_start + r_off];
    let body_str = std::str::from_utf8(body)
        .map_err(|_| anyhow!("cursor-position body not valid UTF-8"))?;
    let (row_str, _col) = body_str
        .split_once(';')
        .ok_or_else(|| anyhow!("cursor-position body lacks ';'"))?;
    let row: u32 = row_str
        .trim()
        .parse()
        .map_err(|_| anyhow!("cursor-position row not a u32: {body_str:?}"))?;
    Ok(Some(row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_position_parses() {
        assert_eq!(parse_cursor_position(b"\x1b[24;1R").unwrap(), Some(24));
    }

    #[test]
    fn cursor_position_with_leading_garbage() {
        assert_eq!(
            parse_cursor_position(b"hello\x1b[42;7Rworld").unwrap(),
            Some(42)
        );
    }

    #[test]
    fn cursor_position_partial_returns_none() {
        assert_eq!(parse_cursor_position(b"\x1b[24;").unwrap(), None);
    }
}
