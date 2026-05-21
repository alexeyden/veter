// Envelope wrapping and the high-level encode helpers. Mirrors the
// shape of vss-protocol/envelope.rs.

use super::codec::stuff;
use super::frame::*;
use super::frames::{Command, HostFrame};

/// Append a single frame to an unstuffed payload buffer.
/// Frame layout (§1.2): u8 frame_type, u32 request_id, u32 body_length,
/// body[body_length].
///
/// Unlike VSS, SES uses `request_id`: a command carries a
/// client-chosen id and its response echoes it, so the client can
/// correlate pipelined probes.
pub fn append_frame(buf: &mut Vec<u8>, frame_type: u8, request_id: u32, body: &[u8]) {
    buf.push(frame_type);
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

fn wrap(frames_buf: &[u8], marker: &[u8; 3]) -> Vec<u8> {
    // §1.2 unstuffed payload = u8 protocol_version, u32 payload_length,
    // frames. `payload_length` is "length of the rest" — just the
    // frames region.
    let mut unstuffed = Vec::with_capacity(5 + frames_buf.len());
    unstuffed.push(PROTOCOL_VERSION);
    unstuffed.extend_from_slice(&(frames_buf.len() as u32).to_le_bytes());
    unstuffed.extend_from_slice(frames_buf);

    let mut env = Vec::with_capacity(7 + unstuffed.len());
    env.push(ESC);
    env.push(APC_OPEN);
    env.extend_from_slice(marker);
    stuff(&unstuffed, &mut env);
    env.push(ESC);
    env.push(ST_CLOSE);
    env
}

/// Wrap a frame buffer as a client→host envelope (uppercase `SES`).
pub fn wrap_c2h_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_C2H)
}

/// Wrap a frame buffer as a host→client envelope (lowercase `ses`).
pub fn wrap_h2c_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_H2C)
}

/// Append a typed command to a frames buffer.
pub fn append_command(buf: &mut Vec<u8>, cmd: &Command, request_id: u32) {
    let body = cmd.encode_body();
    append_frame(buf, cmd.frame_type(), request_id, &body);
}

/// Append a typed host response to a frames buffer.
pub fn append_host_frame(buf: &mut Vec<u8>, frame: &HostFrame, request_id: u32) {
    let body = frame.encode_body();
    append_frame(buf, frame.frame_type(), request_id, &body);
}

/// Encode a single command as one client→host envelope.
pub fn encode_command(cmd: &Command, request_id: u32) -> Vec<u8> {
    let mut frames = Vec::new();
    append_command(&mut frames, cmd, request_id);
    wrap_c2h_envelope(&frames)
}

/// Encode a single host response as one host→client envelope.
pub fn encode_host_frame(frame: &HostFrame, request_id: u32) -> Vec<u8> {
    let mut frames = Vec::new();
    append_host_frame(&mut frames, frame, request_id);
    wrap_h2c_envelope(&frames)
}

/// Read a complete payload off the wire (after APC unstuffing) and
/// yield its frames as `(frame_type, request_id, body)` tuples by
/// invoking `visit` for each frame. Returns `Err` on header / size
/// inconsistencies.
pub fn for_each_frame<F>(payload: &[u8], mut visit: F) -> Result<(), u16>
where
    F: FnMut(u8, u32, &[u8]) -> Result<(), u16>,
{
    use super::codec::Reader;
    let mut r = Reader::new(payload);
    let version = r.u8()?;
    if version != PROTOCOL_VERSION {
        return Err(ERR_BAD_PAYLOAD);
    }
    let payload_len = r.u32()? as usize;
    if payload_len + 5 != payload.len() {
        return Err(ERR_BAD_PAYLOAD);
    }
    while !r.at_end() {
        let frame_type = r.u8()?;
        let request_id = r.u32()?;
        let body_len = r.u32()? as usize;
        let body = r.take(body_len)?;
        visit(frame_type, request_id, body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apc::ApcStream;

    #[test]
    fn command_envelope_round_trip() {
        let env = encode_command(&Command::Detach, 0x1234);
        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);

        let mut frames = Vec::new();
        for_each_frame(&out.payloads[0], |t, rid, body| {
            frames.push((rid, Command::parse(t, body).map_err(|_| 0u16)?));
            Ok(())
        })
        .unwrap();
        assert_eq!(frames, vec![(0x1234, Command::Detach)]);
    }

    #[test]
    fn host_frame_envelope_round_trip() {
        let frame = HostFrame::ProbeResponse {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
            in_session: true,
            name: "session-name".to_string(),
        };
        let env = encode_host_frame(&frame, 7);
        let mut s = ApcStream::with_marker(*MARKER_H2C);
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);

        let mut got = Vec::new();
        for_each_frame(&out.payloads[0], |t, rid, body| {
            got.push((rid, HostFrame::parse(t, body).map_err(|_| 0u16)?));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, vec![(7, frame)]);
    }

    #[test]
    fn multiple_frames_in_one_envelope() {
        let mut frames = Vec::new();
        append_command(&mut frames, &Command::Probe, 1);
        append_command(&mut frames, &Command::Detach, 2);
        let env = wrap_c2h_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);

        let mut got = Vec::new();
        for_each_frame(&out.payloads[0], |t, rid, body| {
            got.push((rid, Command::parse(t, body).map_err(|_| 0u16)?));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, vec![(1, Command::Probe), (2, Command::Detach)]);
    }

    #[test]
    fn for_each_frame_rejects_wrong_version() {
        let mut payload = vec![99u8]; // wrong protocol_version
        payload.extend_from_slice(&0u32.to_le_bytes());
        assert!(for_each_frame(&payload, |_, _, _| Ok::<(), u16>(())).is_err());
    }

    #[test]
    fn for_each_frame_rejects_length_mismatch() {
        let mut payload = vec![PROTOCOL_VERSION];
        payload.extend_from_slice(&100u32.to_le_bytes()); // claims 100 bytes
        payload.extend_from_slice(&[0u8; 5]); // only 5 follow
        assert!(for_each_frame(&payload, |_, _, _| Ok::<(), u16>(())).is_err());
    }

    #[test]
    fn esc_bytes_in_body_survive_envelope() {
        let frame = HostFrame::Err {
            code: ERR_INTERNAL,
            // a string whose UTF-8 has no ESC, but force ESC via name
            msg: "\u{1b}weird\u{1b}".to_string(),
        };
        let env = encode_host_frame(&frame, 0);
        let mut s = ApcStream::with_marker(*MARKER_H2C);
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);
        let mut got = None;
        for_each_frame(&out.payloads[0], |t, _rid, body| {
            got = Some(HostFrame::parse(t, body).map_err(|_| 0u16)?);
            Ok(())
        })
        .unwrap();
        assert_eq!(got.unwrap(), frame);
    }
}
