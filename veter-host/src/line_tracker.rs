//! Shared `top_of_live_screen` tracker for the VGE and PRT engines.
//!
//! Both engines anchor their objects (VGE elements, Scrollback
//! portals) to absolute scrollback line indices, where line 0 is the
//! first row the parser ever displayed. vt100 itself only knows
//! relative state, so the engines maintain the absolute index of the
//! live screen's first row here, advancing it by deltas of the
//! parser's signed `origin_shift` counter: +1 per line scrolled off
//! the top of the main grid (scroll-region scrolls excluded — they
//! don't move the screen relative to scrollback), +1 per row a
//! vertical shrink pushes into scrollback, -1 per row a vertical grow
//! pulls back out.
//!
//! The counter is exact — unlike the old probe-and-hash heuristic it
//! is immune to resize artifacts (a width change used to read as a
//! phantom scrollback eviction and shift every anchor up one row),
//! counts multi-line scrolls at saturated scrollback precisely, and
//! follows xterm-style push/pull vertical resizes so anchors stay
//! glued to their text lines.

pub(crate) struct LineTracker {
    /// Absolute scrollback line index of vt100's first live-screen row.
    pub(crate) top_of_live_screen: i64,
    /// `origin_shift` value at the previous `update` call.
    prev_shift: i64,
    /// `false` until the first `update` baselines `prev_shift`.
    /// Engines reset the tracker (RIS/DECSTR, snapshot restore,
    /// alt-screen return) and rely on the next `update` re-baselining
    /// against the parser without moving `top_of_live_screen`.
    initialized: bool,
}

impl LineTracker {
    pub(crate) fn new() -> Self {
        Self {
            top_of_live_screen: 0,
            prev_shift: 0,
            initialized: false,
        }
    }

    /// Advance `top_of_live_screen` by however far the parser's live
    /// screen moved (scrolls, resize pushes/pulls) since the previous
    /// call. Call after every `parser.process(...)` and after every
    /// `set_size`.
    pub(crate) fn update<CB: vt100::Callbacks>(
        &mut self,
        parser: &vt100::Parser<CB>,
    ) {
        let shift = parser.screen().origin_shift();
        if !self.initialized {
            self.prev_shift = shift;
            self.initialized = true;
            return;
        }
        self.top_of_live_screen += shift - self.prev_shift;
        self.prev_shift = shift;
    }

    /// Reset to initial state (RIS/DECSTR, alt-screen return) — line
    /// tracking re-baselines on the next `update`.
    pub(crate) fn clear(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_by_exact_scroll_count() {
        let mut tracker = LineTracker::new();
        let mut parser = vt100::Parser::new(5, 20, 100);
        tracker.update(&parser);
        // 12 CRLF-terminated lines on a 5-row screen: the cursor
        // reaches the bottom row after 4 line feeds, so the remaining
        // 8 each scroll one line into history.
        parser.process(&b"x\r\n".repeat(12));
        tracker.update(&parser);
        assert_eq!(tracker.top_of_live_screen, 8);
    }

    #[test]
    fn counts_scrolls_past_scrollback_saturation() {
        let mut tracker = LineTracker::new();
        // Tiny scrollback: 4-line cap saturates immediately.
        let mut parser = vt100::Parser::new(2, 10, 4);
        tracker.update(&parser);
        parser.process(&b"\n".repeat(20));
        tracker.update(&parser);
        // 20 LFs on a 2-row screen: the first moves the cursor to the
        // bottom row, every later one scrolls — 19 exactly, even
        // though only 4 scrollback rows survive.
        assert_eq!(tracker.top_of_live_screen, 19);
        assert_eq!(parser.screen().scrollback_fill(), 4);
    }

    #[test]
    fn width_resize_does_not_move_top_of_live_screen() {
        let mut tracker = LineTracker::new();
        let mut parser = vt100::Parser::new(10, 80, 100);
        parser.process(&b"line\r\n".repeat(15));
        tracker.update(&parser);
        let before = tracker.top_of_live_screen;

        // Width-only changes never move the screen origin (this was
        // the probe-and-hash heuristic's phantom-eviction bug).
        for cols in [70, 50, 90] {
            parser.screen_mut().set_size(10, cols);
            parser.process(b"$ ");
            tracker.update(&parser);
        }
        assert_eq!(tracker.top_of_live_screen, before);
    }

    #[test]
    fn vertical_resize_keeps_cursor_line_anchored() {
        let mut tracker = LineTracker::new();
        let mut parser = vt100::Parser::new(10, 80, 100);
        parser.process(&b"line\r\n".repeat(15));
        tracker.update(&parser);
        // The cursor's absolute line is the invariant a vcat image is
        // placed against; shrink + grow must preserve it throughout.
        let cursor_abs = |t: &LineTracker, p: &vt100::Parser<()>| {
            t.top_of_live_screen + i64::from(p.screen().cursor_position().0)
        };
        let anchor = cursor_abs(&tracker, &parser);

        parser.screen_mut().set_size(6, 80); // shrink: pushes rows
        tracker.update(&parser);
        assert_eq!(cursor_abs(&tracker, &parser), anchor);

        parser.screen_mut().set_size(12, 80); // grow: pulls them back
        tracker.update(&parser);
        assert_eq!(cursor_abs(&tracker, &parser), anchor);
    }

    #[test]
    fn shrink_grow_roundtrip_restores_top_of_live_screen() {
        let mut tracker = LineTracker::new();
        let mut parser = vt100::Parser::new(10, 80, 100);
        parser.process(&b"line\r\n".repeat(15));
        tracker.update(&parser);
        let before = tracker.top_of_live_screen;

        parser.screen_mut().set_size(6, 80);
        tracker.update(&parser);
        assert_eq!(tracker.top_of_live_screen, before + 4);

        parser.screen_mut().set_size(10, 80);
        tracker.update(&parser);
        assert_eq!(tracker.top_of_live_screen, before);
    }

    #[test]
    fn scroll_region_scrolls_do_not_count() {
        let mut tracker = LineTracker::new();
        let mut parser = vt100::Parser::new(10, 20, 100);
        tracker.update(&parser);
        // Restrict scrolling to rows 2..=5, park the cursor at the
        // region bottom, and force region scrolls.
        parser.process(b"\x1b[2;5r\x1b[5;1H");
        parser.process(&b"\n".repeat(6));
        tracker.update(&parser);
        assert_eq!(tracker.top_of_live_screen, 0);
    }
}
