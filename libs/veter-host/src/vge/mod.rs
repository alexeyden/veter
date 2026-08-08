// VGE host engine state. Renderer code lives in the `veter` binary's
// `vge::render`; this module contains only the protocol-agnostic
// state machinery so headless consumers (vsd) can link it.
//
// Wire-format types live in the `vge-protocol` crate and are
// re-exported here for convenience.

pub mod snapshot;
pub mod state;

pub use snapshot::SnapshotError;
pub use state::{GpuImageId, HostThemePalette, VgeEngine, VgeState};

pub use vge_protocol::*;

use vge_protocol::apc::Segment;

/// Drive one chunk through the **terminal stage** — the last engine in
/// the byte pipeline, the one that owns the vt100.
///
/// Every other engine (PRT, VFT, VSS, SES) is a plain byte filter: it
/// consumes a whole chunk's envelopes before any of that chunk's text
/// reaches the vt100, so anything it resolves against the screen sees
/// the *pre-chunk* state — off by exactly the text that arrived
/// alongside it. That is harmless for engines with no on-screen state,
/// and wrong for VGE, whose element origins are viewport-relative at
/// command-processing time (spec §5.2).
///
/// So VGE runs last and consumes segments instead: text is handed to
/// the vt100, then each payload or event is applied at the point in
/// the stream where it actually appeared. `after_vt100_process` is
/// re-run before any command that follows new text, because the line
/// tracker anchoring depends on it — but only when text is pending, so
/// a run of commands costs one sync rather than one per command.
///
/// See `doc/vector-graphics-extension.md` §5.2, which makes this
/// ordering normative.
pub fn drive_terminal_stage<CB: vt100::Callbacks>(
    engine: &mut VgeEngine,
    parser: &mut vt100::Parser<CB>,
    input: &[u8],
) {
    let mut text_pending = false;
    for seg in engine.feed_segments(input) {
        match seg {
            Segment::Pass(text) => {
                if !text.is_empty() {
                    parser.process(&text);
                    text_pending = true;
                }
            }
            Segment::Payload(payload) => {
                if text_pending {
                    engine.after_vt100_process(parser);
                    text_pending = false;
                }
                // The parser is the live screen a cursor- or
                // marker-anchored origin resolves against (§9.4), and
                // it is up to date here precisely because the sync
                // above already ran.
                engine.apply_payload(&payload, Some(parser as &dyn state::ScreenAnchor));
            }
            Segment::Event(ev) => {
                if text_pending {
                    engine.after_vt100_process(parser);
                    text_pending = false;
                }
                engine.apply_terminal_event(ev);
            }
        }
    }
    // Unconditional, matching the pre-segment behaviour: alt-screen
    // transitions, scrollback eviction and queued DSR replies are all
    // resolved once per chunk even when the chunk carried no text.
    engine.after_vt100_process(parser);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vge_protocol::codec::Writer;
    use vge_protocol::command::OriginAnchor;
    use vge_protocol::frame::*;

    /// Minimal `CreateElement` envelope for a 1x1 rect at `origin_y`,
    /// hand-built so the test does not depend on the encoder.
    fn create_element_envelope(id: &str, origin_y: f32) -> Vec<u8> {
        let mut body = Writer::new();
        body.str(id);
        body.varu(1); // n_commands
        body.u8(OP_FILL_RECTANGLES);
        body.u8(STYLE_FLAT);
        body.u8(0x01); // RGBA8888
        body.u8(0xFF);
        body.u8(0x00);
        body.u8(0x00);
        body.u8(0xFF);
        body.varu(1); // n_rects
        body.f32(0.0);
        body.f32(0.0);
        body.f32(1.0);
        body.f32(1.0);
        body.f32(0.0); // origin.x
        body.f32(origin_y);
        body.u8(1); // is_visible
        for b in 0i32.to_le_bytes() {
            body.u8(b);
        }

        let mut frames = Vec::new();
        frames.push(CMD_CREATE_ELEMENT);
        frames.extend_from_slice(&1u32.to_le_bytes()); // request_id
        frames.extend_from_slice(&(body.buf.len() as u32).to_le_bytes());
        frames.extend_from_slice(&body.buf);

        let mut payload = vec![PROTOCOL_VERSION];
        payload.extend_from_slice(&(frames.len() as u32).to_le_bytes());
        payload.extend_from_slice(&frames);

        let mut env = vec![ESC, APC_OPEN];
        env.extend_from_slice(MARKER_C2T);
        vge_protocol::codec::stuff(&payload, &mut env);
        env.push(ESC);
        env.push(ST_CLOSE);
        env
    }

    fn engine_and_parser() -> (VgeEngine, vt100::Parser) {
        // 4 rows so a handful of newlines is enough to scroll, which is
        // what moves `top_of_live_screen` and therefore the anchor.
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut parser = vt100::Parser::new(4, 20, 100);
        // Mirror the host: its first `after_vt100_process` picks up
        // the parser's line origin before any command is applied.
        engine.after_vt100_process(&mut parser);
        (engine, parser)
    }

    #[test]
    fn command_anchors_against_text_that_preceded_it_in_the_same_chunk() {
        // The bug this stage exists to fix: text and a command arriving
        // in one read. The element must anchor below the six lines that
        // came before it, not against the pre-chunk screen.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n".to_vec();
        chunk.extend(create_element_envelope("mid", 0.0));
        chunk.extend_from_slice(b"g\r\nh\r\n");
        drive_terminal_stage(&mut engine, &mut parser, &chunk);

        let anchor = engine.state.elements()["mid"].anchor_line;

        // Same bytes, but with the command applied before any of the
        // text reaches the vt100 — what every byte-filter engine does.
        let (mut naive, mut naive_parser) = engine_and_parser();
        let passthrough = naive.process_pty_chunk(&chunk);
        naive_parser.process(&passthrough);
        naive.after_vt100_process(&mut naive_parser);
        let naive_anchor = naive.state.elements()["mid"].anchor_line;

        assert_eq!(
            anchor, 3,
            "six committed lines on a 4-row screen put the live region \
             at line 3; the element anchors there"
        );
        assert_eq!(naive_anchor, 0, "the unordered path anchors pre-chunk");
        assert_ne!(anchor, naive_anchor, "ordering must change the outcome");
    }

    #[test]
    fn commands_split_across_reads_still_anchor_in_order() {
        // Read boundaries belong to the terminal, so the same stream
        // arriving in two chunks must produce the same anchor.
        let mut whole = b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n".to_vec();
        whole.extend(create_element_envelope("split", 0.0));
        whole.extend_from_slice(b"g\r\n");

        let (mut one, mut one_parser) = engine_and_parser();
        drive_terminal_stage(&mut one, &mut one_parser, &whole);
        let expected = one.state.elements()["split"].anchor_line;

        for cut in 1..whole.len() {
            let (mut e, mut p) = engine_and_parser();
            drive_terminal_stage(&mut e, &mut p, &whole[..cut]);
            drive_terminal_stage(&mut e, &mut p, &whole[cut..]);
            assert_eq!(
                e.state.elements()["split"].anchor_line, expected,
                "cut at {cut} moved the anchor"
            );
        }
    }

    #[test]
    fn erase_display_mid_chunk_spares_a_later_element() {
        // `ESC[2J` drops elements anchored in the live region (§5.7).
        // Ordered events mean an element created *after* the erase
        // survives it — with the events applied up front, the erase
        // would run before the element existed and the outcome would
        // differ only by luck.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = create_element_envelope("before", 0.0);
        chunk.extend_from_slice(b"\x1b[2J");
        chunk.extend(create_element_envelope("after", 0.0));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);

        assert!(
            !engine.state.elements().contains_key("before"),
            "element created before the erase must be dropped"
        );
        assert!(
            engine.state.elements().contains_key("after"),
            "element created after the erase must survive"
        );
    }

    /// `CreateElement` with an anchor mode, built by the encoder so the
    /// test exercises the same bytes a client would send.
    fn create_anchored(id: &str, origin_y: f32, anchor: OriginAnchor) -> Vec<u8> {
        use vge_protocol::codec::Point;
        use vge_protocol::command::{Command, CreateElementBody, DrawCmd};
        use vge_protocol::encode::build_envelope;
        build_envelope(&[(
            Command::CreateElement(CreateElementBody {
                id: id.to_string(),
                commands: vec![DrawCmd::FillRectangles {
                    fill: vge_protocol::command::Style::Flat(
                        vge_protocol::command::Color { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    ),
                    rects: vec![vge_protocol::codec::Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }],
                }],
                origin: Point { x: 0.0, y: origin_y },
                is_visible: true,
                draw_order: 0,
                parent: None,
                size: None,
                transform: None,
                anchor,
            }),
            1,
        )])
    }

    #[test]
    fn cursor_anchor_resolves_against_the_live_cursor_row() {
        // The whole point: a client that cannot read a DSR reply names
        // "where the cursor is" and the terminal resolves it.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = b"a\r\nb\r\n".to_vec(); // cursor lands on row 2
        chunk.extend(create_anchored("cur", 0.0, OriginAnchor::Cursor));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);

        let top = engine.top_of_live_screen();
        assert_eq!(parser.screen().cursor_position().0, 2);
        assert_eq!(engine.state.elements()["cur"].anchor_line, top + 2);
    }

    #[test]
    fn cursor_anchor_accepts_negative_offsets() {
        // Negative `y` is how a client reaches the rows *above* the
        // cursor — the reserved space it just printed into.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = b"a\r\nb\r\n".to_vec();
        chunk.extend(create_anchored("up", -2.0, OriginAnchor::Cursor));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);
        let top = engine.top_of_live_screen();
        assert_eq!(engine.state.elements()["up"].anchor_line, top);
    }

    #[test]
    fn marker_anchor_finds_the_token_row() {
        // The application prints a token on the first row it reserved;
        // the terminal, which owns the grid, resolves it. No cursor
        // arithmetic and no assumption about the live-region height.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = b"x\r\n<<tok>>\r\ny\r\n".to_vec();
        chunk.extend(create_anchored(
            "m",
            0.0,
            OriginAnchor::Marker("<<tok>>".into()),
        ));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);
        let top = engine.top_of_live_screen();
        // Rows: 0 = "x", 1 = "<<tok>>", 2 = "y".
        assert_eq!(engine.state.elements()["m"].anchor_line, top + 1);
    }

    #[test]
    fn marker_anchor_takes_the_most_recent_occurrence() {
        // An application that reprints its token every frame must
        // anchor to the latest one, not the first.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = b"<<tok>>\r\nmid\r\n<<tok>>\r\n".to_vec();
        chunk.extend(create_anchored(
            "m",
            0.0,
            OriginAnchor::Marker("<<tok>>".into()),
        ));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);
        let top = engine.top_of_live_screen();
        assert_eq!(engine.state.elements()["m"].anchor_line, top + 2);
    }

    #[test]
    fn marker_anchor_reaches_a_row_already_in_scrollback() {
        // The driving case: a message that reserves two regions is
        // taller than the screen, so by the time a client anchors to
        // the first marker that row has scrolled off. The marker still
        // names its own line — resolving to an absolute line is the
        // whole point — and only a live-screen search would lose it and
        // fall back to the viewport, drawing over the text up there.
        let (mut engine, mut parser) = engine_and_parser();
        // 4-row screen: <<tok>> is committed at line 0, then six more
        // lines push it three rows past the top of the live screen.
        let mut chunk = b"<<tok>>\r\na\r\nb\r\nc\r\nd\r\ne\r\nf\r\n".to_vec();
        chunk.extend(create_anchored("m", 0.0, OriginAnchor::Marker("<<tok>>".into())));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);

        let top = engine.top_of_live_screen();
        assert_eq!(top, 4, "six lines after the marker on a 4-row screen");
        assert_eq!(engine.state.elements()["m"].anchor_line, 0);
        // Not the viewport fallback, which is what the bug produced.
        assert_ne!(engine.state.elements()["m"].anchor_line, top);
    }

    #[test]
    fn a_scrollback_marker_anchor_is_not_evicted_on_arrival() {
        // Anchoring into scrollback puts an element at a line the
        // eviction sweep is already walking past. It must survive its
        // own creation — the ring still holds the row — and go only
        // when that row does.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = b"<<tok>>\r\n".to_vec();
        chunk.extend_from_slice(&b"x\r\n".repeat(6));
        chunk.extend(create_anchored("m", 0.0, OriginAnchor::Marker("<<tok>>".into())));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);
        assert!(engine.state.elements().contains_key("m"));

        // Scroll far enough to push line 0 out of the 100-row ring.
        drive_terminal_stage(&mut engine, &mut parser, &b"y\r\n".repeat(120));
        assert!(
            !engine.state.elements().contains_key("m"),
            "the element goes when the row it named falls out of scrollback"
        );
    }

    #[test]
    fn unmatched_marker_falls_back_to_the_viewport() {
        // Wrong-but-sane beats silently off by an unpredictable amount.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = b"nothing here\r\n".to_vec();
        chunk.extend(create_anchored(
            "m",
            1.0,
            OriginAnchor::Marker("absent".into()),
        ));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);
        let top = engine.top_of_live_screen();
        assert_eq!(engine.state.elements()["m"].anchor_line, top + 1);
    }

    #[test]
    fn anchor_modes_are_exact_only_because_the_stage_is_ordered() {
        // Ties phases 3 and 5 together: the cursor a command resolves
        // against is the one produced by the text that preceded it in
        // the same read. Applying commands before that text — what a
        // byte-filter engine does — gives a different answer.
        let (mut engine, mut parser) = engine_and_parser();
        let mut chunk = b"a\r\nb\r\nc\r\n".to_vec();
        chunk.extend(create_anchored("cur", 0.0, OriginAnchor::Cursor));
        drive_terminal_stage(&mut engine, &mut parser, &chunk);
        let ordered = engine.state.elements()["cur"].anchor_line;

        let (mut naive, mut naive_parser) = engine_and_parser();
        let passthrough = naive.process_pty_chunk(&chunk);
        naive_parser.process(&passthrough);
        naive.after_vt100_process(&mut naive_parser);
        let unordered = naive.state.elements()["cur"].anchor_line;

        assert_ne!(
            ordered, unordered,
            "ordering is what makes cursor anchoring exact"
        );
    }
}
