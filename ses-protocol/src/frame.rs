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
/// Ask the host to detach the session (same teardown as veterd's
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

// APC envelope markers.
pub const MARKER_C2H: &[u8; 3] = b"SES"; // client → host (commands)
pub const MARKER_H2C: &[u8; 3] = b"ses"; // host → client (responses)

pub const ESC: u8 = 0x1B;
pub const APC_OPEN: u8 = 0x5F; // '_'
pub const ST_CLOSE: u8 = 0x5C; // '\\'
