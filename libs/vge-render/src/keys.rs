//! The kitty keyboard protocol, client side: ask the terminal for the
//! one flag that makes `Esc` unambiguous, and read the key form that
//! flag produces.
//!
//! A VGE client reads keystrokes and the terminal's own replies off the
//! same channel, and a bare `ESC` is both a keypress and the first byte
//! of an envelope. Guessing between them needs a timer, and the guess
//! is wrong in both directions: a reply split across a read becomes a
//! spurious `Esc`, and the rest of that envelope is then read as keys.
//!
//! Under "disambiguate escape codes" the guess disappears. `Esc`
//! arrives as `CSI 27 u` — a complete sequence with nothing to wait for
//! — so a bare `ESC` on the channel can only be an envelope opener and
//! is simply kept buffered. The price is that C0 control bytes stop
//! being sent at all: `Ctrl+S` is no longer `0x13` but `CSI 115 ; 5 u`,
//! so a client that asks for the flag has to read [`Chord`]s.
//!
//! See `veter`'s `encode_kitty_disambiguated` for the encoder, and the
//! `keyboard_flags` stack in the vt100 fork for where the request
//! lands.

/// Push "disambiguate escape codes" onto the terminal's flag stack.
///
/// **Write this after entering the alternate screen, not before.** The
/// stack is per screen, so a push that precedes `?1049h` lands on the
/// main screen's stack — leaving the flag on the shell afterwards and
/// the client itself still on legacy encodings.
pub const PUSH_DISAMBIGUATE: &[u8] = b"\x1b[>1u";

/// Pop it again, restoring whatever the program before us had asked
/// for. **Write this before leaving the alternate screen**, for the
/// same reason.
///
/// Not optional: the stack outlives the process, so a client that
/// exits without popping leaves its flag for the next alt-screen
/// program in that pane to inherit — which is a program whose parser
/// may know nothing about this form.
pub const POP: &[u8] = b"\x1b[<u";

/// A keypress in the protocol's key form: the *unshifted* codepoint of
/// the key, plus the modifiers held with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    /// The key's unshifted codepoint — `Ctrl+Shift+A` is 97, with
    /// `shift` set, rather than 65.
    pub codepoint: u32,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Chord {
    /// Whether this is `Esc`, whatever was held with it.
    #[must_use]
    pub fn is_esc(&self) -> bool {
        self.codepoint == 27
    }

    /// The lowercase letter of a `Ctrl+<letter>` chord, or `None` if
    /// that is not what this is. This is the form a legacy C0 binding
    /// translates to: `0x13` was `Ctrl+S`, so `ctrl_letter() ==
    /// Some('s')` is the same keypress.
    #[must_use]
    pub fn ctrl_letter(&self) -> Option<char> {
        if !self.ctrl || self.alt {
            return None;
        }
        let c = char::from_u32(self.codepoint)?;
        c.is_ascii_alphabetic().then(|| c.to_ascii_lowercase())
    }
}

/// Decode the body of `CSI <codepoint> [; <modifiers>] u` — the bytes
/// between the `[` and the `u`.
///
/// Deliberately narrow, because the `u` final byte is shared with
/// sequences that are not keys at all: parameterless `CSI u` is SCORC,
/// and the private-prefix forms (`CSI > 1 u`, `CSI < u`, `CSI = … u`,
/// and the `CSI ? <flags> u` reply to a flag query) are the flag stack
/// itself. All of those have a non-numeric or empty first field, so the
/// digit check is what keeps them out.
///
/// Both fields may carry `:`-separated sub-parameters — the key's
/// second is the shifted key, the modifier's is the event type — so the
/// leading one is taken and the rest ignored rather than the sequence
/// being misread.
#[must_use]
pub fn parse_csi_u(params: &[u8]) -> Option<Chord> {
    let mut fields = params.split(|&c| c == b';');
    let codepoint = number(fields.next()?.split(|&c| c == b':').next()?)?;
    let modifiers = match fields.next() {
        Some(f) => number(f.split(|&c| c == b':').next()?)?,
        None => 1,
    };
    // 1 + the bits: shift 1, alt 2, ctrl 4, super 8. A parameter of 1
    // is "no modifiers", which is also its default when absent.
    let bits = modifiers.saturating_sub(1);
    Some(Chord {
        codepoint,
        shift: bits & 0b1 != 0,
        alt: bits & 0b10 != 0,
        ctrl: bits & 0b100 != 0,
    })
}

/// One decimal CSI parameter, or `None` for an empty or non-numeric
/// field.
fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(params: &[u8]) -> Option<Chord> {
        parse_csi_u(params)
    }

    #[test]
    fn modifiers_are_one_plus_their_bits() {
        assert_eq!(
            chord(b"97;5"),
            Some(Chord { codepoint: 97, shift: false, alt: false, ctrl: true })
        );
        assert_eq!(
            chord(b"97;3"),
            Some(Chord { codepoint: 97, shift: false, alt: true, ctrl: false })
        );
        assert_eq!(
            chord(b"97;7"),
            Some(Chord { codepoint: 97, shift: false, alt: true, ctrl: true })
        );
        // An absent modifier field means none, which is what makes a
        // plain Esc `CSI 27 u`.
        assert_eq!(
            chord(b"27"),
            Some(Chord { codepoint: 27, shift: false, alt: false, ctrl: false })
        );
    }

    #[test]
    fn esc_is_esc_whatever_is_held_with_it() {
        assert!(chord(b"27").unwrap().is_esc());
        assert!(chord(b"27;5").unwrap().is_esc());
        assert!(!chord(b"97;5").unwrap().is_esc());
    }

    /// The translation a legacy C0 binding needs: `0x13` and
    /// `CSI 115 ; 5 u` are the same keypress.
    #[test]
    fn ctrl_letter_recovers_the_legacy_chord() {
        assert_eq!(chord(b"115;5").unwrap().ctrl_letter(), Some('s'));
        assert_eq!(chord(b"26;5").unwrap().ctrl_letter(), None, "not a letter");
        // The protocol names the unshifted key, so Ctrl+Shift+S is
        // still the `s` chord.
        assert_eq!(chord(b"115;6").unwrap().ctrl_letter(), Some('s'));
        // Ctrl+Alt is a different keypress from Ctrl alone.
        assert_eq!(chord(b"115;7").unwrap().ctrl_letter(), None);
        // And so is the key with no ctrl at all.
        assert_eq!(chord(b"115").unwrap().ctrl_letter(), None);
    }

    /// The `u` final byte is shared with the flag stack's own
    /// sequences and with SCORC; reading one as a keypress would
    /// invent a keystroke out of a reply.
    #[test]
    fn what_is_not_a_key_is_refused() {
        for params in [
            b"".as_ref(),      // SCORC — no parameters at all
            b"?1".as_ref(),    // the reply to a flag query
            b">1".as_ref(),    // push
            b"<".as_ref(),     // pop
            b"=1;2".as_ref(),  // set
            b"97;".as_ref(),   // empty modifier field
            b"9 7".as_ref(),   // not digits
        ] {
            assert!(parse_csi_u(params).is_none(), "parsed {params:?}");
        }
    }

    /// Sub-parameters are part of the grammar even where nothing here
    /// reads them, so they must not make a sequence unreadable.
    #[test]
    fn sub_parameters_are_skipped_not_choked_on() {
        // `97:65` — the key, then the shifted key it produced.
        assert_eq!(chord(b"97:65;2").unwrap().codepoint, 97);
        // `5:1` — ctrl, then the event type (press).
        assert!(chord(b"97;5:1").unwrap().ctrl);
    }
}
