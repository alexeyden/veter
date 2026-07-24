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
/// See `doc/client-integration-groundwork.md` item 5.
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
                engine.apply_payload(&payload);
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
        // The line tracker baselines itself on its first update, so the
        // host's very first `after_vt100_process` establishes the
        // origin rather than measuring a delta. Do the same here, or
        // every scroll in the first chunk reads as zero.
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
}

