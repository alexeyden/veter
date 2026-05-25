//! Scrollback search backend for the host search overlay
//! (`App::search`). Pure text extraction + substring matching against
//! either the host or a focused-leaf portal parser — no rendering,
//! no key handling, no scroll mutation.
//!
//! v1 semantics:
//! * Substring (literal), not regex.
//! * Case-insensitive folding is ASCII-only — non-ASCII bytes match
//!   case-sensitively.
//! * Matches don't cross row boundaries. A query that visually spans
//!   a soft-wrap won't match — covers the common case, ducks the
//!   wrap-spanning row→col split that would otherwise complicate the
//!   highlight renderer.
//! * Empty cells in a row are treated as space (`' '`) so leading
//!   indentation is searchable; trailing spaces are stripped.

use vt100::Callbacks;

/// Per-row indexed text + byte→col map. Built once per search
/// session (cached on [`crate::SearchState`]) and re-queried per
/// keystroke via [`find_matches`]; rebuilt only when the target
/// parser advances (see step 3).
pub struct TextIndex {
    pub rows: Vec<IndexedRow>,
}

pub struct IndexedRow {
    /// Absolute scrollback line index of this row, in the target
    /// parser's coord space (same as `top_of_live_screen + visible_row`
    /// at extraction time). Match jumps use this directly with
    /// `set_scrollback(top - line)`.
    pub line: i64,
    /// Row contents as one string (trailing spaces stripped).
    pub text: String,
    /// ASCII-folded bytes byte-for-byte parallel to `text.as_bytes()`.
    /// Used as the haystack in case-insensitive mode so per-keystroke
    /// matching is a raw byte compare.
    pub text_lower: Vec<u8>,
    /// For byte b in `[0, text.len()]`, `byte_to_col[b]` is the cell
    /// column where byte b lives (start of the cell whose UTF-8 bytes
    /// include b). The sentinel `byte_to_col[text.len()]` is the
    /// column just past the last cell — used to compute end-of-match
    /// coords.
    pub byte_to_col: Vec<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchSpan {
    pub line: i64,
    pub col_start: u16,
    /// Exclusive end-cell column. Always `> col_start` (zero-width
    /// matches are dropped to keep the highlight visible).
    pub col_end: u16,
}

/// Walk every row currently in `parser`'s buffer (scrollback + live
/// screen) and return a per-row index. Restores the parser's
/// scrollback position before returning.
///
/// `top_of_live_screen` is the engine-tracked anchor for `parser`
/// (host: `prt.top_of_live_screen()`; portal:
/// `portal.children.top_of_live_screen()`), same convention as
/// `extract_text_from_parser` in main.rs.
pub fn extract_indexed_text<CB: Callbacks>(
    parser: &mut vt100::Parser<CB>,
    top_of_live_screen: i64,
) -> TextIndex {
    let (rows, cols) = parser.screen().size();
    let saved = parser.screen().scrollback();

    // Probe scrollback fill via the cap trick (mirrors main.rs:1887-1890):
    // set to MAX, observe the clamped value. This temporarily moves
    // the view, which the per-line walk below overwrites anyway —
    // single restore at the very end is enough.
    parser.screen_mut().set_scrollback(usize::MAX);
    let scrollback_fill = parser.screen().scrollback();

    let first_line = top_of_live_screen - scrollback_fill as i64;
    let last_line = top_of_live_screen + rows as i64 - 1;

    let mut indexed = Vec::with_capacity(scrollback_fill + rows as usize);

    for line in first_line..=last_line {
        let target_scrollback = (top_of_live_screen - line).max(0) as usize;
        parser.screen_mut().set_scrollback(target_scrollback);
        let actual = parser.screen().scrollback() as i64;
        let viewport_top = top_of_live_screen - actual;
        let row_in_view = line - viewport_top;
        if row_in_view < 0 || row_in_view >= rows as i64 {
            continue;
        }
        let row = row_in_view as u16;
        indexed.push(index_row(parser.screen(), row, cols, line));
    }

    parser.screen_mut().set_scrollback(saved);
    TextIndex { rows: indexed }
}

fn index_row(screen: &vt100::Screen, row: u16, cols: u16, line: i64) -> IndexedRow {
    let mut text = String::with_capacity(cols as usize);
    let mut text_lower: Vec<u8> = Vec::with_capacity(cols as usize);
    let mut byte_to_col: Vec<u16> = Vec::with_capacity(cols as usize + 1);

    let mut col: u16 = 0;
    while col < cols {
        let Some(cell) = screen.cell(row, col) else { break };
        if cell.is_wide_continuation() {
            // Belongs to the previous wide cell; its bytes were
            // already emitted there.
            col += 1;
            continue;
        }
        let s: &str = if cell.has_contents() { cell.contents() } else { " " };
        let start = text.len();
        text.push_str(s);
        for &b in s.as_bytes() {
            text_lower.push(b.to_ascii_lowercase());
        }
        for _ in start..text.len() {
            byte_to_col.push(col);
        }
        col += if cell.is_wide() { 2 } else { 1 };
    }
    byte_to_col.push(col);

    while text.ends_with(' ') {
        text.pop();
        text_lower.pop();
        byte_to_col.pop();
    }

    IndexedRow { line, text, text_lower, byte_to_col }
}

/// Search `index` for non-overlapping occurrences of `query`. Empty
/// query returns no matches. Order is row-by-row, oldest to newest;
/// within a row, left-to-right.
pub fn find_matches(
    index: &TextIndex,
    query: &str,
    case_insensitive: bool,
) -> Vec<MatchSpan> {
    if query.is_empty() {
        return Vec::new();
    }
    let pattern: Vec<u8> = if case_insensitive {
        query.as_bytes().iter().map(|b| b.to_ascii_lowercase()).collect()
    } else {
        query.as_bytes().to_vec()
    };
    let finder = memchr::memmem::Finder::new(&pattern);

    let mut spans = Vec::new();
    for row in &index.rows {
        let haystack: &[u8] = if case_insensitive {
            &row.text_lower
        } else {
            row.text.as_bytes()
        };
        if haystack.len() < pattern.len() {
            continue;
        }
        let mut search_start = 0;
        while search_start + pattern.len() <= haystack.len() {
            let Some(rel) = finder.find(&haystack[search_start..]) else {
                break;
            };
            let abs = search_start + rel;
            let end_byte = abs + pattern.len();
            // Drop matches landing on mid-UTF-8 bytes — they'd
            // collapse to a zero-width highlight and confuse the
            // user.
            if !row.text.is_char_boundary(abs) || !row.text.is_char_boundary(end_byte)
            {
                search_start = abs + 1;
                continue;
            }
            let col_start = row.byte_to_col[abs];
            let col_end = row.byte_to_col[end_byte];
            if col_end > col_start {
                spans.push(MatchSpan { line: row.line, col_start, col_end });
            }
            // Non-overlapping: advance past the match.
            search_start = end_byte;
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use vt100::Parser;

    fn parse(bytes: &[u8], rows: u16, cols: u16) -> Parser {
        let mut p = Parser::new(rows, cols, 100);
        p.process(bytes);
        p
    }

    #[test]
    fn extract_indexes_live_screen() {
        let mut p = parse(b"hello world\r\nrust", 4, 20);
        let idx = extract_indexed_text(&mut p, 0);
        // 4 rows total; trailing-empty rows trim to "" and stay.
        assert_eq!(idx.rows.len(), 4);
        assert_eq!(idx.rows[0].text, "hello world");
        assert_eq!(idx.rows[1].text, "rust");
        assert_eq!(idx.rows[2].text, "");
        assert_eq!(idx.rows[3].text, "");
    }

    #[test]
    fn byte_to_col_sentinel_is_text_len() {
        let mut p = parse(b"abc", 2, 10);
        let idx = extract_indexed_text(&mut p, 0);
        let row = &idx.rows[0];
        assert_eq!(row.text, "abc");
        // 3 byte entries + 1 sentinel.
        assert_eq!(row.byte_to_col.len(), 4);
        assert_eq!(row.byte_to_col[0], 0);
        assert_eq!(row.byte_to_col[3], 3);
    }

    #[test]
    fn find_substring_case_sensitive() {
        let mut p = parse(b"Hello hello HELLO", 2, 20);
        let idx = extract_indexed_text(&mut p, 0);
        let m = find_matches(&idx, "hello", false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 0);
        assert_eq!(m[0].col_start, 6);
        assert_eq!(m[0].col_end, 11);
    }

    #[test]
    fn find_substring_case_insensitive() {
        let mut p = parse(b"Hello hello HELLO", 2, 20);
        let idx = extract_indexed_text(&mut p, 0);
        let m = find_matches(&idx, "hello", true);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].col_start, 0);
        assert_eq!(m[1].col_start, 6);
        assert_eq!(m[2].col_start, 12);
    }

    #[test]
    fn matches_dont_cross_rows() {
        // "foobar" split across two physical rows shouldn't match.
        let mut p = parse(b"foo\r\nbar", 3, 5);
        let idx = extract_indexed_text(&mut p, 0);
        assert!(find_matches(&idx, "foobar", true).is_empty());
        assert_eq!(find_matches(&idx, "foo", true).len(), 1);
        assert_eq!(find_matches(&idx, "bar", true).len(), 1);
    }

    #[test]
    fn empty_query_returns_no_matches() {
        let mut p = parse(b"anything", 2, 10);
        let idx = extract_indexed_text(&mut p, 0);
        assert!(find_matches(&idx, "", true).is_empty());
    }

    #[test]
    fn wide_char_col_accounting() {
        // "あい" — two wide chars, each occupies 2 cells. "あ" is 3
        // bytes in UTF-8. Match on "い" should report col 2..4.
        let mut p = parse("あい".as_bytes(), 2, 10);
        let idx = extract_indexed_text(&mut p, 0);
        let m = find_matches(&idx, "い", false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].col_start, 2);
        assert_eq!(m[0].col_end, 4);
    }
}
