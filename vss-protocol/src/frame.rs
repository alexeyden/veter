// Protocol constants for the VSS (Veter State Snapshot) extension.
//
// See `doc/session-manager.md` §4 for the protocol semantics. The wire
// format mirrors PRT/VGE/VFT §1.1–1.4.
//
// Several frame codes and reject reasons are reserved for future
// engine wiring. The dead-code lint is silenced here so the protocol
// surface stays declared even before the engine consumes every code.
#![allow(dead_code)]

/// Unstable WIP protocol — version 0. Bumps to 1 once the wire
/// format is declared stable, in lockstep with the rest of the
/// extensions.
pub const PROTOCOL_VERSION: u8 = 0;

/// Monotonic engine-state snapshot version. Both `vsd` and
/// `veter` bake this constant into their binaries; the renderer
/// rejects any incoming snapshot whose `SnapshotBegin.snapshot_version`
/// differs. Bump on every breaking change to *any* of the three
/// sub-snapshot layouts (vt100 / VGE / PRT). See
/// `doc/session-manager.md` §4.2.
///
/// History:
/// - v3: added `DetachNotify` downstream frame so the renderer can
///   restore its pre-attach state when an attach ends.
/// - v2: VGE and PRT sub-snapshots gained `top_of_live_screen` so
///   anchor-line semantics survive across attach.
/// - v1: initial layout.
pub const SNAPSHOT_VERSION: u32 = 3;

// Engine → renderer frame codes (marker `VSS`).
pub const FRM_SNAPSHOT_BEGIN: u8 = 0x01;
pub const FRM_VT_FRAGMENT: u8 = 0x02;
pub const FRM_VGE_FRAGMENT: u8 = 0x03;
pub const FRM_PRT_FRAGMENT: u8 = 0x04;
pub const FRM_SNAPSHOT_END: u8 = 0x05;
/// Tell the renderer that the attach is ending. Restore the
/// pre-attach engine state that was saved on the first
/// `SnapshotBegin` of this attach window. Body is empty.
pub const FRM_DETACH_NOTIFY: u8 = 0x06;

// Renderer → engine frame codes (marker `vss`).
pub const FRM_SNAPSHOT_ACCEPTED: u8 = 0x01;
pub const FRM_SNAPSHOT_REJECTED: u8 = 0x02;

// SnapshotRejected reasons.
pub const REJECT_VERSION_MISMATCH: u8 = 0x01;
pub const REJECT_MALFORMED: u8 = 0x02;
pub const REJECT_CAPACITY: u8 = 0x03;

// Decode error codes — internal to this crate; not on the wire.
pub const ERR_BAD_PAYLOAD: u16 = 0x0001;
pub const ERR_UNKNOWN_FRAME: u16 = 0x0002;

// APC envelope markers (§4.1).
pub const MARKER_E2R: &[u8; 3] = b"VSS"; // engine → renderer
pub const MARKER_R2E: &[u8; 3] = b"vss"; // renderer → engine

pub const ESC: u8 = 0x1B;
pub const APC_OPEN: u8 = 0x5F; // '_'
pub const ST_CLOSE: u8 = 0x5C; // '\\'

// Transport-hostile payload bytes that byte-stuffing also neutralises.
// A VSS envelope can be relayed to an inner program through its input
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

/// Default ceiling on a single fragment's payload, in bytes. vsd
/// chunks `Vt/Vge/Prt` snapshots at this granularity before wrapping
/// them in envelopes. 16 KiB stays well under any plausible per-APC
/// budget while keeping the frame count modest for multi-megabyte
/// snapshots dominated by images.
pub const DEFAULT_MAX_FRAGMENT_BYTES: usize = 16 * 1024;
