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
    fn a_full_prefix_matches_the_whole_string() {
        for s in ["", "abc", "привет", "日本語", "a日b", "e\u{0301}x"] {
            let n = s.chars().count();
            assert_eq!(prefix_cells(s, n), text_cells(s), "{s:?}");
            // Past the end clamps rather than panicking.
            assert_eq!(prefix_cells(s, n + 5), text_cells(s), "{s:?}");
        }
    }
}
