// Protocol constants from doc/portal-extension.md.
//
// Several event codes are reserved for future engine wiring (Phase 2+).
// Silence the dead-code lint on the whole module so the protocol surface
// stays declared even before the engine consumes it.
#![allow(dead_code)]

pub const PROTOCOL_VERSION: u8 = 1;

// §3 command codes (client → host)
pub const CMD_PROBE: u8 = 0x01;
pub const CMD_CREATE_PORTAL: u8 = 0x02;
pub const CMD_DELETE_PORTAL: u8 = 0x03;
pub const CMD_UPDATE_SIZE: u8 = 0x04;
pub const CMD_UPDATE_ORIGIN: u8 = 0x05;
pub const CMD_UPDATE_VISIBILITY: u8 = 0x06;
pub const CMD_UPDATE_DRAW_ORDER: u8 = 0x07;
pub const CMD_CLEAR_ALL: u8 = 0x08;
pub const CMD_WRITE_PORTAL: u8 = 0x09;
pub const CMD_SET_FOCUS: u8 = 0x0A;
pub const CMD_SET_CURSOR_STYLE: u8 = 0x0B;
pub const CMD_SET_PORTAL_SCROLLBACK: u8 = 0x0C;

// §4.1 response codes (host → client, low half)
pub const RSP_OK: u8 = 0x01;
pub const RSP_ERR: u8 = 0x02;
pub const RSP_PROBE: u8 = 0x03;

// §4.2 event codes (host → client, high half)
pub const EVT_RAW_REPLY: u8 = 0x80;
pub const EVT_BELL: u8 = 0x81;
pub const EVT_TITLE_CHANGE: u8 = 0x82;
pub const EVT_ICON_NAME_CHANGE: u8 = 0x83;
pub const EVT_WORKING_DIR_CHANGE: u8 = 0x84;
pub const EVT_CLIPBOARD_OP: u8 = 0x85;
pub const EVT_CURSOR_VISIBILITY_CHANGE: u8 = 0x86;
pub const EVT_BUFFER_MODE_CHANGE: u8 = 0x87;
pub const EVT_PORTAL_EVICTED: u8 = 0x88;
pub const EVT_RESIZE_NOTIFY: u8 = 0x89;
pub const EVT_MOUSE_MODE_CHANGE: u8 = 0x8A;

// §4.1 error codes
pub const ERR_UNKNOWN_COMMAND: u16 = 0x0001;
pub const ERR_BAD_PAYLOAD: u16 = 0x0002;
pub const ERR_UNSUPPORTED_VERSION: u16 = 0x0003;
pub const ERR_UNKNOWN_PORTAL: u16 = 0x0010;
pub const ERR_DUPLICATE_ID: u16 = 0x0011;
pub const ERR_TOO_MANY_PORTALS: u16 = 0x0012;
pub const ERR_SIZE_OUT_OF_RANGE: u16 = 0x0013;
pub const ERR_WRITE_TOO_LARGE: u16 = 0x0014;
pub const ERR_MAX_NESTING_DEPTH: u16 = 0x0040;
pub const ERR_INTERNAL: u16 = 0x00FF;

// §2.1 features bitmask
pub const FEAT_ALT_SCREEN_IN_PORTAL: u8 = 1 << 0;
pub const FEAT_EMIT_TITLE_EVENTS: u8 = 1 << 1;
pub const FEAT_EMIT_ICON_EVENTS: u8 = 1 << 2;
pub const FEAT_EMIT_CWD_EVENTS: u8 = 1 << 3;
pub const FEAT_EMIT_CLIPBOARD_EVENTS: u8 = 1 << 4;
pub const FEAT_EMIT_BELL_EVENTS: u8 = 1 << 5;
pub const FEAT_EMIT_MOUSE_MODE_EVENTS: u8 = 1 << 6;

// §10 trailing capability bits (after `max_nesting_depth`).
pub const FEAT_VGE_IN_PORTAL: u8 = 1 << 0;

// §5.2 anchor mode discriminants
pub const ANCHOR_LIVE: u8 = 0;
pub const ANCHOR_SCROLLBACK: u8 = 1;

// §9.1 SetFocus mode discriminants
pub const FOCUS_HOST: u8 = 0;
pub const FOCUS_PORTAL: u8 = 1;

// §9.2 unfocused cursor styles
pub const CURSOR_HIDDEN: u8 = 0;
pub const CURSOR_HOLLOW: u8 = 1;
pub const CURSOR_DIM: u8 = 2;

// §8.7 PortalEvicted reasons
pub const EVICT_SCROLLBACK: u8 = 0;
pub const EVICT_ERASE: u8 = 1;
pub const EVICT_ALT_SWAP: u8 = 2;

// §8.4 ClipboardOp ops
pub const CLIPBOARD_SET: u8 = 0;
pub const CLIPBOARD_QUERY: u8 = 1;

// APC envelope markers (§1.1).
pub const MARKER_C2T: &[u8; 3] = b"PRT";
pub const MARKER_T2C: &[u8; 3] = b"prt";

pub const ESC: u8 = 0x1B;
pub const APC_OPEN: u8 = 0x5F; // '_'
pub const ST_CLOSE: u8 = 0x5C; // '\\'

// §6.8 portal ID cap.
pub const MAX_ID_BYTES: usize = 64;
