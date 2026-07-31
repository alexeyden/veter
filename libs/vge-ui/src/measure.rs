//! How wide a string is, in grid cells.
//!
//! Modal boxes are sized in cells and their carets are placed in cells,
//! so every layout here needs to turn a string into a column count. A
//! character count is wrong for anything but narrow scripts: CJK and
//! most emoji occupy two columns, combining marks none.
//!
//! The rule is `unicode-width`, the same one the vt100 grid applies to
//! its own cells (`Cell::set` there sets `is_wide` from it), so a modal
//! lines up with the terminal text behind it. The host shapes VGE text
//! with the font's real glyph advances rather than snapping to cells
//! (VGE §7.4), so for a glyph that comes from a fallback font this is an
//! approximation — but it is the grid's own approximation, and far
//! closer than counting characters.

use unicode_width::UnicodeWidthChar as _;
use unicode_width::UnicodeWidthStr as _;

/// Columns `text` occupies when drawn.
pub fn text_cells(text: &str) -> f32 {
    text.width() as f32
}

/// Columns the first `chars` characters of `text` occupy — the caret
/// column for a cursor held as a char index. A `chars` past the end
/// measures the whole string.
pub fn prefix_cells(text: &str, chars: usize) -> f32 {
    text.chars()
        .take(chars)
        .map(|c| c.width().unwrap_or(0))
        .sum::<usize>() as f32
}

/// Clip `text` to at most `max` columns, marking a cut with `…`.
///
/// The marker costs a column of its own, so a clipped result keeps
/// `max - 1` columns of text. A wide glyph straddling the boundary is
/// dropped rather than half-drawn, which can leave the result one
/// column short of `max` — never over it, which is what callers
/// budgeting a fixed rect need.
pub fn elide(text: &str, max: usize) -> String {
    if width_at_most(text, max) {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// [`elide`] from the *front*, keeping the tail — what a long path
/// wants, since where you are matters more than the root.
pub fn elide_front(text: &str, max: usize) -> String {
    if width_at_most(text, max) {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut keep = 0usize; // chars kept from the end
    let mut used = 0usize;
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        used += w;
        keep += 1;
    }
    let skip = text.chars().count() - keep;
    let mut out = String::from("…");
    out.extend(text.chars().skip(skip));
    out
}

/// Whether `text` fits in `max` columns, without summing past the cap.
fn width_at_most(text: &str, max: usize) -> bool {
    let mut used = 0usize;
    for c in text.chars() {
        used += c.width().unwrap_or(0);
        if used > max {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_wide_and_zero_width() {
        assert_eq!(text_cells("abc"), 3.0);
        assert_eq!(text_cells("привет"), 6.0);
        assert_eq!(text_cells("日本語"), 6.0);
        assert_eq!(text_cells("🙂"), 2.0);
        // A combining mark rides on its base and claims no column.
        assert_eq!(text_cells("e\u{0301}"), 1.0);
    }

    #[test]
    fn prefix_measures_chars_not_columns() {
        // Two chars in is four columns for wide glyphs.
        assert_eq!(prefix_cells("日本語", 2), 4.0);
        assert_eq!(prefix_cells("abc", 2), 2.0);
    }

    #[test]
    fn elide_keeps_what_fits_untouched() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("exactly-10", 10), "exactly-10");
        assert_eq!(elide_front("short", 10), "short");
        // Ten columns, five glyphs — measured, not counted.
        assert_eq!(elide("日本語です", 10), "日本語です");
    }

    #[test]
    fn elide_never_exceeds_the_budget() {
        for max in 0..12 {
            for s in ["abcdefghij", "日本語です五", "a日b語c", "привет"] {
                for out in [elide(s, max), elide_front(s, max)] {
                    assert!(
                        text_cells(&out) <= max as f32,
                        "{s:?} at {max} gave {out:?} ({} cells)",
                        text_cells(&out)
                    );
                }
            }
        }
    }

    #[test]
    fn a_wide_glyph_is_dropped_rather_than_split() {
        // Budget 4 leaves 3 columns for text; a third wide glyph would
        // need a fourth, so two are kept and the result is 5 - 1 wide.
        let out = elide("日本語です", 4);
        assert_eq!(out, "日…");
        assert_eq!(text_cells(&out), 3.0);
        // The narrow case fills the budget exactly.
        assert_eq!(elide("abcdefgh", 4), "abc…");
    }

    #[test]
    fn elide_front_keeps_the_tail() {
        assert_eq!(elide_front("/a/very/long/path", 6), "…/path");
        let out = elide_front("先頭を捨てる", 5);
        assert!(out.starts_with('…') && out.ends_with("てる"), "{out}");
        assert!(text_cells(&out) <= 5.0);
    }

    #[test]
    fn a_budget_of_one_is_just_the_marker() {
        assert_eq!(elide("abcdef", 1), "…");
        assert_eq!(elide_front("abcdef", 1), "…");
        // Nothing fits in nothing — not even the marker.
        assert_eq!(elide("abcdef", 0), "");
        assert_eq!(elide_front("abcdef", 0), "");
    }

    #[test]
    fn a_full_prefix_matches_the_whole_string() {
        for s in ["", "abc", "привет", "日本語", "a日b", "e\u{0301}x"] {
            let n = s.chars().count();
            assert_eq!(prefix_cells(s, n), text_cells(s), "{s:?}");
            // Past the end clamps rather than panicking.
            assert_eq!(prefix_cells(s, n + 5), text_cells(s), "{s:?}");
        }
    }
}
