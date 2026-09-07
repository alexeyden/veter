//! Answers to the terminal identification and mode queries a program
//! sends and then *waits* for.
//!
//! Nothing in this stack used to answer any of them. The vt100 fork
//! routes `CSI c`, `CSI > c`, `CSI > q` and DECRQM to its
//! `unhandled_csi` callback, which does nothing, so vim, neovim, tmux
//! and a handful of zsh plugins each ate their own DA1 timeout on every
//! start — a visible stall, once per launch, per pane.
//!
//! The replies live here rather than in either engine because two
//! engines produce them: the host-level VGE engine answers for the host
//! grid, and the PRT engine answers for each portal (the same split
//! that already exists for DSR — see `prt::TerminalEvent`). Two
//! responders must not disagree about what terminal this is.

/// Primary device attributes (DA1), the answer to `CSI c`.
///
/// `62` — VT220-class; `22` — ANSI colour. Deliberately short: every
/// number here is a capability claim, and a claim we don't implement
/// (sixel, selective erase, DECRQSS) is worse than an absent one,
/// because the sender acts on it and gets no reply to the follow-up.
pub const DA1: &[u8] = b"\x1b[?62;22c";

/// Secondary device attributes (DA2), the answer to `CSI > c`.
///
/// `CSI > Pp ; Pv ; Pc c`. `Pp = 0` is "unknown terminal type", which
/// is the honest answer — the alternative is borrowing xterm's `41`
/// and inviting xterm-specific behaviour we haven't implemented.
pub const DA2: &[u8] = b"\x1b[>0;1;0c";

/// XTVERSION, the answer to `CSI > q`: `DCS > | <name> ST`.
///
/// The version is this crate's, which is `0.1.0` and always will be —
/// the build a binary came from lives in `veter-version`, behind a
/// build script keyed on `HEAD`, and taking that dependency here would
/// rebuild every crate in the workspace on every commit. A name that
/// parses is what the sender is actually after.
pub const XTVERSION: &[u8] = b"\x1bP>|veter(0.1.0)\x1b\\";

/// DECRQM report values (`CSI ? Ps ; Pm $ y`).
const MODE_NOT_RECOGNISED: u8 = 0;
const MODE_SET: u8 = 1;
const MODE_RESET: u8 = 2;

/// Answer one DECRQM against `screen`'s live mode state.
///
/// A mode the screen doesn't model is reported as "not recognised"
/// rather than left unanswered: `0` is a real answer that unblocks the
/// sender and tells it the truth, where silence is what costs it a
/// timeout.
#[must_use]
pub fn decrqm(private: bool, mode: u16, screen: &vt100::Screen) -> Vec<u8> {
    use vt100::{MouseProtocolEncoding as Enc, MouseProtocolMode as Mode};

    let state = if private {
        match mode {
            // DECCKM — application cursor keys.
            1 => Some(screen.application_cursor()),
            // DECSCNM — reverse video, which terminfo's `flash` uses.
            5 => Some(screen.reverse_video()),
            // DECAWM.
            7 => Some(screen.autowrap()),
            // X10 mouse reporting.
            9 => Some(screen.mouse_protocol_mode() == Mode::Press),
            // Cursor blink, which DECSCUSR's odd parameters also set.
            12 => Some(screen.cursor_blink()),
            // DECTCEM — the mode is *cursor visible*, so it is set
            // when the screen is not hiding it.
            25 => Some(!screen.hide_cursor()),
            // Alternate screen, all three spellings. The fork
            // implements 47 and 1049; 1047 shares their grid, so
            // reporting it from the same bit is consistent with what
            // the screen would do if asked to set it.
            47 | 1047 | 1049 => Some(screen.alternate_screen()),
            1000 => Some(screen.mouse_protocol_mode() == Mode::PressRelease),
            // Focus reporting — the renderer sends CSI I / CSI O.
            1004 => Some(screen.focus_reporting()),
            1002 => Some(screen.mouse_protocol_mode() == Mode::ButtonMotion),
            1003 => Some(screen.mouse_protocol_mode() == Mode::AnyMotion),
            1005 => Some(screen.mouse_protocol_encoding() == Enc::Utf8),
            1006 => Some(screen.mouse_protocol_encoding() == Enc::Sgr),
            1016 => Some(screen.mouse_protocol_encoding() == Enc::SgrPixels),
            2004 => Some(screen.bracketed_paste()),
            _ => None,
        }
    } else {
        match mode {
            // IRM — insert/replace (terminfo `smir` / `rmir`).
            4 => Some(screen.insert_mode()),
            // LNM — LF also does a carriage return.
            20 => Some(screen.newline_mode()),
            _ => None,
        }
    };

    let value = match state {
        Some(true) => MODE_SET,
        Some(false) => MODE_RESET,
        None => MODE_NOT_RECOGNISED,
    };
    let prefix = if private { "?" } else { "" };
    format!("\x1b[{prefix}{mode};{value}$y").into_bytes()
}

/// Answer one XTWINOPS size query (`CSI 14 t`, `CSI 16 t`,
/// `CSI 18 t`).
///
/// xterm answers each with a different leading code — `4` for the text
/// area in pixels, `6` for one cell, `8` for the text area in cells —
/// and puts height before width in all three. Programs ask so they can
/// size graphics against the grid, and they block on the reply.
///
/// Returns `None` for an op this doesn't answer, so the caller leaves
/// the sequence alone rather than inventing a report.
#[must_use]
pub fn window_size_report(
    op: u16,
    rows: u16,
    cols: u16,
    cell_width_px: u16,
    cell_height_px: u16,
) -> Option<Vec<u8>> {
    let (code, height, width) = match op {
        14 => (
            4,
            rows.saturating_mul(cell_height_px),
            cols.saturating_mul(cell_width_px),
        ),
        16 => (6, cell_height_px, cell_width_px),
        18 => (8, rows, cols),
        _ => return None,
    };
    Some(format!("\x1b[{code};{height};{width}t").into_bytes())
}

/// Format one colour the way the OSC colour reports do.
///
/// xterm reports sixteen bits per channel and answers an eight-bit
/// value by repeating each byte — `0xab` becomes `abab` — which is
/// what makes both ends of the range come out exactly right (`ff`
/// becomes `ffff`, not `ff00`). Clients that parse these expect the
/// doubling.
fn rgb_spec(r: u8, g: u8, b: u8) -> String {
    format!("rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
}

/// Answer an `OSC 10 / 11 / 12 ; ?` query.
///
/// vim's `t_RB` and neovim's `background` detection both send
/// `OSC 11 ; ? ST` at startup and wait for it; unanswered, it costs the
/// same stall at launch an unanswered DA1 did.
#[must_use]
pub fn dynamic_color_report(
    which: vt100::DynamicColor,
    (r, g, b): (u8, u8, u8),
) -> Vec<u8> {
    let code = match which {
        vt100::DynamicColor::Foreground => 10,
        vt100::DynamicColor::Background => 11,
        vt100::DynamicColor::Cursor => 12,
    };
    format!("\x1b]{code};{}\x1b\\", rgb_spec(r, g, b)).into_bytes()
}

/// Answer an `OSC 4 ; <index> ; ?` palette query.
#[must_use]
pub fn palette_color_report(index: u8, (r, g, b): (u8, u8, u8)) -> Vec<u8> {
    format!("\x1b]4;{index};{}\x1b\\", rgb_spec(r, g, b)).into_bytes()
}

/// Parse an X colour specification as it appears in OSC 4 / 10 / 11 /
/// 12: `rgb:R/G/B` with one to four hex digits per channel, or the
/// `#RGB` / `#RRGGBB` / `#RRRRGGGGBBBB` forms.
///
/// Short channels scale rather than truncate, so `rgb:f/f/f` and
/// `rgb:ffff/ffff/ffff` both come out white.
#[must_use]
pub fn parse_color_spec(spec: &[u8]) -> Option<(u8, u8, u8)> {
    let spec = std::str::from_utf8(spec).ok()?.trim();

    fn channel(s: &str) -> Option<u8> {
        if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(s, 16).ok()?;
        let bits = 4 * u32::try_from(s.len()).ok()?;
        let max = (1u32 << bits) - 1;
        u8::try_from(v * 255 / max).ok()
    }

    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let r = channel(parts.next()?)?;
        let g = channel(parts.next()?)?;
        let b = channel(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        return Some((r, g, b));
    }

    if let Some(rest) = spec.strip_prefix('#') {
        if rest.is_empty() || rest.len() % 3 != 0 || rest.len() > 12 {
            return None;
        }
        let n = rest.len() / 3;
        return Some((
            channel(&rest[..n])?,
            channel(&rest[n..2 * n])?,
            channel(&rest[2 * n..])?,
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_after(bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(24, 80, 100);
        p.process(bytes);
        p
    }

    #[test]
    fn decrqm_reports_a_mode_the_screen_actually_tracks() {
        let p = screen_after(b"\x1b[?2004h");
        assert_eq!(decrqm(true, 2004, p.screen()), b"\x1b[?2004;1$y".to_vec());

        let p = screen_after(b"\x1b[?2004h\x1b[?2004l");
        assert_eq!(decrqm(true, 2004, p.screen()), b"\x1b[?2004;2$y".to_vec());
    }

    #[test]
    fn decrqm_reports_cursor_visibility_the_right_way_round() {
        // DECTCEM set means the cursor is *shown*.
        let p = screen_after(b"");
        assert_eq!(decrqm(true, 25, p.screen()), b"\x1b[?25;1$y".to_vec());
        let p = screen_after(b"\x1b[?25l");
        assert_eq!(decrqm(true, 25, p.screen()), b"\x1b[?25;2$y".to_vec());
    }

    /// The point of answering at all: an unknown mode gets `0`, not
    /// silence. Silence is what makes the sender wait.
    #[test]
    fn decrqm_answers_unknown_modes_rather_than_staying_silent() {
        let p = screen_after(b"");
        assert_eq!(decrqm(true, 12345, p.screen()), b"\x1b[?12345;0$y".to_vec());
        // KAM (`CSI 2 h`) is a real ANSI mode the screen doesn't model.
        assert_eq!(decrqm(false, 2, p.screen()), b"\x1b[2;0$y".to_vec());
    }

    /// IRM and LNM used to report "not recognised" because the screen
    /// didn't model them. It does now, so the report is the truth.
    #[test]
    fn decrqm_reports_the_ansi_modes_the_screen_now_models() {
        let p = screen_after(b"");
        assert_eq!(decrqm(false, 4, p.screen()), b"\x1b[4;2$y".to_vec());
        let p = screen_after(b"\x1b[4h\x1b[20h");
        assert_eq!(decrqm(false, 4, p.screen()), b"\x1b[4;1$y".to_vec());
        assert_eq!(decrqm(false, 20, p.screen()), b"\x1b[20;1$y".to_vec());
    }

    /// `CSI 14 t` / `16 t` / `18 t` each answer with their own leading
    /// code, height before width.
    #[test]
    fn window_size_reports_use_xterms_shapes() {
        assert_eq!(
            window_size_report(14, 24, 80, 9, 20).unwrap(),
            b"\x1b[4;480;720t".to_vec(),
        );
        assert_eq!(
            window_size_report(16, 24, 80, 9, 20).unwrap(),
            b"\x1b[6;20;9t".to_vec(),
        );
        assert_eq!(
            window_size_report(18, 24, 80, 9, 20).unwrap(),
            b"\x1b[8;24;80t".to_vec(),
        );
        assert!(window_size_report(99, 24, 80, 9, 20).is_none());
    }

    /// The colour reports double each byte, so both ends of the range
    /// land exactly: `ff` reports as `ffff`, not `ff00`.
    #[test]
    fn colour_reports_widen_each_channel_to_sixteen_bits() {
        assert_eq!(
            dynamic_color_report(vt100::DynamicColor::Background, (0x1e, 0x1e, 0x1e)),
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\".to_vec(),
        );
        assert_eq!(
            palette_color_report(9, (0xff, 0x00, 0x80)),
            b"\x1b]4;9;rgb:ffff/0000/8080\x1b\\".to_vec(),
        );
    }

    #[test]
    fn colour_specs_parse_in_every_shape_clients_send() {
        assert_eq!(parse_color_spec(b"rgb:ff/00/80"), Some((0xff, 0x00, 0x80)));
        assert_eq!(
            parse_color_spec(b"rgb:ffff/0000/8080"),
            Some((0xff, 0x00, 0x80)),
        );
        // A short channel scales rather than truncates.
        assert_eq!(parse_color_spec(b"rgb:f/f/f"), Some((255, 255, 255)));
        assert_eq!(parse_color_spec(b"#ff0080"), Some((0xff, 0x00, 0x80)));
        assert_eq!(parse_color_spec(b"#f08"), Some((0xff, 0x00, 0x88)));
        assert_eq!(parse_color_spec(b"?"), None);
        assert_eq!(parse_color_spec(b"rgb:zz/00/00"), None);
        assert_eq!(parse_color_spec(b"rgb:ff/00"), None);
    }

    #[test]
    fn decrqm_reports_autowrap() {
        let p = screen_after(b"");
        assert_eq!(decrqm(true, 7, p.screen()), b"\x1b[?7;1$y".to_vec());
        let p = screen_after(b"\x1b[?7l");
        assert_eq!(decrqm(true, 7, p.screen()), b"\x1b[?7;2$y".to_vec());
    }

    #[test]
    fn decrqm_distinguishes_the_two_sgr_mouse_encodings() {
        let p = screen_after(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(decrqm(true, 1006, p.screen()), b"\x1b[?1006;1$y".to_vec());
        assert_eq!(decrqm(true, 1016, p.screen()), b"\x1b[?1016;2$y".to_vec());

        let p = screen_after(b"\x1b[?1000h\x1b[?1016h");
        assert_eq!(decrqm(true, 1016, p.screen()), b"\x1b[?1016;1$y".to_vec());
    }
}
