// Typed command parsing for PRT frames (§3, §6, §7, §9).

use super::codec::{DecodeError, DecodeResult, Reader};
use super::frame::*;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum AnchorMode {
    Live,
    Scrollback,
}

impl AnchorMode {
    fn from_u8(v: u8) -> DecodeResult<Self> {
        match v {
            ANCHOR_LIVE => Ok(AnchorMode::Live),
            ANCHOR_SCROLLBACK => Ok(AnchorMode::Scrollback),
            _ => Err(DecodeError::bad_payload()),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            AnchorMode::Live => ANCHOR_LIVE,
            AnchorMode::Scrollback => ANCHOR_SCROLLBACK,
        }
    }
}

/// §9.3 — where `SetPortalScrollback` moves a view.
///
/// Every form is an *intent* the host resolves against the buffer as it
/// stands when the command is processed, never a position the client
/// computed from what it last heard: the host moves a scrolled view on
/// its own (it stays pinned to its text as output arrives, and is
/// clamped when that text is evicted), so any number a client holds is
/// stale by the time it arrives.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum ScrollTarget {
    /// Follow the live region.
    Live,
    /// Move by this many rows; positive is back into history. Reaching
    /// the live region returns the view to it.
    Delta(i32),
    /// Put this absolute line (the buffer's `top_of_live_screen`
    /// numbering) at the top of the view, clamped to the lines still
    /// held. A line at or below the top of the live screen is live;
    /// `i64::MIN` is the oldest line.
    Line(i64),
}

impl ScrollTarget {
    pub fn read(r: &mut Reader<'_>) -> DecodeResult<Self> {
        match r.u8()? {
            SCROLL_LIVE => Ok(ScrollTarget::Live),
            SCROLL_DELTA => Ok(ScrollTarget::Delta(r.i32()?)),
            SCROLL_LINE => Ok(ScrollTarget::Line(r.i64()?)),
            _ => Err(DecodeError::bad_payload()),
        }
    }

    pub fn write(&self, w: &mut super::codec::Writer) {
        match *self {
            ScrollTarget::Live => w.u8(SCROLL_LIVE),
            ScrollTarget::Delta(n) => {
                w.u8(SCROLL_DELTA);
                w.i32(n);
            }
            ScrollTarget::Line(l) => {
                w.u8(SCROLL_LINE);
                w.i64(l);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum FocusTarget {
    Host,
    Portal(String),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum CursorStyle {
    Hidden,
    Hollow,
    Dim,
}

impl CursorStyle {
    fn from_u8(v: u8) -> DecodeResult<Self> {
        match v {
            CURSOR_HIDDEN => Ok(CursorStyle::Hidden),
            CURSOR_HOLLOW => Ok(CursorStyle::Hollow),
            CURSOR_DIM => Ok(CursorStyle::Dim),
            _ => Err(DecodeError::bad_payload()),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            CursorStyle::Hidden => CURSOR_HIDDEN,
            CursorStyle::Hollow => CURSOR_HOLLOW,
            CursorStyle::Dim => CURSOR_DIM,
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CreatePortalBody {
    pub id: String,
    pub size_w: u32,
    pub size_h: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub anchor_mode: AnchorMode,
    pub is_visible: bool,
    pub draw_order: i32,
    /// Reserved; spec mandates it be `0`. Decoder enforces this.
    pub flags: u8,
    pub scrollback_lines: u32,
}

/// §6.9 — add a second view onto an existing portal's buffer.
///
/// The new view shows the same cells as `src_id` and shares its
/// scrollback, sub-portals and every per-portal engine; it has its own
/// position, draw order, visibility and scroll offset. Writing to
/// either view reaches the one buffer, so both are equal: there is no
/// original and no copy, and deleting either leaves the other working.
///
/// No `size_w` / `size_h` / `scrollback_lines`: there is one grid, so
/// the new view necessarily inherits its dimensions and history.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ForkPortalBody {
    /// Existing view whose buffer the new view will show.
    pub src_id: String,
    /// Id for the new view; must be unused in this scope.
    pub new_id: String,
    pub origin_x: i32,
    pub origin_y: i32,
    pub anchor_mode: AnchorMode,
    pub is_visible: bool,
    pub draw_order: i32,
    /// Reserved; spec mandates it be `0`. Decoder enforces this.
    pub flags: u8,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UpdateOriginBody {
    pub id: String,
    pub new_origin_x: i32,
    pub new_origin_y: i32,
    /// Echoed by the client; engine rejects mismatch with portal's
    /// current mode (§6.4).
    pub anchor_mode: AnchorMode,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct WritePortalBody {
    pub id: String,
    pub data: Vec<u8>,
}

/// §9.4 — how loud a status item is; the client picks the colours.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum StatusLevel {
    Info,
    Warn,
    Error,
}

/// §9.4 — the progress figure a status item carries, if any.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum StatusProgress {
    /// 0..=100.
    Percent(u8),
    Indeterminate,
}

/// §9.4 — one change to the sender's status items. `SetStatus` carries
/// it from a program to the engine inside its portal, and
/// `StatusChange` (§8.14) relays it, unchanged, to the portal's client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum StatusOp {
    /// Set (or replace) the item under `key`.
    Set {
        key: String,
        level: StatusLevel,
        progress: Option<StatusProgress>,
        text: String,
    },
    /// Drop the item under `key` — or, with `prefix`, every item whose
    /// key starts with it; an empty prefix drops them all.
    Remove { key: String, prefix: bool },
}

/// One line of chrome: no C0 controls, no DEL (§9.4).
fn status_str<'a>(r: &mut Reader<'a>, max: usize) -> DecodeResult<&'a str> {
    let s = r.string()?;
    if s.len() > max || s.chars().any(|c| c < ' ' || c == '\u{7f}') {
        return Err(DecodeError::bad_payload());
    }
    Ok(s)
}

impl StatusOp {
    pub fn key(&self) -> &str {
        match self {
            StatusOp::Set { key, .. } | StatusOp::Remove { key, .. } => key,
        }
    }

    pub fn read(r: &mut Reader<'_>) -> DecodeResult<Self> {
        let flags = r.u8()?;
        if flags & !(STATUS_FLAG_REMOVE | STATUS_FLAG_KEY_IS_PREFIX) != 0 {
            return Err(DecodeError::bad_payload());
        }
        let key = status_str(r, MAX_STATUS_KEY_BYTES)?.to_owned();
        let prefix = flags & STATUS_FLAG_KEY_IS_PREFIX != 0;
        if flags & STATUS_FLAG_REMOVE != 0 {
            return Ok(StatusOp::Remove { key, prefix });
        }
        if prefix {
            return Err(DecodeError::bad_payload());
        }
        let level = match r.u8()? {
            STATUS_LEVEL_INFO => StatusLevel::Info,
            STATUS_LEVEL_WARN => StatusLevel::Warn,
            STATUS_LEVEL_ERROR => StatusLevel::Error,
            _ => return Err(DecodeError::bad_payload()),
        };
        let kind = r.u8()?;
        let percent = r.u8()?;
        let progress = match kind {
            STATUS_PROGRESS_NONE if percent == 0 => None,
            STATUS_PROGRESS_PERCENT if percent <= 100 => Some(StatusProgress::Percent(percent)),
            STATUS_PROGRESS_INDETERMINATE if percent == 0 => Some(StatusProgress::Indeterminate),
            _ => return Err(DecodeError::bad_payload()),
        };
        let text = status_str(r, MAX_STATUS_TEXT_BYTES)?.to_owned();
        Ok(StatusOp::Set { key, level, progress, text })
    }

    pub fn write(&self, w: &mut super::codec::Writer) {
        match self {
            StatusOp::Remove { key, prefix } => {
                let prefix = if *prefix { STATUS_FLAG_KEY_IS_PREFIX } else { 0 };
                w.u8(STATUS_FLAG_REMOVE | prefix);
                w.str(key);
            }
            StatusOp::Set { key, level, progress, text } => {
                w.u8(0);
                w.str(key);
                w.u8(match level {
                    StatusLevel::Info => STATUS_LEVEL_INFO,
                    StatusLevel::Warn => STATUS_LEVEL_WARN,
                    StatusLevel::Error => STATUS_LEVEL_ERROR,
                });
                let (kind, percent) = match progress {
                    None => (STATUS_PROGRESS_NONE, 0),
                    Some(StatusProgress::Percent(p)) => (STATUS_PROGRESS_PERCENT, *p),
                    Some(StatusProgress::Indeterminate) => (STATUS_PROGRESS_INDETERMINATE, 0),
                };
                w.u8(kind);
                w.u8(percent);
                w.str(text);
            }
        }
    }
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum Command {
    Probe,
    CreatePortal(CreatePortalBody),
    DeletePortal { id: String },
    UpdateSize { id: String, new_w: u32, new_h: u32 },
    UpdateOrigin(UpdateOriginBody),
    UpdateVisibility { id: String, is_visible: bool },
    UpdateDrawOrder { id: String, draw_order: i32 },
    ClearAll,
    WritePortal(WritePortalBody),
    SetFocus { target: FocusTarget },
    SetCursorStyle { unfocused: CursorStyle },
    /// §9.3 — move the named view through its buffer's scrollback.
    SetPortalScrollback { id: String, to: ScrollTarget },
    /// §6.9 — a second view onto an existing portal's buffer.
    ForkPortal(ForkPortalBody),
    /// §9.4 — set or remove one of the sender's own status items.
    SetStatus(StatusOp),
}

fn read_id(r: &mut Reader<'_>) -> DecodeResult<String> {
    let s = r.string()?;
    // §6.8 — IDs are non-empty in every command that references one.
    if s.is_empty() {
        return Err(DecodeError::bad_payload());
    }
    if s.len() > MAX_ID_BYTES {
        return Err(DecodeError::bad_payload());
    }
    Ok(s.to_owned())
}

/// Parse one frame body given its `frame_type`. Returns the decoded
/// command on success, or an `error_code` (one of the `ERR_*` constants
/// in §4.1) on failure.
///
/// The caller is responsible for surfacing `request_id` to the client
/// in the matching error response — we don't see it here.
pub fn parse(frame_type: u8, body: &[u8]) -> Result<Command, u16> {
    let mut r = Reader::new(body);
    match frame_type {
        CMD_PROBE => {
            // Body is empty (§2.1). Strict on trailing bytes.
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::Probe)
        }
        CMD_CREATE_PORTAL => {
            let id = read_id(&mut r)?;
            let size_w = r.u32()?;
            let size_h = r.u32()?;
            let origin_x = r.i32()?;
            let origin_y = r.i32()?;
            let anchor_mode = AnchorMode::from_u8(r.u8()?)?;
            let is_visible = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ERR_BAD_PAYLOAD),
            };
            let draw_order = r.i32()?;
            let flags = r.u8()?;
            // §6.1 — `flags` is reserved; non-zero is rejected.
            if flags != 0 {
                return Err(ERR_BAD_PAYLOAD);
            }
            let scrollback_lines = r.u32()?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            // Size = 0 is `err_size_out_of_range` per §6.1, but the spec
            // also lists it as engine-side; we surface bad_payload only
            // for hard structural problems and let the engine raise
            // size_out_of_range. So we accept zero here and let the
            // engine validate.
            Ok(Command::CreatePortal(CreatePortalBody {
                id,
                size_w,
                size_h,
                origin_x,
                origin_y,
                anchor_mode,
                is_visible,
                draw_order,
                flags,
                scrollback_lines,
            }))
        }
        CMD_FORK_PORTAL => {
            let src_id = read_id(&mut r)?;
            let new_id = read_id(&mut r)?;
            let origin_x = r.i32()?;
            let origin_y = r.i32()?;
            let anchor_mode = AnchorMode::from_u8(r.u8()?)?;
            let is_visible = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ERR_BAD_PAYLOAD),
            };
            let draw_order = r.i32()?;
            let flags = r.u8()?;
            // §6.9 — `flags` is reserved; non-zero is rejected.
            if flags != 0 {
                return Err(ERR_BAD_PAYLOAD);
            }
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::ForkPortal(ForkPortalBody {
                src_id,
                new_id,
                origin_x,
                origin_y,
                anchor_mode,
                is_visible,
                draw_order,
                flags,
            }))
        }
        CMD_DELETE_PORTAL => {
            let id = read_id(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::DeletePortal { id })
        }
        CMD_UPDATE_SIZE => {
            let id = read_id(&mut r)?;
            let new_w = r.u32()?;
            let new_h = r.u32()?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateSize { id, new_w, new_h })
        }
        CMD_UPDATE_ORIGIN => {
            let id = read_id(&mut r)?;
            let new_origin_x = r.i32()?;
            let new_origin_y = r.i32()?;
            let anchor_mode = AnchorMode::from_u8(r.u8()?)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateOrigin(UpdateOriginBody {
                id,
                new_origin_x,
                new_origin_y,
                anchor_mode,
            }))
        }
        CMD_UPDATE_VISIBILITY => {
            let id = read_id(&mut r)?;
            let is_visible = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ERR_BAD_PAYLOAD),
            };
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateVisibility { id, is_visible })
        }
        CMD_UPDATE_DRAW_ORDER => {
            let id = read_id(&mut r)?;
            let draw_order = r.i32()?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateDrawOrder { id, draw_order })
        }
        CMD_CLEAR_ALL => {
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::ClearAll)
        }
        CMD_WRITE_PORTAL => {
            let id = read_id(&mut r)?;
            let data = r.bytes()?.to_vec();
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::WritePortal(WritePortalBody { id, data }))
        }
        CMD_SET_FOCUS => {
            // §9.1 — `mode` byte; `id` field is present iff mode == 1.
            let mode = r.u8()?;
            let target = match mode {
                FOCUS_HOST => {
                    if !r.at_end() {
                        return Err(ERR_BAD_PAYLOAD);
                    }
                    FocusTarget::Host
                }
                FOCUS_PORTAL => {
                    let id = read_id(&mut r)?;
                    if !r.at_end() {
                        return Err(ERR_BAD_PAYLOAD);
                    }
                    FocusTarget::Portal(id)
                }
                _ => return Err(ERR_BAD_PAYLOAD),
            };
            Ok(Command::SetFocus { target })
        }
        CMD_SET_CURSOR_STYLE => {
            let unfocused = CursorStyle::from_u8(r.u8()?)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::SetCursorStyle { unfocused })
        }
        CMD_SET_PORTAL_SCROLLBACK => {
            let id = read_id(&mut r)?;
            let to = ScrollTarget::read(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::SetPortalScrollback { id, to })
        }
        CMD_SET_STATUS => {
            let op = StatusOp::read(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::SetStatus(op))
        }
        // §3 — frame types in 0x80..=0xFF are events and MUST NOT appear
        // in client-to-host envelopes. Treat as unknown command.
        _ => Err(ERR_UNKNOWN_COMMAND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Writer;

    fn parse_one(frame_type: u8, w: Writer) -> Result<Command, u16> {
        parse(frame_type, &w.buf)
    }

    #[test]
    fn probe_empty_body() {
        let cmd = parse(CMD_PROBE, &[]).unwrap();
        assert!(matches!(cmd, Command::Probe));
    }

    #[test]
    fn probe_rejects_trailing_bytes() {
        assert_eq!(parse(CMD_PROBE, &[0x00]).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn create_portal_roundtrip() {
        let mut w = Writer::new();
        w.str("left");
        w.u32(80);
        w.u32(24);
        w.i32(0);
        w.i32(5);
        w.u8(ANCHOR_LIVE);
        w.u8(1);
        w.i32(10);
        w.u8(0);
        w.u32(1000);

        let cmd = parse_one(CMD_CREATE_PORTAL, w).unwrap();
        let Command::CreatePortal(body) = cmd else {
            panic!("expected CreatePortal");
        };
        assert_eq!(body.id, "left");
        assert_eq!(body.size_w, 80);
        assert_eq!(body.size_h, 24);
        assert_eq!(body.origin_x, 0);
        assert_eq!(body.origin_y, 5);
        assert_eq!(body.anchor_mode, AnchorMode::Live);
        assert!(body.is_visible);
        assert_eq!(body.draw_order, 10);
        assert_eq!(body.flags, 0);
        assert_eq!(body.scrollback_lines, 1000);
    }

    #[test]
    fn create_portal_rejects_nonzero_flags() {
        let mut w = Writer::new();
        w.str("p");
        w.u32(1);
        w.u32(1);
        w.i32(0);
        w.i32(0);
        w.u8(ANCHOR_LIVE);
        w.u8(0);
        w.i32(0);
        w.u8(0x01); // reserved must be 0
        w.u32(0);
        assert_eq!(parse_one(CMD_CREATE_PORTAL, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn create_portal_rejects_invalid_anchor_mode() {
        let mut w = Writer::new();
        w.str("p");
        w.u32(1);
        w.u32(1);
        w.i32(0);
        w.i32(0);
        w.u8(0xFF);
        w.u8(0);
        w.i32(0);
        w.u8(0);
        w.u32(0);
        assert_eq!(parse_one(CMD_CREATE_PORTAL, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn create_portal_rejects_invalid_visibility_byte() {
        // is_visible must be exactly 0 or 1.
        let mut w = Writer::new();
        w.str("p");
        w.u32(1);
        w.u32(1);
        w.i32(0);
        w.i32(0);
        w.u8(ANCHOR_LIVE);
        w.u8(0x02);
        w.i32(0);
        w.u8(0);
        w.u32(0);
        assert_eq!(parse_one(CMD_CREATE_PORTAL, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn empty_id_is_bad_payload() {
        let mut w = Writer::new();
        w.str("");
        assert_eq!(parse_one(CMD_DELETE_PORTAL, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn id_too_long_is_bad_payload() {
        let mut w = Writer::new();
        w.str(&"x".repeat(MAX_ID_BYTES + 1));
        assert_eq!(parse_one(CMD_DELETE_PORTAL, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn write_portal_roundtrip() {
        let mut w = Writer::new();
        w.str("left");
        w.bytes(&[0x1B, b'[', b'H']);
        let cmd = parse_one(CMD_WRITE_PORTAL, w).unwrap();
        let Command::WritePortal(body) = cmd else {
            panic!();
        };
        assert_eq!(body.id, "left");
        assert_eq!(body.data, vec![0x1B, b'[', b'H']);
    }

    #[test]
    fn set_focus_host_no_id() {
        let mut w = Writer::new();
        w.u8(FOCUS_HOST);
        let cmd = parse_one(CMD_SET_FOCUS, w).unwrap();
        let Command::SetFocus { target } = cmd else {
            panic!();
        };
        assert_eq!(target, FocusTarget::Host);
    }

    #[test]
    fn set_focus_portal_with_id() {
        let mut w = Writer::new();
        w.u8(FOCUS_PORTAL);
        w.str("right");
        let cmd = parse_one(CMD_SET_FOCUS, w).unwrap();
        let Command::SetFocus { target } = cmd else {
            panic!();
        };
        assert_eq!(target, FocusTarget::Portal("right".into()));
    }

    #[test]
    fn set_focus_host_with_trailing_id_is_bad() {
        // mode==0 ⇒ id MUST NOT be present.
        let mut w = Writer::new();
        w.u8(FOCUS_HOST);
        w.str("right");
        assert_eq!(parse_one(CMD_SET_FOCUS, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn set_focus_portal_missing_id_is_bad() {
        let mut w = Writer::new();
        w.u8(FOCUS_PORTAL);
        // truncated — id missing
        assert_eq!(parse_one(CMD_SET_FOCUS, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn set_cursor_style_roundtrip() {
        let mut w = Writer::new();
        w.u8(CURSOR_DIM);
        let cmd = parse_one(CMD_SET_CURSOR_STYLE, w).unwrap();
        let Command::SetCursorStyle { unfocused } = cmd else {
            panic!();
        };
        assert_eq!(unfocused, CursorStyle::Dim);
    }

    #[test]
    fn unknown_frame_type_is_unknown_command() {
        // Reserved client-to-host range.
        assert_eq!(parse(0x7F, &[]).unwrap_err(), ERR_UNKNOWN_COMMAND);
        // Event range — never legal client→host (§3).
        assert_eq!(parse(0x80, &[]).unwrap_err(), ERR_UNKNOWN_COMMAND);
        assert_eq!(parse(0xFF, &[]).unwrap_err(), ERR_UNKNOWN_COMMAND);
    }

    #[test]
    fn update_origin_roundtrip() {
        let mut w = Writer::new();
        w.str("p");
        w.i32(-3);
        w.i32(7);
        w.u8(ANCHOR_SCROLLBACK);
        let cmd = parse_one(CMD_UPDATE_ORIGIN, w).unwrap();
        let Command::UpdateOrigin(body) = cmd else {
            panic!();
        };
        assert_eq!(body.id, "p");
        assert_eq!(body.new_origin_x, -3);
        assert_eq!(body.new_origin_y, 7);
        assert_eq!(body.anchor_mode, AnchorMode::Scrollback);
    }

    fn status_roundtrip(op: StatusOp) {
        let body = crate::encode::set_status_body(&op);
        let Command::SetStatus(back) = parse(CMD_SET_STATUS, &body).unwrap() else {
            panic!("expected SetStatus");
        };
        assert_eq!(back, op);
    }

    #[test]
    fn set_status_roundtrips_every_shape() {
        status_roundtrip(StatusOp::Set {
            key: "dl".into(),
            level: StatusLevel::Warn,
            progress: Some(StatusProgress::Percent(42)),
            text: "ubuntu.iso — 1.2 GB".into(),
        });
        status_roundtrip(StatusOp::Set {
            key: String::new(),
            level: StatusLevel::Info,
            progress: Some(StatusProgress::Indeterminate),
            text: String::new(),
        });
        status_roundtrip(StatusOp::Set {
            key: "claude.state".into(),
            level: StatusLevel::Error,
            progress: None,
            text: "failed".into(),
        });
        status_roundtrip(StatusOp::Remove { key: "claude.".into(), prefix: true });
        status_roundtrip(StatusOp::Remove { key: "dl".into(), prefix: false });
    }

    #[test]
    fn set_status_rejects_what_section_9_4_forbids() {
        let set = |flags: u8, key: &str, level: u8, kind: u8, pct: u8, text: &str| {
            let mut w = Writer::new();
            w.u8(flags);
            w.str(key);
            w.u8(level);
            w.u8(kind);
            w.u8(pct);
            w.str(text);
            parse(CMD_SET_STATUS, &w.buf)
        };
        assert!(set(0, "k", 0, 1, 100, "ok").is_ok());
        // Reserved flag bit; prefix without remove.
        assert_eq!(set(0x04, "k", 0, 0, 0, "").unwrap_err(), ERR_BAD_PAYLOAD);
        assert_eq!(set(STATUS_FLAG_KEY_IS_PREFIX, "k", 0, 0, 0, "").unwrap_err(), ERR_BAD_PAYLOAD);
        // Unknown level / progress kind; percent out of range or stray.
        assert_eq!(set(0, "k", 3, 0, 0, "").unwrap_err(), ERR_BAD_PAYLOAD);
        assert_eq!(set(0, "k", 0, 3, 0, "").unwrap_err(), ERR_BAD_PAYLOAD);
        assert_eq!(set(0, "k", 0, 1, 101, "").unwrap_err(), ERR_BAD_PAYLOAD);
        assert_eq!(set(0, "k", 0, 0, 5, "").unwrap_err(), ERR_BAD_PAYLOAD);
        // Controls and oversize strings.
        assert_eq!(set(0, "k", 0, 0, 0, "a\nb").unwrap_err(), ERR_BAD_PAYLOAD);
        assert_eq!(set(0, "k", 0, 0, 0, "a\x1b[2Jb").unwrap_err(), ERR_BAD_PAYLOAD);
        assert_eq!(set(0, "k\x7f", 0, 0, 0, "").unwrap_err(), ERR_BAD_PAYLOAD);
        let long_key = "k".repeat(MAX_STATUS_KEY_BYTES + 1);
        assert_eq!(set(0, &long_key, 0, 0, 0, "").unwrap_err(), ERR_BAD_PAYLOAD);
        let long_text = "t".repeat(MAX_STATUS_TEXT_BYTES + 1);
        assert_eq!(set(0, "k", 0, 0, 0, &long_text).unwrap_err(), ERR_BAD_PAYLOAD);
        // Trailing bytes after a remove.
        let mut w = Writer::new();
        w.u8(STATUS_FLAG_REMOVE);
        w.str("k");
        w.u8(0);
        assert_eq!(parse(CMD_SET_STATUS, &w.buf).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn clear_all_roundtrip() {
        assert!(matches!(parse(CMD_CLEAR_ALL, &[]).unwrap(), Command::ClearAll));
        assert_eq!(parse(CMD_CLEAR_ALL, &[0x00]).unwrap_err(), ERR_BAD_PAYLOAD);
    }
}
