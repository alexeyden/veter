// Protocol constants from doc/file-transfer-extension.md.
//
// Several event codes and error codes are reserved for future engine
// wiring. Silence the dead-code lint on the whole module so the protocol
// surface stays declared even before the engine consumes it.
#![allow(dead_code)]

/// Unstable WIP protocol — version 0. Bumps to 1 when the wire
/// format is declared stable. See `doc/file-transfer-extension.md`.
pub const PROTOCOL_VERSION: u8 = 0;

// §3 command codes (client → host)
pub const CMD_PROBE: u8 = 0x01;
pub const CMD_BEGIN_UPLOAD: u8 = 0x02;
pub const CMD_UPLOAD_CHUNK: u8 = 0x03;
pub const CMD_END_UPLOAD: u8 = 0x04;
pub const CMD_BEGIN_DOWNLOAD: u8 = 0x05;
pub const CMD_REPORT_DOWNLOAD_ACK: u8 = 0x06;
pub const CMD_REQUEST_ACK: u8 = 0x07;
pub const CMD_CANCEL_TRANSFER: u8 = 0x08;

// §4.1 response codes (host → client, low half)
pub const RSP_OK: u8 = 0x01;
pub const RSP_ERR: u8 = 0x02;
pub const RSP_PROBE: u8 = 0x03;

// §4.2 event codes (host → client, high half)
pub const EVT_DOWNLOAD_CHUNK: u8 = 0x80;
pub const EVT_DOWNLOAD_END: u8 = 0x81;
pub const EVT_UPLOAD_ACK: u8 = 0x82;
pub const EVT_TRANSFER_ABORTED: u8 = 0x83;

// §4.1 error codes
pub const ERR_UNKNOWN_COMMAND: u16 = 0x0001;
pub const ERR_BAD_PAYLOAD: u16 = 0x0002;
pub const ERR_UNSUPPORTED_VERSION: u16 = 0x0003;
pub const ERR_UNKNOWN_TRANSFER: u16 = 0x0010;
pub const ERR_DUPLICATE_TRANSFER: u16 = 0x0011;
pub const ERR_TOO_MANY_TRANSFERS: u16 = 0x0012;
pub const ERR_UNSUPPORTED_DIR: u16 = 0x0013;
pub const ERR_CHUNK_TOO_LARGE: u16 = 0x0020;
pub const ERR_CHUNK_OFFSET: u16 = 0x0021;
pub const ERR_TOO_MANY_BYTES: u16 = 0x0022;
pub const ERR_PATH_TOO_LONG: u16 = 0x0030;
pub const ERR_PATH_INVALID: u16 = 0x0031;
pub const ERR_PATH_DENIED: u16 = 0x0032;
pub const ERR_PATH_EXISTS: u16 = 0x0033;
pub const ERR_PATH_MISSING: u16 = 0x0034;
pub const ERR_PICKER_UNAVAILABLE: u16 = 0x0040;
pub const ERR_CANCELLED: u16 = 0x0041;
pub const ERR_IO: u16 = 0x0050;
pub const ERR_DISK_FULL: u16 = 0x0051;
pub const ERR_PREMATURE_END: u16 = 0x0052;
pub const ERR_INTERNAL: u16 = 0x00FF;

// §2.1 features bitmask
pub const FEAT_UPLOAD: u8 = 1 << 0;
pub const FEAT_DOWNLOAD: u8 = 1 << 1;

// §6.1 BeginUpload flags
pub const FLAG_OVERWRITE: u8 = 1 << 0;

// §8.3 TransferAborted reasons
pub const ABORT_CLIENT_CANCEL: u8 = 0;
pub const ABORT_HOST_CANCEL: u8 = 1;
pub const ABORT_IO_ERROR: u8 = 2;
pub const ABORT_DISK_FULL: u8 = 3;
pub const ABORT_HOST_RESET: u8 = 4;
pub const ABORT_PATH_REVOKED: u8 = 5;
pub const ABORT_LIMIT_EXCEEDED: u8 = 6;

// APC envelope markers (§1.1).
pub const MARKER_C2H: &[u8; 3] = b"VFT";
pub const MARKER_H2C: &[u8; 3] = b"vft";

pub const ESC: u8 = 0x1B;
pub const APC_OPEN: u8 = 0x5F; // '_'
pub const ST_CLOSE: u8 = 0x5C; // '\\'

// Transport-hostile payload bytes that byte-stuffing also neutralises
// (§1.3). The download path delivers arbitrary file bytes through the
// inner program's *input* channel, which may pass through an interactive
// relay (e.g. an `ssh` client). Such relays interpret some bytes instead
// of forwarding them: `~` is ssh's escape character (`\n~.` tears the
// session down), and DC1/DC3 are software flow control (XON/XOFF). We
// escape these so the on-wire envelope body can never contain them
// literally — and in particular `~` can never follow a newline.
pub const TILDE: u8 = 0x7E; // '~'  ssh escape character
pub const XON: u8 = 0x11; // DC1  XON (resume) flow control
pub const XOFF: u8 = 0x13; // DC3  XOFF (pause) flow control

// Second byte of each `ESC <mark>` escape inside an envelope body. ESC
// itself stays `ESC ESC` (§1.3); the rest map to safe ASCII letters that
// are themselves transport-clean and distinct from `ESC`/`ST_CLOSE`.
pub const ESC_MARK_TILDE: u8 = b'T'; // 0x54 → TILDE
pub const ESC_MARK_XON: u8 = b'Q'; // 0x51 → XON
pub const ESC_MARK_XOFF: u8 = b'S'; // 0x53 → XOFF

// §5.2 transfer ID cap (≤ 64 UTF-8 bytes).
pub const MAX_ID_BYTES: usize = 64;
