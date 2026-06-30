// Protocol constants from doc/vector-graphics-extension.md.
//
// Some constants are reserved for future phases (image table, gradients,
// reset). Silence the dead-code lint on the whole module so the protocol
// surface stays declared even before its handlers exist.
#![allow(dead_code)]

/// Unstable WIP protocol — version 0. Bumps to 1 when the wire
/// format is declared stable. See `doc/vector-graphics-extension.md`.
pub const PROTOCOL_VERSION: u8 = 0;

// §3 command codes
pub const CMD_PROBE: u8 = 0x01;
pub const CMD_SET_GLOBAL_STYLE: u8 = 0x02;
pub const CMD_CREATE_ELEMENT: u8 = 0x03;
pub const CMD_DELETE_ELEMENT: u8 = 0x04;
pub const CMD_UPDATE_COMMANDS: u8 = 0x05;
pub const CMD_UPDATE_COMMAND: u8 = 0x06;
pub const CMD_UPDATE_TEXT: u8 = 0x07;
pub const CMD_UPDATE_IMAGE: u8 = 0x08;
pub const CMD_UPDATE_ORIGIN: u8 = 0x09;
pub const CMD_UPDATE_VISIBILITY: u8 = 0x0A;
pub const CMD_UPDATE_DRAW_ORDER: u8 = 0x0B;
pub const CMD_UPLOAD_IMAGE: u8 = 0x0C;
pub const CMD_DROP_IMAGE: u8 = 0x0D;
pub const CMD_CLEAR_ALL: u8 = 0x0E;
pub const CMD_UPDATE_SIZE: u8 = 0x0F;
pub const CMD_UPDATE_TRANSFORM: u8 = 0x10;

// §4 response codes
pub const RSP_OK: u8 = 0x01;
pub const RSP_ERR: u8 = 0x02;
pub const RSP_PROBE: u8 = 0x03;
pub const RSP_CHUNK_ACK: u8 = 0x04;

/// Sentinel `request_id` value that asks the host to apply the
/// command but not emit a response frame. Used for "state push"
/// scenarios where the sender is a stateful middleman (e.g. vsd
/// replaying its session snapshot to a freshly attached renderer)
/// and a response would just round-trip through the upstream chain
/// back into the inner program's PTY, where stray bytes get
/// interpreted by whatever is reading there. Clients that need an
/// ack must use any other value (typically a monotonically
/// increasing counter starting at 1).
pub const REQ_ID_NO_RESPONSE: u32 = u32::MAX;

// §4 error codes
pub const ERR_UNKNOWN_COMMAND: u16 = 0x0001;
pub const ERR_BAD_PAYLOAD: u16 = 0x0002;
pub const ERR_UNSUPPORTED_VERSION: u16 = 0x0003;
pub const ERR_UNKNOWN_ELEMENT: u16 = 0x0010;
pub const ERR_DUPLICATE_ID: u16 = 0x0011;
pub const ERR_TOO_MANY_ELEMENTS: u16 = 0x0012;
pub const ERR_COMMAND_INDEX: u16 = 0x0013;
pub const ERR_TEXT_RANGE: u16 = 0x0014;
pub const ERR_UNKNOWN_STYLE: u16 = 0x0020;
pub const ERR_RESERVED_STYLE_ID: u16 = 0x0021;
pub const ERR_UNKNOWN_IMAGE: u16 = 0x0030;
pub const ERR_IMAGE_TOO_LARGE: u16 = 0x0031;
pub const ERR_IMAGE_DECODE: u16 = 0x0032;
pub const ERR_DUPLICATE_IMAGE_ID: u16 = 0x0033;
pub const ERR_TOO_MANY_IMAGES: u16 = 0x0034;
pub const ERR_MAX_NESTING_DEPTH: u16 = 0x0040;
pub const ERR_INTERNAL: u16 = 0x00FF;

// §7.1 draw command opcodes
pub const OP_FILL_POLYGON: u8 = 0x01;
pub const OP_FILL_RECTANGLES: u8 = 0x02;
pub const OP_FILL_PATH: u8 = 0x03;
pub const OP_DRAW_LINES: u8 = 0x04;
pub const OP_DRAW_LINE_LOOP: u8 = 0x05;
pub const OP_DRAW_LINE_STRIP: u8 = 0x06;
pub const OP_DRAW_LINE_PATH: u8 = 0x07;
pub const OP_OUTLINE_FILL_POLYGON: u8 = 0x08;
pub const OP_OUTLINE_FILL_RECTANGLES: u8 = 0x09;
pub const OP_OUTLINE_FILL_PATH: u8 = 0x0A;
pub const OP_DRAW_TEXT: u8 = 0x20;
pub const OP_DRAW_IMAGE: u8 = 0x21;

// §7.3 style kinds
pub const STYLE_FLAT: u8 = 0x01;
pub const STYLE_LINEAR_GRADIENT: u8 = 0x02;
pub const STYLE_RADIAL_GRADIENT: u8 = 0x03;
pub const STYLE_REF: u8 = 0xFF;

// §7.3 color formats
pub const COLOR_RGBA8888: u8 = 0x01;
pub const COLOR_RGB565: u8 = 0x02;

// APC envelope markers
pub const MARKER_C2T: &[u8; 3] = b"VGE";
pub const MARKER_T2C: &[u8; 3] = b"vge";

pub const ESC: u8 = 0x1B;
pub const APC_OPEN: u8 = 0x5F; // '_'
pub const ST_CLOSE: u8 = 0x5C; // '\\'

// Transport-hostile payload bytes that byte-stuffing also neutralises.
// VGE envelopes can be relayed to an inner program through its input
// channel (e.g. a portal's RawReply forwarded into an `ssh` client),
// which interprets some bytes instead of forwarding them: `~` is ssh's
// escape character (`\n~.` tears the session down) and DC1/DC3 are
// software flow control (XON/XOFF). Escaping them keeps the on-wire
// envelope body free of these — and in particular `~` can never follow a
// newline.
pub const TILDE: u8 = 0x7E; // '~'  ssh escape character
pub const XON: u8 = 0x11; // DC1  XON (resume) flow control
pub const XOFF: u8 = 0x13; // DC3  XOFF (pause) flow control

// Second byte of each `ESC <mark>` escape inside an envelope body. ESC
// itself stays `ESC ESC`; the rest map to safe ASCII letters that are
// themselves transport-clean and distinct from `ESC`/`ST_CLOSE`.
pub const ESC_MARK_TILDE: u8 = b'T'; // 0x54 → TILDE
pub const ESC_MARK_XON: u8 = b'Q'; // 0x51 → XON
pub const ESC_MARK_XOFF: u8 = b'S'; // 0x53 → XOFF
