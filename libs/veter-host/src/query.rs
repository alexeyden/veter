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
            // X10 mouse reporting.
            9 => Some(screen.mouse_protocol_mode() == Mode::Press),
            // DECTCEM — the mode is *cursor visible*, so it is set
            // when the screen is not hiding it.
            25 => Some(!screen.hide_cursor()),
            // Alternate screen, all three spellings. The fork
            // implements 47 and 1049; 1047 shares their grid, so
            // reporting it from the same bit is consistent with what
            // the screen would do if asked to set it.
            47 | 1047 | 1049 => Some(screen.alternate_screen()),
            1000 => Some(screen.mouse_protocol_mode() == Mode::PressRelease),
            1002 => Some(screen.mouse_protocol_mode() == Mode::ButtonMotion),
            1003 => Some(screen.mouse_protocol_mode() == Mode::AnyMotion),
            1005 => Some(screen.mouse_protocol_encoding() == Enc::Utf8),
            1006 => Some(screen.mouse_protocol_encoding() == Enc::Sgr),
            1016 => Some(screen.mouse_protocol_encoding() == Enc::SgrPixels),
            2004 => Some(screen.bracketed_paste()),
            _ => None,
        }
    } else {
        // No ANSI mode is modelled by the screen: IRM and LNM are both
        // unimplemented, and claiming otherwise would be a lie the
        // sender acts on.
        None
    };

    let value = match state {
        Some(true) => MODE_SET,
        Some(false) => MODE_RESET,
        None => MODE_NOT_RECOGNISED,
    };
    let prefix = if private { "?" } else { "" };
    format!("\x1b[{prefix}{mode};{value}$y").into_bytes()
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
        assert_eq!(decrqm(false, 4, p.screen()), b"\x1b[4;0$y".to_vec());
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
