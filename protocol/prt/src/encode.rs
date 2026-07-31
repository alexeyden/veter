// Client-side command body encoders (§3, §6, §7, §9).
//
// Mirrors the decoders in `command.rs` so commands can be round-tripped
// in tests and emitted by the prt-cli helper.

use crate::codec::Writer;
use crate::command::{
    Command, CreatePortalBody, CursorStyle, FocusTarget, UpdateOriginBody, WritePortalBody,
};
use crate::envelope::{append_frame, wrap_c2t_envelope};
use crate::frame::*;

#[cfg(test)]
use crate::command::AnchorMode;

pub fn probe_body() -> Vec<u8> {
    Vec::new()
}

pub fn create_portal_body(b: &CreatePortalBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + b.id.len() + 4 * 4 + 1 + 1 + 4 + 1 + 4);
    w.str(&b.id);
    w.u32(b.size_w);
    w.u32(b.size_h);
    w.i32(b.origin_x);
    w.i32(b.origin_y);
    w.u8(b.anchor_mode.as_u8());
    w.u8(u8::from(b.is_visible));
    w.i32(b.draw_order);
    w.u8(b.flags);
    w.u32(b.scrollback_lines);
    w.buf
}

pub fn delete_portal_body(id: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len());
    w.str(id);
    w.buf
}

pub fn update_size_body(id: &str, new_w: u32, new_h: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(9 + id.len());
    w.str(id);
    w.u32(new_w);
    w.u32(new_h);
    w.buf
}

pub fn update_origin_body(b: &UpdateOriginBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(10 + b.id.len());
    w.str(&b.id);
    w.i32(b.new_origin_x);
    w.i32(b.new_origin_y);
    w.u8(b.anchor_mode.as_u8());
    w.buf
}

pub fn update_visibility_body(id: &str, is_visible: bool) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len());
    w.str(id);
    w.u8(u8::from(is_visible));
    w.buf
}

pub fn update_draw_order_body(id: &str, draw_order: i32) -> Vec<u8> {
    let mut w = Writer::with_capacity(5 + id.len());
    w.str(id);
    w.i32(draw_order);
    w.buf
}

pub fn clear_all_body() -> Vec<u8> {
    Vec::new()
}

pub fn write_portal_body(b: &WritePortalBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + b.id.len() + 5 + b.data.len());
    w.str(&b.id);
    w.bytes(&b.data);
    w.buf
}

pub fn set_focus_body(target: &FocusTarget) -> Vec<u8> {
    let mut w = Writer::new();
    match target {
        FocusTarget::Host => {
            w.u8(crate::frame::FOCUS_HOST);
        }
        FocusTarget::Portal(id) => {
            w.u8(crate::frame::FOCUS_PORTAL);
            w.str(id);
        }
    }
    w.buf
}

pub fn set_cursor_style_body(unfocused: CursorStyle) -> Vec<u8> {
    let mut w = Writer::with_capacity(1);
    w.u8(unfocused.as_u8());
    w.buf
}

pub fn set_portal_scrollback_body(id: &str, lines: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(5 + id.len());
    w.str(id);
    w.u32(lines);
    w.buf
}

/// Discriminate the wire frame type of a `Command` (§3 table).
pub fn frame_type_for(cmd: &Command) -> u8 {
    match cmd {
        Command::Probe => CMD_PROBE,
        Command::CreatePortal(_) => CMD_CREATE_PORTAL,
        Command::DeletePortal { .. } => CMD_DELETE_PORTAL,
        Command::UpdateSize { .. } => CMD_UPDATE_SIZE,
        Command::UpdateOrigin(_) => CMD_UPDATE_ORIGIN,
        Command::UpdateVisibility { .. } => CMD_UPDATE_VISIBILITY,
        Command::UpdateDrawOrder { .. } => CMD_UPDATE_DRAW_ORDER,
        Command::ClearAll => CMD_CLEAR_ALL,
        Command::WritePortal(_) => CMD_WRITE_PORTAL,
        Command::SetFocus { .. } => CMD_SET_FOCUS,
        Command::SetCursorStyle { .. } => CMD_SET_CURSOR_STYLE,
        Command::SetPortalScrollback { .. } => CMD_SET_PORTAL_SCROLLBACK,
    }
}

/// Encode a `Command`'s body bytes (§6, §7, §9).
pub fn encode_command(cmd: &Command) -> Vec<u8> {
    match cmd {
        Command::Probe => probe_body(),
        Command::CreatePortal(b) => create_portal_body(b),
        Command::DeletePortal { id } => delete_portal_body(id),
        Command::UpdateSize { id, new_w, new_h } => update_size_body(id, *new_w, *new_h),
        Command::UpdateOrigin(b) => update_origin_body(b),
        Command::UpdateVisibility { id, is_visible } => {
            update_visibility_body(id, *is_visible)
        }
        Command::UpdateDrawOrder { id, draw_order } => {
            update_draw_order_body(id, *draw_order)
        }
        Command::ClearAll => clear_all_body(),
        Command::WritePortal(b) => write_portal_body(b),
        Command::SetFocus { target } => set_focus_body(target),
        Command::SetCursorStyle { unfocused } => set_cursor_style_body(*unfocused),
        Command::SetPortalScrollback { id, lines } => {
            set_portal_scrollback_body(id, *lines)
        }
    }
}

/// Bundle a slice of `(command, request_id)` pairs into a single
/// client-to-host APC envelope ready to write to a PTY (§1.2).
pub fn build_envelope(commands: &[(Command, u32)]) -> Vec<u8> {
    let mut frames = Vec::new();
    for (cmd, req_id) in commands {
        let body = encode_command(cmd);
        append_frame(&mut frames, frame_type_for(cmd), *req_id, &body);
    }
    wrap_c2t_envelope(&frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse;

    #[test]
    fn create_portal_round_trips() {
        let b = CreatePortalBody {
            id: "left".into(),
            size_w: 100,
            size_h: 60,
            origin_x: 0,
            origin_y: 0,
            anchor_mode: AnchorMode::Live,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 1000,
        };
        let body = create_portal_body(&b);
        let cmd = parse(CMD_CREATE_PORTAL, &body).unwrap();
        let Command::CreatePortal(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed.id, b.id);
        assert_eq!(parsed.size_w, b.size_w);
        assert_eq!(parsed.size_h, b.size_h);
        assert_eq!(parsed.origin_x, b.origin_x);
        assert_eq!(parsed.origin_y, b.origin_y);
        assert_eq!(parsed.anchor_mode, b.anchor_mode);
        assert_eq!(parsed.is_visible, b.is_visible);
        assert_eq!(parsed.draw_order, b.draw_order);
        assert_eq!(parsed.flags, b.flags);
        assert_eq!(parsed.scrollback_lines, b.scrollback_lines);
    }

    #[test]
    fn write_portal_round_trips() {
        let b = WritePortalBody {
            id: "left".into(),
            data: vec![0x1B, b'[', b'H', b'\x07', 0x1B],
        };
        let body = write_portal_body(&b);
        let cmd = parse(CMD_WRITE_PORTAL, &body).unwrap();
        let Command::WritePortal(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed.id, b.id);
        assert_eq!(parsed.data, b.data);
    }

    #[test]
    fn set_focus_round_trips() {
        for target in [
            FocusTarget::Host,
            FocusTarget::Portal("right".into()),
        ] {
            let body = set_focus_body(&target);
            let cmd = parse(CMD_SET_FOCUS, &body).unwrap();
            let Command::SetFocus { target: parsed } = cmd else {
                panic!()
            };
            assert_eq!(parsed, target);
        }
    }

    #[test]
    fn set_cursor_style_round_trips() {
        for style in [CursorStyle::Hidden, CursorStyle::Hollow, CursorStyle::Dim] {
            let body = set_cursor_style_body(style);
            let cmd = parse(CMD_SET_CURSOR_STYLE, &body).unwrap();
            let Command::SetCursorStyle { unfocused } = cmd else {
                panic!()
            };
            assert_eq!(unfocused, style);
        }
    }

    #[test]
    fn set_portal_scrollback_round_trips() {
        for &lines in &[0u32, 1, 100, 5_000, u32::MAX] {
            let body = set_portal_scrollback_body("left", lines);
            let cmd = parse(CMD_SET_PORTAL_SCROLLBACK, &body).unwrap();
            let Command::SetPortalScrollback { id, lines: parsed } = cmd else {
                panic!("wrong variant");
            };
            assert_eq!(id, "left");
            assert_eq!(parsed, lines);
        }
    }

    #[test]
    fn set_portal_scrollback_empty_id_rejected() {
        let body = set_portal_scrollback_body("", 5);
        assert_eq!(
            parse(CMD_SET_PORTAL_SCROLLBACK, &body).unwrap_err(),
            crate::frame::ERR_BAD_PAYLOAD
        );
    }

    #[test]
    fn build_envelope_round_trips_through_apc_stream() {
        use crate::apc::ApcStream;
        use crate::codec::Reader;
        use crate::frame::{MARKER_C2T, PROTOCOL_VERSION};

        let create = Command::CreatePortal(CreatePortalBody {
            id: "left".into(),
            size_w: 80,
            size_h: 24,
            origin_x: 0,
            origin_y: 0,
            anchor_mode: AnchorMode::Live,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 100,
        });
        let probe = Command::Probe;
        let env = build_envelope(&[(probe, 1), (create, 2)]);

        // Pull the unstuffed payload back out and verify two frames.
        let mut s = ApcStream::with_marker(*MARKER_C2T);
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);
        let mut r = Reader::new(&out.payloads[0]);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _payload_len = r.u32().unwrap();
        let mut request_ids = Vec::new();
        while !r.at_end() {
            let _ft = r.u8().unwrap();
            let rid = r.u32().unwrap();
            let body_len = r.u32().unwrap() as usize;
            r.take(body_len).unwrap();
            request_ids.push(rid);
        }
        assert_eq!(request_ids, vec![1, 2]);
    }

    #[test]
    fn update_origin_round_trips() {
        let b = UpdateOriginBody {
            id: "p".into(),
            new_origin_x: -3,
            new_origin_y: 7,
            anchor_mode: AnchorMode::Scrollback,
        };
        let body = update_origin_body(&b);
        let cmd = parse(CMD_UPDATE_ORIGIN, &body).unwrap();
        let Command::UpdateOrigin(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed.id, b.id);
        assert_eq!(parsed.new_origin_x, b.new_origin_x);
        assert_eq!(parsed.new_origin_y, b.new_origin_y);
        assert_eq!(parsed.anchor_mode, b.anchor_mode);
    }
}
