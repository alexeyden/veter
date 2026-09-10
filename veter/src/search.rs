//! Scrollback search backend for the host search overlay
//! (`App::search`). Pure text extraction + substring matching against
//! either the host or a focused-leaf portal parser — no rendering,
//! no key handling, no scroll mutation.
//!
//! v1 semantics:
//! * Substring (literal), not regex.
//! * Case-insensitive folding is ASCII-only — non-ASCII bytes match
//!   case-sensitively.
//! * Query matches don't cross row boundaries. A query that visually
//!   spans a soft-wrap won't match — covers the common case, ducks the
//!   wrap-spanning row→col split that would otherwise complicate the
//!   highlight renderer. ([`MatchSpan`] can still describe a multi-row
//!   span, and [`crate::hints`] produces them: a wrapped URL has to come
//!   back whole or not at all.)
//! * Empty cells in a row are treated as space (`' '`) so leading
//!   indentation is searchable; trailing spaces are stripped — except on
//!   a soft-wrapped row, where they are content the next row continues
//!   from (see [`IndexedRow::wrapped`]).

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
    /// This row soft-wraps into the next one, i.e. the two are halves of
    /// one logical line. [`crate::hints`] joins such runs before
    /// scanning, so a URL broken by the right margin is detected whole.
    /// Trailing spaces are left on a wrapped row for the same reason:
    /// stripping them would silently shorten the joined text and shift
    /// every column mapping after it.
    pub wrapped: bool,
}

/// A span of cells in a target parser's absolute scrollback coords.
///
/// Query matches are always single-row (`end_line == line`); hint spans
/// may cover a soft-wrap chain, in which case the covered cells are
/// `col_start..cols` on `line`, all of every row between, and
/// `0..col_end` on `end_line`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchSpan {
    pub line: i64,
    pub col_start: u16,
    /// Row the span ends on. Equal to `line` for a single-row span.
    pub end_line: i64,
    /// Exclusive end-cell column, on `end_line`. For a single-row span
    /// always `> col_start` (zero-width matches are dropped to keep the
    /// highlight visible).
    pub col_end: u16,
}

impl MatchSpan {
    /// A span confined to one row.
    pub fn row(line: i64, col_start: u16, col_end: u16) -> Self {
        Self {
            line,
            col_start,
            end_line: line,
            col_end,
        }
    }
}

/// Walk every row currently in `screen`'s buffer (scrollback + live
/// screen) and return a per-row index.
///
/// `top_of_live_screen` is the engine-tracked anchor for `screen`
/// (host: `prt.top_of_live_screen()`; portal:
/// `portal.children.top_of_live_screen()`), same convention as
/// `extract_text_from_parser` in main.rs.
pub fn extract_indexed_text(screen: &vt100::Screen, top_of_live_screen: i64) -> TextIndex {
    let (rows, _) = screen.size();
    let fill = screen.scrollback_fill();
    extract_lines(
        screen,
        top_of_live_screen,
        top_of_live_screen - fill as i64,
        top_of_live_screen + rows as i64 - 1,
    )
}

/// How far [`extract_indexed_window`] will chase a soft-wrap chain past
/// the range it was asked for. One logical line can in principle wrap
/// over the whole buffer, and indexing all of it would give back
/// exactly the cost the windowed scan exists to avoid.
const WRAP_CHAIN_LIMIT: usize = 256;

/// Index the lines `first..=last` only, extended outward over a
/// soft-wrap chain either end lands inside.
///
/// Hint mode's labels describe what is on screen and nothing else, so
/// it indexes one viewport rather than the whole buffer — which is what
/// makes rescanning on every scroll step affordable. The extension is
/// not optional: [`crate::hints`] joins a wrap chain into one logical
/// line before scanning, so a URL cut by the viewport edge has to
/// arrive whole or it is detected as something else, or not at all.
pub fn extract_indexed_window(
    screen: &vt100::Screen,
    top_of_live_screen: i64,
    first: i64,
    last: i64,
) -> TextIndex {
    let (rows, _) = screen.size();
    let fill = screen.scrollback_fill();
    let oldest = top_of_live_screen - fill as i64;
    let newest = top_of_live_screen + rows as i64 - 1;
    if newest < oldest || last < first {
        return TextIndex { rows: Vec::new() };
    }
    let mut first = first.clamp(oldest, newest);
    let mut last = last.clamp(oldest, newest);
    let wrapped_at = |line: i64| {
        locate_line(top_of_live_screen, fill, rows, line)
            .is_some_and(|(offset, row)| screen.row_wrapped_at(offset, row))
    };
    // Back to the head of the chain `first` sits in: the row above is
    // part of it exactly when that row wraps into this one.
    for _ in 0..WRAP_CHAIN_LIMIT {
        if first <= oldest || !wrapped_at(first - 1) {
            break;
        }
        first -= 1;
    }
    // Forward to the tail of the chain `last` sits in.
    for _ in 0..WRAP_CHAIN_LIMIT {
        if last >= newest || !wrapped_at(last) {
            break;
        }
        last += 1;
    }
    extract_lines(screen, top_of_live_screen, first, last)
}

/// Index every line in `first..=last` that is still in the buffer.
///
/// Rows are read at an explicit view offset rather than by moving the
/// grid's own scroll position: a portal buffer can be shared by two
/// views at different offsets (`portal-extension.md` §6.9) and its grid
/// stays live, so nothing here may move it. Reading a row as a slice
/// also costs one visible-row walk per *row* instead of one per cell —
/// the same reason `RowDamage::from_screen` does it that way.
fn extract_lines(
    screen: &vt100::Screen,
    top_of_live_screen: i64,
    first: i64,
    last: i64,
) -> TextIndex {
    let (rows, cols) = screen.size();
    let fill = screen.scrollback_fill();
    let mut indexed = Vec::with_capacity((last - first + 1).max(0) as usize);
    for line in first..=last {
        let Some((offset, row)) = locate_line(top_of_live_screen, fill, rows, line) else {
            continue;
        };
        indexed.push(index_row(screen, offset, row, cols, line));
    }
    TextIndex { rows: indexed }
}

/// View offset + visible row that absolute line `line` sits at, or
/// `None` when it has fallen out of the buffer. A line older than the
/// scrollback fill clamps to the oldest offset and lands on a negative
/// row, which is how it reports "gone".
fn locate_line(
    top_of_live_screen: i64,
    fill: usize,
    rows: u16,
    line: i64,
) -> Option<(usize, u16)> {
    let offset = (top_of_live_screen - line).clamp(0, fill as i64);
    let row = line - (top_of_live_screen - offset);
    (row >= 0 && row < rows as i64).then_some((offset as usize, row as u16))
}

fn index_row(
    screen: &vt100::Screen,
    offset: usize,
    row: u16,
    cols: u16,
    line: i64,
) -> IndexedRow {
    let wrapped = screen.row_wrapped_at(offset, row);
    let cells = screen.visible_row_cells_at(offset, row);
    let mut text = String::with_capacity(cols as usize);
    let mut text_lower: Vec<u8> = Vec::with_capacity(cols as usize);
    let mut byte_to_col: Vec<u16> = Vec::with_capacity(cols as usize + 1);

    let mut col: u16 = 0;
    while col < cols {
        let Some(cell) = cells.get(usize::from(col)) else { break };
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

    // A wrapped row keeps its trailing spaces: the logical line runs on
    // into the next row, so those cells sit *inside* the joined text and
    // dropping them would splice two halves that aren't adjacent.
    if !wrapped {
        while text.ends_with(' ') {
            text.pop();
            text_lower.pop();
            byte_to_col.pop();
        }
    }

    IndexedRow { line, text, text_lower, byte_to_col, wrapped }
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
                spans.push(MatchSpan::row(row.line, col_start, col_end));
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
        let idx = extract_indexed_text(p.screen(), 0);
        // 4 rows total; trailing-empty rows trim to "" and stay.
        assert_eq!(idx.rows.len(), 4);
        assert_eq!(idx.rows[0].text, "hello world");
        assert_eq!(idx.rows[1].text, "rust");
        assert_eq!(idx.rows[2].text, "");
        assert_eq!(idx.rows[3].text, "");
    }

    /// The wrap flag is what lets `hints` rejoin a logical line, so it
    /// has to survive extraction. A row that ran into the right margin
    /// is flagged; the row that ends the chain is not.
    #[test]
    fn extract_records_the_wrap_flag() {
        // 12 chars into a 5-column screen: rows 0 and 1 wrap, row 2 ends
        // the chain.
        let mut p = parse(b"abcdefghijkl", 4, 5);
        let idx = extract_indexed_text(p.screen(), 0);
        assert_eq!(idx.rows[0].text, "abcde");
        assert!(idx.rows[0].wrapped);
        assert!(idx.rows[1].wrapped);
        assert_eq!(idx.rows[2].text, "kl");
        assert!(!idx.rows[2].wrapped);
    }

    #[test]
    fn byte_to_col_sentinel_is_text_len() {
        let mut p = parse(b"abc", 2, 10);
        let idx = extract_indexed_text(p.screen(), 0);
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
        let idx = extract_indexed_text(p.screen(), 0);
        let m = find_matches(&idx, "hello", false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 0);
        assert_eq!(m[0].col_start, 6);
        assert_eq!(m[0].col_end, 11);
    }

    #[test]
    fn find_substring_case_insensitive() {
        let mut p = parse(b"Hello hello HELLO", 2, 20);
        let idx = extract_indexed_text(p.screen(), 0);
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
        let idx = extract_indexed_text(p.screen(), 0);
        assert!(find_matches(&idx, "foobar", true).is_empty());
        assert_eq!(find_matches(&idx, "foo", true).len(), 1);
        assert_eq!(find_matches(&idx, "bar", true).len(), 1);
    }

    #[test]
    fn empty_query_returns_no_matches() {
        let mut p = parse(b"anything", 2, 10);
        let idx = extract_indexed_text(p.screen(), 0);
        assert!(find_matches(&idx, "", true).is_empty());
    }

    /// A query may hold spaces — the row text keeps them, so a
    /// multi-word phrase matches like any other substring. (Typing one
    /// is the part that needed fixing: winit reports the space bar as
    /// `NamedKey::Space`, so the search bar's character path never saw
    /// it; see `App::handle_search_key_input`.)
    #[test]
    fn find_phrase_with_spaces() {
        let mut p = parse(b"the quick brown fox\r\nthe quick red fox", 3, 30);
        let idx = extract_indexed_text(p.screen(), 0);
        let m = find_matches(&idx, "quick brown", true);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 0);
        assert_eq!(m[0].col_start, 4);
        assert_eq!(m[0].col_end, 15);
        // Both rows share the shorter phrase.
        assert_eq!(find_matches(&idx, "the quick", true).len(), 2);
        // Trailing space narrows nothing away mid-line.
        assert_eq!(find_matches(&idx, "fox ", true).len(), 0);
    }

    /// The scrollback part of the buffer is indexed at absolute line
    /// coords, without moving the grid's own scroll position — a
    /// shared portal buffer must stay live while a search reads it.
    #[test]
    fn extract_reads_history_without_moving_the_grid() {
        // 6 lines through a 2-row screen: 4 fall into scrollback, and
        // `top_of_live_screen` is 4.
        let mut p = parse(b"l0\r\nl1\r\nl2\r\nl3\r\nl4\r\nl5", 2, 10);
        let top = p.screen().top_of_live_screen();
        assert_eq!(top, 4);
        let idx = extract_indexed_text(p.screen(), top);
        let texts: Vec<&str> = idx.rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["l0", "l1", "l2", "l3", "l4", "l5"]);
        assert_eq!(idx.rows[0].line, 0);
        assert_eq!(idx.rows[5].line, 5);
        assert_eq!(p.screen().scrollback(), 0, "the grid must not have moved");
    }

    /// Hint mode indexes one viewport, so a window has to hold exactly
    /// the lines asked for — that is the whole saving.
    #[test]
    fn window_indexes_only_the_lines_asked_for() {
        let mut p = parse(b"l0\r\nl1\r\nl2\r\nl3\r\nl4\r\nl5", 2, 10);
        let top = p.screen().top_of_live_screen();
        let idx = extract_indexed_window(p.screen(), top, 2, 3);
        let texts: Vec<&str> = idx.rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["l2", "l3"]);
        assert_eq!(idx.rows[0].line, 2);
    }

    /// A window that cuts a soft-wrap chain reaches out to both of its
    /// ends: `hints` joins the chain before scanning, so half a URL
    /// would be detected as something else, or not at all.
    #[test]
    fn window_extends_over_a_wrap_chain() {
        // cols=4, so "abcdefghij" wraps over rows 0..2, then "tail".
        let mut p = parse(b"abcdefghij\r\ntail", 2, 4);
        let top = p.screen().top_of_live_screen();
        // Ask for the middle row of the chain alone.
        let idx = extract_indexed_window(p.screen(), top, 1, 1);
        let texts: Vec<&str> = idx.rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["abcd", "efgh", "ij"]);
        // The row after the chain is not dragged in with it.
        assert!(!idx.rows.last().unwrap().wrapped);
    }

    /// A window clamps to what the buffer holds rather than indexing
    /// lines that have been evicted or not yet written.
    #[test]
    fn window_clamps_to_the_buffer() {
        let mut p = parse(b"l0\r\nl1\r\nl2", 2, 10);
        let top = p.screen().top_of_live_screen();
        let idx = extract_indexed_window(p.screen(), top, -50, 50);
        let texts: Vec<&str> = idx.rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["l0", "l1", "l2"]);
    }

    #[test]
    fn wide_char_col_accounting() {
        // "あい" — two wide chars, each occupies 2 cells. "あ" is 3
        // bytes in UTF-8. Match on "い" should report col 2..4.
        let mut p = parse("あい".as_bytes(), 2, 10);
        let idx = extract_indexed_text(p.screen(), 0);
        let m = find_matches(&idx, "い", false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].col_start, 2);
        assert_eq!(m[0].col_end, 4);
    }
}
