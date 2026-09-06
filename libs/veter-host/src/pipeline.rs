//! The host byte pipeline, in one place.
//!
//! A chunk from a PTY runs through a prefix of byte filters and then
//! exactly one segment-aware terminal stage, and that walk happens in
//! two places: the host grid (`veter`'s `App::process_pty_output`,
//! `vsd`'s worker) and every portal
//! (`prt::PrtEngine::cmd_write_portal`). It used to be written out
//! twice, which is how the same ordering bug came to live in both.
//!
//! **VSS goes first, and it splits the chunk.** Every other filter is
//! order-free — each one's APC parser passes the others' markers
//! through verbatim, and a nested envelope inside a `WritePortal`
//! payload is byte-stuffed, so no parser can steal another's bytes.
//! VSS is different in kind: a snapshot doesn't *extract* state, it
//! *replaces* the engines the rest of the pipeline is about to run
//! against. Applied after the whole chunk (which is what draining
//! `take_completed_snapshots` at the end amounts to), a chunk carrying
//! `[text][snapshot][command]` gets both halves wrong — the text lands
//! on the restored screen it was never meant for, and the command is
//! applied to engines that are then thrown away, its response cleared
//! with them. Over SSH the daemon's snapshot and the inner
//! multiplexer's redraw coalesce routinely, so this is what an
//! ordinary attach looks like, not a corner case.
//!
//! So VSS segments the chunk and the rest of the walk runs once per
//! run of bytes, with each restore applied in between.
//!
//! SES is the one stage that is *not* here. It runs ahead of this, in
//! each caller, and `chunk` is what it left behind — because `vsd`
//! forwards that passthrough to its renderer, and needs it as one
//! contiguous piece rather than per segment. Running it up front is
//! safe for the same reason the order of the filters below is free,
//! and it keeps every caller's stage order the same.

use crate::prt::PrtEngine;
use crate::vft::VftEngine;
use crate::vge::VgeEngine;
use crate::vge::state::HitTester;
use crate::vss::{VssEngine, VssSegment};

/// A context's engine state as of the moment before an attach, kept so
/// a `DetachNotify` can put back the view the snapshot replaced (the
/// user's ssh shell, their `vmux` pane). Three binary snapshots in the
/// same format used on the wire, so there is no second in-memory shape
/// just for this.
#[derive(Clone)]
pub struct PreAttachBackup {
    /// The `sequence_id` of the snapshot this was stashed against.
    ///
    /// An attach that dies without sending `DetachNotify` — a dropped
    /// connection — leaves its stash behind. Keeping it and skipping
    /// the stash on the *next* attach means a later detach restores a
    /// screen from two attaches ago, which is not what the pane was
    /// showing when the user typed `vsd attach`; what it was showing
    /// is the dead session's leftovers, and that is what they expect
    /// back. So a snapshot with a different id is a different attach,
    /// and re-stashes.
    pub sequence_id: u32,
    pub vt: Vec<u8>,
    pub vge: Vec<u8>,
    pub prt: Vec<u8>,
}

/// The engines one context owns. Borrowed as separate `&mut`s because
/// at the portal level they are disjoint fields of one `Portal`.
pub struct Engines<'a, CB: vt100::Callbacks> {
    pub vss: &'a mut VssEngine,
    pub prt: &'a mut PrtEngine,
    pub vft: &'a mut VftEngine,
    pub vge: &'a mut VgeEngine,
    pub parser: &'a mut vt100::Parser<CB>,
}

/// What the caller still has to act on after [`drive_chunk`].
#[derive(Default)]
pub struct ChunkWalk {
    /// PRT's side-channel observations, accumulated across every run
    /// of bytes in the chunk and still in stream order. The RIS /
    /// DECSTR reaction VFT needs is already applied; what is left is
    /// each caller's own (a portal counts DSR queries off this).
    pub terminal_events: Vec<prt_protocol::TerminalEvent>,
    /// Snapshots applied during this chunk. Non-zero means the screen
    /// was replaced wholesale part-way through, which invalidates any
    /// before/after sampling the caller took around the call.
    pub restores: usize,
}

/// Run one chunk through the whole pipeline for one context.
///
/// `chunk` must already have been through this context's SES engine —
/// see the module docs. `backup` is the context's pre-attach stash:
/// filled on the first snapshot of an attach and consumed by a
/// `DetachNotify`.
pub fn drive_chunk<CB: vt100::Callbacks>(
    chunk: &[u8],
    e: Engines<'_, CB>,
    backup: &mut Option<PreAttachBackup>,
    hit: Option<&dyn HitTester>,
) -> ChunkWalk {
    let Engines { vss, prt, vft, vge, parser } = e;
    let mut walk = ChunkWalk::default();

    for seg in vss.process_pty_chunk_segments(chunk) {
        match seg {
            VssSegment::Pass(bytes) => {
                let chunk = prt.process_pty_chunk_with_hit(&bytes, hit);
                // §5.6 / §10 — VFT observes no control sequences of
                // its own, so a reset reaches it through PRT's event
                // stream. Every caller does this identically; the
                // rest of the events go back for the caller to read.
                //
                // Coarse on purpose, and this is the portal path's
                // rule, not the host's: the abort fires for a reset
                // anywhere in this run of bytes, before any of the
                // run's VFT frames are processed. So a transfer
                // started *after* a reset in the same run survives
                // (`vsend` right after a `clear`), and one started
                // just before it does not die until the next reset.
                // Aborting precisely would need VFT to be
                // segment-aware the way VGE is; until then this is
                // the direction that loses a live transfer less
                // often.
                for ev in &chunk.terminal_events {
                    if matches!(
                        ev,
                        prt_protocol::TerminalEvent::HardReset
                            | prt_protocol::TerminalEvent::SoftReset
                    ) {
                        vft.abort_all(vft_protocol::frame::ABORT_HOST_RESET, "");
                    }
                }
                walk.terminal_events.extend(chunk.terminal_events);

                let vft_pass = vft.process_pty_chunk(&chunk.passthrough);
                // Drain worker events that arrived synchronously
                // during this run (e.g. a Finalised reply for an
                // EndUpload whose writer had nothing left queued).
                vft.drive();
                crate::vge::drive_terminal_stage(vge, parser, &vft_pass, hit);
            }
            VssSegment::Restore(cs) => {
                walk.restores += 1;
                // First snapshot of an attach: stash what it is about
                // to replace, so a later DetachNotify can put it back.
                // Later snapshots of the *same* attach overwrite
                // without re-stashing; see the `sequence_id` field.
                if backup.as_ref().is_none_or(|b| b.sequence_id != cs.sequence_id) {
                    *backup = Some(PreAttachBackup {
                        sequence_id: cs.sequence_id,
                        vt: parser.screen().binary_snapshot(),
                        vge: vge.binary_snapshot(),
                        prt: prt.binary_snapshot(),
                    });
                }
                // A restore error leaves that engine at its
                // pre-snapshot state, which is the right failure mode
                // — no partial-apply corruption. The `Reject`
                // envelope for a version mismatch was already queued
                // by the VSS engine.
                restore(parser, vge, prt, &cs.vt_bytes, &cs.vge_bytes, &cs.prt_bytes);
            }
            VssSegment::Detach => {
                if let Some(b) = backup.take() {
                    restore(parser, vge, prt, &b.vt, &b.vge, &b.prt);
                }
            }
        }
    }
    walk
}

/// Install one set of engine snapshots, then put the grid back to the
/// size *this* context is being drawn at.
///
/// `restore_from_binary_snapshot` installs the sender's grid geometry
/// along with its contents, and the sender's is only right at the
/// instant the snapshot was taken. Attach, resize the window, detach:
/// the stash was made at the pre-resize size, so restoring it silently
/// puts the receiving portal's vt100 back to a size nothing else in
/// the renderer agrees with, and it stays there until the next resize
/// happens to correct it.
fn restore<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    vge: &mut VgeEngine,
    prt: &mut PrtEngine,
    vt_bytes: &[u8],
    vge_bytes: &[u8],
    prt_bytes: &[u8],
) {
    let (rows, cols) = parser.screen().size();
    let _ = parser.screen_mut().restore_from_binary_snapshot(vt_bytes);
    if parser.screen().size() != (rows, cols) {
        parser.screen_mut().set_size(rows, cols);
    }
    let _ = vge.restore_from_binary_snapshot(vge_bytes);
    let _ = prt.restore_from_binary_snapshot(prt_bytes);
    // The line origin both sub-engines anchor against came in with the
    // vt100 fragment restored just above — and moved again if the
    // resize pushed rows into scrollback.
    vge.sync_top_of_live_screen(parser);
    prt.sync_top_of_live_screen(parser);
}
