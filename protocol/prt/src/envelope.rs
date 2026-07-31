// APC envelope wrapping (§1.1–1.2) for both directions, plus host-side
// body builders for ProbeResponse, Err, and every event in §8.

use crate::codec::{stuff, Writer};
use crate::frame::*;

/// Build the body for a ProbeResponse (§2.1).
///
/// `vge_features`, if `Some`, is appended as a trailing u8 byte carrying
/// the §10 VGE-integration capability bits. The body length is the
/// source of truth, so omitting it (`None`) is equivalent to advertising
/// "VGE-in-portal not supported" — clients reading a shorter body MUST
/// treat missing trailing fields as zero.
pub struct ProbeBody {
    pub protocol_version: u16,
    pub max_portals: u32,
    pub max_portal_cells_w: u32,
    pub max_portal_cells_h: u32,
    pub max_scrollback_lines: u32,
    pub max_write_bytes: u32,
    pub features: u8,
    pub max_nesting_depth: u8,
    pub vge_features: Option<u8>,
    /// §10 — when the host themes `host.*` styles
    /// (`FEAT_VGE_HOST_THEMED_STYLES`), the RGBA8 value `host.accent`
    /// resolves to for the probing engine's nesting depth. Lets a client
    /// derive its own shades (translucent, darkened) from the same accent
    /// it references by `StyleRef`. Encoded only when `Some`, and only
    /// meaningful when the themed bit is set in `vge_features`.
    pub accent_rgba: Option<[u8; 4]>,
}

impl ProbeBody {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(30);
        w.u16(self.protocol_version);
        w.u32(self.max_portals);
        w.u32(self.max_portal_cells_w);
        w.u32(self.max_portal_cells_h);
        w.u32(self.max_scrollback_lines);
        w.u32(self.max_write_bytes);
        w.u8(self.features);
        w.u8(self.max_nesting_depth);
        // `accent_rgba` follows `vge_features`, so it can only be present
        // when `vge_features` is too (no positional gap).
        if let Some(extra) = self.vge_features {
            w.u8(extra);
            if let Some(rgba) = self.accent_rgba {
                for b in rgba {
                    w.u8(b);
                }
            }
        }
        w.buf
    }
}

/// Build the body for an Err response (§4.1).
pub fn err_body(error_code: u16, message: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + 1 + message.len());
    w.u16(error_code);
    w.str(message);
    w.buf
}

// ---- §8 event bodies ---------------------------------------------------

pub fn raw_reply_body(id: &str, data: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len() + 5 + data.len());
    w.str(id);
    w.bytes(data);
    w.buf
}

pub fn bell_body(id: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len());
    w.str(id);
    w.buf
}

/// Body for an `EVT_PORTAL_ACTIVITY` event (§8). Carries only the
/// portal id; the activity heuristic lives host-side.
pub fn portal_activity_body(id: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len());
    w.str(id);
    w.buf
}

pub fn title_change_body(id: &str, title: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len() + title.len());
    w.str(id);
    w.str(title);
    w.buf
}

pub fn icon_name_change_body(id: &str, name: &str) -> Vec<u8> {
    title_change_body(id, name)
}

pub fn working_dir_change_body(id: &str, uri: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len() + uri.len());
    w.str(id);
    w.str(uri);
    w.buf
}

/// `selection` is the ASCII byte ('c', 'p', 's', …); `op` is
/// `CLIPBOARD_SET` (0) or `CLIPBOARD_QUERY` (1). For queries the spec
/// says `data` is empty; we still encode it consistently as a `bytes`
/// field so the wire layout stays uniform.
pub fn clipboard_op_body(id: &str, selection: u8, op: u8, data: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(3 + id.len() + 5 + data.len());
    w.str(id);
    w.u8(selection);
    w.u8(op);
    w.bytes(data);
    w.buf
}

pub fn cursor_visibility_change_body(id: &str, visible: bool) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len());
    w.str(id);
    w.u8(u8::from(visible));
    w.buf
}

pub fn buffer_mode_change_body(id: &str, on_alt: bool) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len());
    w.str(id);
    w.u8(u8::from(on_alt));
    w.buf
}

pub fn portal_evicted_body(id: &str, reason: u8) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len());
    w.str(id);
    w.u8(reason);
    w.buf
}

pub fn resize_notify_body(id: &str, rows: u32, cols: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(9 + id.len());
    w.str(id);
    w.u32(rows);
    w.u32(cols);
    w.buf
}

pub fn mouse_mode_change_body(
    id: &str,
    protocol: u8,
    encoding: u8,
    focus_events: u8,
) -> Vec<u8> {
    let mut w = Writer::with_capacity(4 + id.len());
    w.str(id);
    w.u8(protocol);
    w.u8(encoding);
    w.u8(focus_events);
    w.buf
}

pub fn portal_scroll_delta_body(id: &str, delta: i32) -> Vec<u8> {
    let mut w = Writer::with_capacity(5 + id.len());
    w.str(id);
    w.i32(delta);
    w.buf
}

pub fn portal_scroll_set_body(id: &str, offset: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(5 + id.len());
    w.str(id);
    w.u32(offset);
    w.buf
}

// ---- frame + envelope wrapping ----------------------------------------

/// Append a single frame to an unstuffed payload buffer.
/// Frame layout (§1.2): u8 frame_type, u32 request_id, u32 body_length,
/// body[body_length].
pub fn append_frame(buf: &mut Vec<u8>, frame_type: u8, request_id: u32, body: &[u8]) {
    buf.push(frame_type);
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

fn wrap(frames_buf: &[u8], marker: &[u8; 3]) -> Vec<u8> {
    // §1.2 unstuffed payload = u8 protocol_version, u32 payload_length,
    // frames. payload_length is "length of the rest" — i.e. just the
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

/// Wrap a frame buffer as a host→client envelope (lowercase `prt`
/// marker). This is what the host emits as responses and events.
pub fn wrap_t2c_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_T2C)
}

/// Wrap a frame buffer as a client→host envelope (uppercase `PRT`
/// marker). Used by clients (and the test CLI) to feed commands.
pub fn wrap_c2t_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_C2T)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apc::ApcStream;
    use crate::codec::Reader;

    #[test]
    fn probe_body_encoded_size() {
        // Required prefix: 2 + 4*5 + 1 + 1 = 24 bytes.
        let pb = ProbeBody {
            protocol_version: 0,
            max_portals: 64,
            max_portal_cells_w: 1024,
            max_portal_cells_h: 512,
            max_scrollback_lines: 100_000,
            max_write_bytes: 1 << 20,
            features: 0xFF,
            max_nesting_depth: 8,
            vge_features: None,
            accent_rgba: None,
        };
        assert_eq!(pb.encode().len(), 24);

        // With trailing VGE byte: 25.
        let pb2 = ProbeBody {
            vge_features: Some(FEAT_VGE_IN_PORTAL),
            ..pb
        };
        assert_eq!(pb2.encode().len(), 25);

        // With VGE byte + accent RGBA: 25 + 4 = 29.
        let pb3 = ProbeBody {
            vge_features: Some(FEAT_VGE_IN_PORTAL | FEAT_VGE_HOST_THEMED_STYLES),
            accent_rgba: Some([0x12, 0x34, 0x56, 0xFF]),
            ..pb
        };
        assert_eq!(pb3.encode().len(), 29);

        // accent_rgba without vge_features cannot be encoded (no gap):
        // it is silently dropped, length stays at the 24-byte prefix.
        let pb4 = ProbeBody {
            vge_features: None,
            accent_rgba: Some([1, 2, 3, 4]),
            ..pb
        };
        assert_eq!(pb4.encode().len(), 24);
    }

    #[test]
    fn t2c_envelope_passes_through_apc_stream() {
        // ApcStream defaults to MARKER_C2T; a T2C envelope must come back
        // as plain passthrough.
        let mut frames = Vec::new();
        append_frame(&mut frames, RSP_OK, 42, &[]);
        let env = wrap_t2c_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.payloads.is_empty());
        assert_eq!(out.passthrough, env);
    }

    #[test]
    fn c2t_envelope_round_trips_with_stuffing() {
        let mut frames = Vec::new();
        // Body deliberately contains ESC bytes to exercise stuffing.
        append_frame(&mut frames, RSP_OK, 0xDEAD_BEEF, &[0x1B, 0x00, 0x1B]);
        let env = wrap_c2t_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);

        let mut r = Reader::new(&out.payloads[0]);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let payload_len = r.u32().unwrap();
        assert_eq!(payload_len as usize, frames.len());
        assert_eq!(r.u8().unwrap(), RSP_OK);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u32().unwrap(), 3);
    }

    #[test]
    fn raw_reply_body_round_trip() {
        let body = raw_reply_body("left", b"\x1b[6;42R");
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "left");
        assert_eq!(r.bytes().unwrap(), b"\x1b[6;42R");
        assert!(r.at_end());
    }

    #[test]
    fn mouse_mode_change_body_round_trip() {
        let body = mouse_mode_change_body("p", 2, 2, 1);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u8().unwrap(), 2);
        assert_eq!(r.u8().unwrap(), 2);
        assert_eq!(r.u8().unwrap(), 1);
        assert!(r.at_end());
    }

    #[test]
    fn portal_scroll_delta_body_round_trip() {
        let body = portal_scroll_delta_body("pane-1", -3);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "pane-1");
        assert_eq!(r.i32().unwrap(), -3);
        assert!(r.at_end());
    }

    #[test]
    fn portal_scroll_set_body_round_trip() {
        let body = portal_scroll_set_body("pane-2", 1234);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "pane-2");
        assert_eq!(r.u32().unwrap(), 1234);
        assert!(r.at_end());
    }

    #[test]
    fn portal_evicted_body_round_trip() {
        let body = portal_evicted_body("test-#123", EVICT_SCROLLBACK);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "test-#123");
        assert_eq!(r.u8().unwrap(), EVICT_SCROLLBACK);
        assert!(r.at_end());
    }

    #[test]
    fn resize_notify_body_round_trip() {
        let body = resize_notify_body("p", 24, 80);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u32().unwrap(), 24);
        assert_eq!(r.u32().unwrap(), 80);
        assert!(r.at_end());
    }

    #[test]
    fn portal_activity_body_round_trip() {
        let body = portal_activity_body("p3");
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "p3");
        assert!(r.at_end());
    }

    #[test]
    fn err_body_round_trip() {
        let body = err_body(ERR_UNKNOWN_PORTAL, "id not found");
        let mut r = Reader::new(&body);
        assert_eq!(r.u16().unwrap(), ERR_UNKNOWN_PORTAL);
        assert_eq!(r.string().unwrap(), "id not found");
        assert!(r.at_end());
    }

    #[test]
    fn write_portal_body_in_envelope_unstuffs_correctly() {
        // Build a c2t envelope carrying CMD_WRITE_PORTAL with an ESC in
        // the inner data, run through ApcStream, decode, and verify.
        use crate::command::{parse, Command};

        let mut body = Writer::new();
        body.str("left");
        body.bytes(&[0x1B, b'[', b'H']);

        let mut frames = Vec::new();
        append_frame(&mut frames, CMD_WRITE_PORTAL, 7, &body.buf);
        let env = wrap_c2t_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);

        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _ = r.u32().unwrap();
        let frame_type = r.u8().unwrap();
        let request_id = r.u32().unwrap();
        let body_len = r.u32().unwrap();
        let body_bytes = r.take(body_len as usize).unwrap();
        assert_eq!(frame_type, CMD_WRITE_PORTAL);
        assert_eq!(request_id, 7);

        let cmd = parse(frame_type, body_bytes).unwrap();
        let Command::WritePortal(b) = cmd else {
            panic!();
        };
        assert_eq!(b.id, "left");
        assert_eq!(b.data, vec![0x1B, b'[', b'H']);
    }
}
