//! Parse terminal→client response envelopes — currently the chunk-ack
//! stream that drives upload progress. Extracted from vcat.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use vge_protocol::apc::ApcStream;
use vge_protocol::codec::Reader;
use vge_protocol::frame::*;

use crate::tty::{poll_stdin_until, read_stdin};

/// Read response envelopes until we see a ChunkAck whose request_id
/// matches `expected_req`, then return its `bytes_received` field.
/// Returns `Ok(None)` on timeout. Used after each UploadImage chunk so
/// the sender can drive a progress UI from the host's view of bytes
/// actually committed (§4).
pub fn wait_for_chunk_ack(
    expected_image_id: &str,
    expected_req: u32,
    timeout: Duration,
) -> Result<Option<u32>> {
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
        for payload in out.payloads {
            if let Some(bytes) = find_chunk_ack(&payload, expected_image_id, expected_req)? {
                return Ok(Some(bytes));
            }
            // Non-matching envelope (e.g. spurious responses from
            // earlier commands, or RSP_ERR). Keep reading — the next
            // envelope should carry the ack.
        }
    }
}

/// Scan a single response envelope payload for an `RSP_CHUNK_ACK`
/// frame matching `(image_id, req_id)`. Returns its `bytes_received`
/// if found. RSP_ERR frames matching `req_id` are surfaced as errors
/// (the host bailed mid-stream — e.g. budget exhausted).
pub fn find_chunk_ack(
    payload: &[u8],
    expected_image_id: &str,
    expected_req: u32,
) -> Result<Option<u32>> {
    let mut r = Reader::new(payload);
    let _version = r.u8().map_err(|_| anyhow!("chunk-ack envelope: version"))?;
    let _payload_len = r
        .u32()
        .map_err(|_| anyhow!("chunk-ack envelope: payload_len"))?;
    while !r.at_end() {
        let frame_type = r
            .u8()
            .map_err(|_| anyhow!("chunk-ack envelope: frame_type"))?;
        let req_id = r.u32().map_err(|_| anyhow!("chunk-ack envelope: req_id"))?;
        let body_len = r
            .u32()
            .map_err(|_| anyhow!("chunk-ack envelope: body_len"))? as usize;
        let body = r
            .take(body_len)
            .map_err(|_| anyhow!("chunk-ack envelope: body"))?;
        if frame_type == RSP_CHUNK_ACK && req_id == expected_req {
            let mut br = Reader::new(body);
            let img_id = br.string().map_err(|_| anyhow!("chunk-ack body: id"))?;
            let bytes = br.u32().map_err(|_| anyhow!("chunk-ack body: bytes"))?;
            if img_id != expected_image_id {
                bail!("chunk-ack id mismatch: expected {expected_image_id:?}, got {img_id:?}");
            }
            return Ok(Some(bytes));
        }
        if frame_type == RSP_ERR && req_id == expected_req {
            let mut er = Reader::new(body);
            let code = er.u16().unwrap_or(0);
            let msg = er.string().unwrap_or("");
            bail!("host rejected chunk req_id={expected_req}: code=0x{code:04X} {msg}");
        }
    }
    Ok(None)
}
