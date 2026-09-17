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
        // SES leads, outside the walk — see `veter_host::pipeline`.
        let chunk = self.ses.process_pty_chunk(chunk);
        drive_chunk(
            &chunk,
            Engines {
                vss: &mut self.vss,
                prt: &mut self.prt,
                vft: &mut self.vft,
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
    chunk.extend_from_slice(&vss_protocol::encode_detach_notify(7));
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

    host.feed(&vss_protocol::encode_detach_notify(2));
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
    host.feed(&vss_protocol::encode_detach_notify(5));

    assert_eq!(host.screen(), "my shell");
}

/// Cell metrics belong to whoever renders. A snapshot carries the
/// sender's — the daemon's, which are the last renderer's or the 8x16
/// defaults if its probe timed out — and installing them would lay
/// every element out against a cell size that isn't on screen.
#[test]
fn a_restore_keeps_the_receiver_s_cell_metrics() {
    let mut host = Host::new();
    host.vge.set_dimensions((11, 24), 2.0);

    // The snapshot in `snapshot_of` is built with the 8x16 default.
    host.feed(&snapshot_of(24, 80, 4, b"restored"));

    assert_eq!(host.vge.cell_px(), (11, 24));
    assert!((host.vge.scale_factor() - 2.0).abs() < f32::EPSILON);
}

// ---- Detach: what the renderer owes the session ------------------------
//
// `vsd` keeps reading after it sends `DetachNotify`, forwarding to the
// session everything up to the renderer's `DetachAccepted` and treating
// what follows as no longer the session's (`doc/session-manager.md`
// §4.4). That only works if the answer comes *last*.

impl Host {
    /// Feed `chunk` and return what the renderer writes back, in the
    /// order `veter`'s host loop writes it.
    fn feed_replies(&mut self, chunk: &[u8]) -> Vec<u8> {
        let chunk = self.ses.process_pty_chunk(chunk);
        drive_chunk(
            &chunk,
            Engines {
                vss: &mut self.vss,
                prt: &mut self.prt,
                vft: &mut self.vft,
                vge: &mut self.vge,
                parser: &mut self.parser,
            },
            &mut self.backup,
            None,
        );
        self.prt.flush_pending_events();
        [
            self.prt.take_responses(),
            self.vge.take_responses(),
            self.vft.take_responses(),
            self.vss.take_responses(),
            self.ses.take_responses(),
        ]
        .concat()
    }
}

/// A `BeginUpload` to a fresh path under the temp dir, and that path.
fn begin_upload(name: &str) -> (Vec<u8>, std::path::PathBuf) {
    use vft_protocol::command::{BeginUploadBody, Command};
    let path = std::env::temp_dir().join(format!("vss-detach-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let env = vft_protocol::encode::build_envelope(&[(
        Command::BeginUpload(BeginUploadBody {
            transfer_id: name.into(),
            host_path: path.to_string_lossy().into_owned(),
            basename: String::new(),
            total_bytes: 100,
            flags: 0,
            mode: 0,
            mtime: 0,
        }),
        1,
    )]);
    (env, path)
}

fn write_portal(id: &str, data: Vec<u8>) -> Vec<u8> {
    use prt_protocol::command::{Command, WritePortalBody};
    prt_protocol::encode::build_envelope(&[(
        Command::WritePortal(WritePortalBody { id: id.into(), data }),
        2,
    )])
}

/// `(frame_type, request_id, body)` for every frame of every envelope
/// under `marker` in `bytes`.
fn frames(bytes: &[u8], marker: [u8; 3]) -> Vec<(u8, u32, Vec<u8>)> {
    let mut out = Vec::new();
    for payload in vft_protocol::apc::ApcStream::with_marker(marker).feed(bytes).payloads {
        let mut r = vft_protocol::codec::Reader::new(&payload);
        let _version = r.u8().unwrap();
        let _len = r.u32().unwrap();
        while !r.at_end() {
            let ft = r.u8().unwrap();
            let rid = r.u32().unwrap();
            let len = r.u32().unwrap() as usize;
            out.push((ft, rid, r.take(len).unwrap().to_vec()));
        }
    }
    out
}

/// The ids of the transfers a host→client VFT stream says were aborted.
fn aborted(bytes: &[u8]) -> Vec<String> {
    frames(bytes, *vft_protocol::frame::MARKER_H2C)
        .into_iter()
        .filter(|(ft, _, _)| *ft == vft_protocol::frame::EVT_TRANSFER_ABORTED)
        .map(|(_, _, body)| {
            vft_protocol::codec::Reader::new(&body).string().unwrap().to_owned()
        })
        .collect()
}

/// Where `needle` starts in `haystack`.
fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The context's own VFT engine is in no snapshot, so a restore leaves
/// its transfers running: a download would go on streaming into
/// whatever the pty belongs to after the detach. It is aborted, and
/// the abort is ahead of the answer, so `vsd` hands it to the session.
#[test]
fn a_detach_aborts_the_session_s_transfer_ahead_of_its_answer() {
    let mut host = Host::new();
    host.feed(b"my shell");
    host.feed(&snapshot_envelope(b"session"));
    let (upload, path) = begin_upload("host-level");
    host.feed(&upload);

    let out = host.feed_replies(&vss_protocol::encode_detach_notify(7));
    let _ = std::fs::remove_file(&path);

    assert_eq!(aborted(&out), ["host-level"]);
    let answer = position(&out, &vss_protocol::encode_detach_accepted(7))
        .expect("the detach was not answered");
    let abort = position(&out, b"\x1b_vft").unwrap();
    assert!(abort < answer, "the abort came after the answer");
    assert_eq!(host.screen(), "my shell");
}

/// A transfer inside one of the session's portals: the restore throws
/// the portal away, which used to stop the transfer without telling
/// the client that started it.
#[test]
fn a_detach_tells_a_portal_s_client_its_transfer_is_gone() {
    let mut host = Host::new();
    host.feed(b"my shell");
    host.feed(&snapshot_envelope(b"session"));
    host.feed(&create_portal_envelope("pane"));
    let (upload, path) = begin_upload("in-portal");
    host.feed(&write_portal("pane", upload));

    let out = host.feed_replies(&vss_protocol::encode_detach_notify(7));
    let _ = std::fs::remove_file(&path);

    let reply = frames(&out, *prt_protocol::frame::MARKER_T2C)
        .into_iter()
        .find(|(ft, _, _)| *ft == prt_protocol::frame::EVT_RAW_REPLY)
        .expect("no RawReply for the portal");
    let mut r = prt_protocol::codec::Reader::new(&reply.2);
    assert_eq!(r.string().unwrap(), "pane");
    assert_eq!(aborted(r.bytes().unwrap()), ["in-portal"]);

    let answer = position(&out, &vss_protocol::encode_detach_accepted(7)).unwrap();
    assert!(position(&out, b"\x1b_prt").unwrap() < answer);
}

/// The session's last commands and the `DetachNotify` arrive in one
/// chunk routinely — the daemon writes the notify right behind the
/// last output it forwards. Their replies are owed to the session, and
/// the restore used to throw them away with the state it replaced.
#[test]
fn replies_to_commands_ahead_of_the_detach_survive_it() {
    let mut host = Host::new();
    host.feed(b"my shell");
    host.feed(&snapshot_envelope(b"session"));

    let mut chunk = create_portal_envelope("late");
    chunk.extend_from_slice(&vss_protocol::encode_detach_notify(7));
    let out = host.feed_replies(&chunk);

    let ok = frames(&out, *prt_protocol::frame::MARKER_T2C)
        .into_iter()
        .any(|(ft, rid, _)| ft == prt_protocol::frame::RSP_OK && rid == 1);
    assert!(ok, "the CreatePortal's Ok was dropped by the restore");
    let answer = position(&out, &vss_protocol::encode_detach_accepted(7)).unwrap();
    assert!(position(&out, b"\x1b_prt").unwrap() < answer);
}
