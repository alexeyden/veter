// Protocol constants for the SES (Session Extension).
//
// See `doc/session-extension.md` for the protocol semantics. The wire
// format mirrors PRT/VGE/VFT/VSS §1.1–1.4.

/// Unstable WIP protocol — version 0. Bumps to 1 once the wire format
/// is declared stable, in lockstep with the rest of the extensions.
pub const PROTOCOL_VERSION: u8 = 0;

// Client → host command codes (marker `SES`).
/// Ask the host whether it is a session and, if so, its name.
pub const CMD_PROBE: u8 = 0x01;
/// Ask the host to detach the session (same teardown as vsd's
/// `Ctrl+\ d` hotkey). A non-session host replies `RSP_ERR`.
pub const CMD_DETACH: u8 = 0x02;

// Host → client response codes (marker `ses`).
/// Command succeeded; empty body.
pub const RSP_OK: u8 = 0x01;
/// Command failed; body is `u16 code, string msg`.
pub const RSP_ERR: u8 = 0x02;
/// Probe answer; body is
/// `u8 protocol_version, u8 features, u8 in_session, string name`.
pub const RSP_PROBE: u8 = 0x03;

// Wire error codes (carried in an `RSP_ERR` body).
pub const ERR_UNKNOWN_COMMAND: u16 = 0x0001;
pub const ERR_BAD_PAYLOAD: u16 = 0x0002;
/// `CMD_DETACH` sent to a host that is not a session.
pub const ERR_NOT_IN_SESSION: u16 = 0x0010;
pub const ERR_INTERNAL: u16 = 0x00FF;

// Decode error code — internal to this crate; surfaced by `parse`
// when the frame type is not recognised.
pub const ERR_UNKNOWN_FRAME: u16 = 0x0003;

/// Sentinel `request_id` value that asks the host to apply the
/// command but not emit a response frame. The mirror of VGE's
/// `REQ_ID_NO_RESPONSE` (see `doc/vector-graphics-extension.md` §1.2),
/// and needed for the same reason: a sender that is not the pane's
/// foreground program can never read the reply — it lands in the
/// pane's input queue, where whoever the kernel wakes consumes it —
/// and a stateful middleman replaying state would otherwise have its
/// acks round-trip back into the inner program's PTY as keystrokes.
/// Senders that need acknowledgement must use any other value.
pub const REQ_ID_NO_RESPONSE: u32 = u32::MAX;

// APC envelope markers.
pub const MARKER_C2H: &[u8; 3] = b"SES"; // client → host (commands)
pub const MARKER_H2C: &[u8; 3] = b"ses"; // host → client (responses)

pub const ESC: u8 = 0x1B;
pub const APC_OPEN: u8 = 0x5F; // '_'
pub const ST_CLOSE: u8 = 0x5C; // '\\'

// Transport-hostile payload bytes that byte-stuffing also neutralises.
// A SES envelope can be relayed to an inner program through its input
// channel — e.g. a portal's RawReply forwarded into a pane that is an
// `ssh` client, where it becomes session input subject to escape
// processing. Such relays interpret some bytes instead of forwarding
// them: `~` is ssh's escape character (`\n~.` tears the session down)
// and DC1/DC3 are software flow control (XON/XOFF). Escaping them keeps
// the on-wire envelope body free of these — and in particular `~` can
// never follow a newline.
pub const TILDE: u8 = 0x7E; // '~'  ssh escape character
pub const XON: u8 = 0x11; // DC1  XON (resume) flow control
pub const XOFF: u8 = 0x13; // DC3  XOFF (pause) flow control

// Second byte of each `ESC <mark>` escape inside an envelope body. ESC
// itself stays `ESC ESC`; the rest map to safe ASCII letters that are
// themselves transport-clean and distinct from `ESC`/`ST_CLOSE`.
pub const ESC_MARK_TILDE: u8 = b'T'; // 0x54 → TILDE
pub const ESC_MARK_XON: u8 = b'Q'; // 0x51 → XON
pub const ESC_MARK_XOFF: u8 = b'S'; // 0x53 → XOFF

// Output post-processing rewrites three more bytes, so byte-stuffing
// has to neutralise them as well. A pane's termios normally has
// `OPOST` on, and with it `ONLCR` (LF -> CRLF) by default; `OCRNL`,
// `ONLRET` and `ONOCR` rewrite CR, and `TABDLY=XTABS` expands TAB into
// spaces. Any of those silently corrupts an envelope written to a
// cooked tty -- the case an out-of-band client hits, since it cannot
// change the termios of a pane it does not own.
pub const TAB: u8 = 0x09; // HT   expanded by TABDLY=XTABS
pub const LF: u8 = 0x0A; // NL   rewritten by ONLCR
pub const CR: u8 = 0x0D; // CR   rewritten by OCRNL / ONLRET / ONOCR

pub const ESC_MARK_TAB: u8 = b'H'; // 0x48 -> TAB
pub const ESC_MARK_LF: u8 = b'N'; // 0x4E -> LF
pub const ESC_MARK_CR: u8 = b'R'; // 0x52 -> CR
