//! Ordering of a VSS snapshot against the bytes around it in the same
//! chunk (BUGS.md §1.1).
//!
//! A snapshot replaces the receiving context's engines wholesale, so
//! its position in the stream decides what belongs to which set of
//! engines. Applying it after the whole chunk — which is what draining
//! `take_completed_snapshots()` at the end amounts to — gets both
//! halves wrong. Over SSH the daemon's snapshot and the inner
//! multiplexer's SIGWINCH redraw coalesce routinely, so a chunk
//! shaped `[text][snapshot][command]` is what an ordinary attach looks
//! like.

use veter_host::pipeline::{drive_chunk, Engines, PreAttachBackup};
use veter_host::prt::PrtEngine;
use veter_host::ses::SesEngine;
use veter_host::vft::VftEngine;
use veter_host::vge::VgeEngine;
use veter_host::vss::VssEngine;

const CELL: (u16, u16) = (8, 16);

struct Host {
    vss: VssEngine,
    prt: PrtEngine,
    vft: VftEngine,
    ses: SesEngine,
    vge: VgeEngine,
    parser: vt100::Parser,
    backup: Option<PreAttachBackup>,
}

impl Host {
    fn new() -> Self {
        Self {
            vss: VssEngine::new(),
            prt: PrtEngine::new(),
            vft: VftEngine::new(|| {}),
            ses: SesEngine::new(),
            vge: VgeEngine::new(CELL, 1.0),
            parser: vt100::Parser::new(24, 80, 100),
            backup: None,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        drive_chunk(
            chunk,
            Engines {
                vss: &mut self.vss,
                prt: &mut self.prt,
                vft: &mut self.vft,
                ses: &mut self.ses,
                vge: &mut self.vge,
                parser: &mut self.parser,
            },
            &mut self.backup,
            None,
        );
        let _ = self.prt.take_responses();
        let _ = self.vge.take_responses();
        let _ = self.vss.take_responses();
    }

    /// The pipeline as it was before this fix: every stage a
    /// whole-chunk byte filter, with completed snapshots drained and
    /// applied at the end. Kept so the tests below pin the bug and not
    /// just the fix — an ordering test that passes either way tests
    /// nothing.
    fn feed_unordered(&mut self, chunk: &[u8]) {
        let prt_chunk = self.prt.process_pty_chunk_with_hit(chunk, None);
        let vft_pass = self.vft.process_pty_chunk(&prt_chunk.passthrough);
        let vss_pass = self.vss.process_pty_chunk(&vft_pass);
        let ses_pass = self.ses.process_pty_chunk(&vss_pass);
        veter_host::vge::drive_terminal_stage(
            &mut self.vge,
            &mut self.parser,
            &ses_pass,
            None,
        );
        for cs in self.vss.take_completed_snapshots() {
            let _ = self
                .parser
                .screen_mut()
                .restore_from_binary_snapshot(&cs.vt_bytes);
            let _ = self.vge.restore_from_binary_snapshot(&cs.vge_bytes);
            let _ = self.prt.restore_from_binary_snapshot(&cs.prt_bytes);
        }
        let _ = self.prt.take_responses();
        let _ = self.vge.take_responses();
        let _ = self.vss.take_responses();
    }

    fn screen(&self) -> String {
        self.parser.screen().contents().trim_end().to_string()
    }
}

/// A snapshot envelope carrying the state of a host that has `text`
/// on its screen and nothing else.
fn snapshot_envelope(text: &[u8]) -> Vec<u8> {
    snapshot_of(24, 80, 7, text)
}

fn snapshot_of(rows: u16, cols: u16, sequence_id: u32, text: &[u8]) -> Vec<u8> {
    let mut parser = vt100::Parser::new(rows, cols, 100);
    parser.process(text);
    let vge = VgeEngine::new(CELL, 1.0);
    let prt = PrtEngine::new();
    vss_protocol::encode_snapshot(
        vss_protocol::frame::SNAPSHOT_VERSION,
        rows,
        cols,
        sequence_id,
        &parser.screen().binary_snapshot(),
        &vge.binary_snapshot(),
        &prt.binary_snapshot(),
        vss_protocol::frame::DEFAULT_MAX_FRAGMENT_BYTES,
    )
}

fn create_portal_envelope(id: &str) -> Vec<u8> {
    use prt_protocol::command::{AnchorMode, Command, CreatePortalBody};
    prt_protocol::encode::build_envelope(&[(
        Command::CreatePortal(CreatePortalBody {
            id: id.into(),
            size_w: 20,
            size_h: 4,
            origin_x: 0,
            origin_y: 0,
            anchor_mode: AnchorMode::Live,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 10,
        }),
        1,
    )])
}

/// Text *after* the snapshot belongs on the restored screen. The old
/// host pipeline fed the whole chunk's text to the vt100 first and
/// applied the restore afterwards, so this text was wiped by the
/// restore it was meant to land on.
#[test]
fn text_after_a_snapshot_lands_on_the_restored_screen() {
    let mut host = Host::new();
    let mut chunk = snapshot_envelope(b"restored");
    chunk.extend_from_slice(b" and more");
    host.feed(&chunk);

    assert_eq!(host.screen(), "restored and more");
}

/// …and so does a command after it: it belongs to the engines the
/// snapshot installed. Applied out of order, `p1` was created on the
/// pre-restore engine and thrown away with it.
#[test]
fn a_portal_created_after_a_snapshot_survives_the_restore() {
    let mut host = Host::new();
    let mut chunk = snapshot_envelope(b"restored");
    chunk.extend_from_slice(&create_portal_envelope("p1"));
    host.feed(&chunk);

    assert!(
        host.prt.state.current().portals.contains_key("p1"),
        "the portal created after the snapshot was wiped by it"
    );
}

/// Both halves at once, around one snapshot — the shape a real attach
/// produces once a network hop coalesces the daemon's snapshot with
/// the inner multiplexer's redraw.
#[test]
fn text_and_a_command_around_one_snapshot_both_land_correctly() {
    let mut host = Host::new();
    let mut chunk = b"before".to_vec();
    chunk.extend_from_slice(&snapshot_envelope(b"restored"));
    chunk.extend_from_slice(&create_portal_envelope("p1"));
    chunk.extend_from_slice(b" after");
    host.feed(&chunk);

    assert_eq!(host.screen(), "restored after");
    assert!(host.prt.state.current().portals.contains_key("p1"));
}

/// A detach rolls back to what was on screen before the attach, and
/// text after the notify in the same chunk lands on *that* screen.
#[test]
fn detach_restores_the_pre_attach_view_before_the_text_after_it() {
    let mut host = Host::new();
    host.feed(b"my shell");
    let mut chunk = snapshot_envelope(b"session");
    chunk.extend_from_slice(&vss_protocol::encode_detach_notify());
    chunk.extend_from_slice(b"\r\nback");
    host.feed(&chunk);

    assert_eq!(host.screen(), "my shell\nback");
}

/// The two controls: the same bytes through the old whole-chunk
/// pipeline get both halves wrong. Without these, the tests above
/// would pass against the code that had the bug.
#[test]
fn the_unordered_pipeline_wipes_the_text_after_the_snapshot() {
    let mut host = Host::new();
    let mut chunk = snapshot_envelope(b"restored");
    chunk.extend_from_slice(b" and more");
    host.feed_unordered(&chunk);

    assert_eq!(
        host.screen(),
        "restored",
        "premise: unordered, the restore lands on top of the text after it"
    );
}

#[test]
fn the_unordered_pipeline_loses_a_portal_created_after_the_snapshot() {
    let mut host = Host::new();
    let mut chunk = snapshot_envelope(b"restored");
    chunk.extend_from_slice(&create_portal_envelope("p1"));
    host.feed_unordered(&chunk);

    assert!(
        !host.prt.state.current().portals.contains_key("p1"),
        "premise: unordered, the restore wipes the portal created after it"
    );
}

/// A snapshot carries the sender's grid geometry, which is only right
/// at the instant it was taken. Installing it leaves the receiver's
/// vt100 at a size nothing else in the renderer agrees with, until
/// some later resize happens to correct it.
#[test]
fn a_restore_keeps_the_receiving_context_s_own_size() {
    let mut host = Host::new();
    assert_eq!(host.parser.screen().size(), (24, 80));
    host.feed(&snapshot_of(10, 40, 3, b"smaller"));

    assert_eq!(host.parser.screen().size(), (24, 80));
    assert_eq!(host.screen(), "smaller");
}

/// An attach that dies without a `DetachNotify` used to leave its
/// stash behind, so the *next* attach didn't take one and a later
/// detach restored a screen from two attaches ago. What the pane was
/// actually showing when the user re-attached is the dead session's
/// leftovers, and that is what they should get back.
#[test]
fn a_second_attach_stashes_what_is_on_screen_now() {
    let mut host = Host::new();
    host.feed(b"my shell");
    host.feed(&snapshot_of(24, 80, 1, b"session one"));
    assert_eq!(host.screen(), "session one");

    // The connection drops here: no DetachNotify. A fresh attach
    // arrives with its own sequence id.
    host.feed(&snapshot_of(24, 80, 2, b"session two"));
    assert_eq!(host.screen(), "session two");

    host.feed(&vss_protocol::encode_detach_notify());
    assert_eq!(
        host.screen(),
        "session one",
        "detach restored a stash from before the previous attach"
    );
}

/// Two snapshots of the *same* attach must not re-stash — the second
/// would capture the first one's restored screen and the detach would
/// put the session back instead of the pane.
#[test]
fn a_second_snapshot_of_one_attach_does_not_re_stash() {
    let mut host = Host::new();
    host.feed(b"my shell");
    host.feed(&snapshot_of(24, 80, 5, b"session"));
    host.feed(&snapshot_of(24, 80, 5, b"session redrawn"));
    host.feed(&vss_protocol::encode_detach_notify());

    assert_eq!(host.screen(), "my shell");
}
