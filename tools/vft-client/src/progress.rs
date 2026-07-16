// Two progress UIs sharing a `ProgressUI` trait:
//
//   * `VgeProgress` — draws a single VGE element (a rounded track, a
//     rounded fill that grows, and a centred status text overlaid on
//     both) on the terminal grid. The bar is half a cell thick and
//     centred in its row, so it still occupies exactly one line.
//     Updates via `UpdateCommand` so each tick costs only the changed
//     command body, not the whole element.
//
//   * `AsciiProgress` — carriage-return-driven fallback for terminals
//     that did not advertise VGE in their probe response.
//
// Both write directly to stdout / stderr; the caller is responsible
// for any raw-TTY mode (writes use `\r\n` to behave correctly under
// raw mode, which still works under cooked mode).

use std::io::Write;
use std::time::Instant;

use anyhow::Result;

use vge_protocol::codec::Point;
use vge_protocol::command::{
    Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, Style, UpdateCommandBody,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_protocol::path::{PathNode, PathSegment};

/// Bar thickness in cells. The element still occupies a full 1-cell
/// row — the leftover height becomes padding split evenly above and
/// below, so surrounding output spacing is unchanged.
const BAR_H: f32 = 0.5;
/// Y offset of the bar within its row, centring `BAR_H` vertically.
const BAR_Y: f32 = (1.0 - BAR_H) / 2.0;
/// `origin.y` for the status text. The renderer places a `DrawText`
/// baseline at `origin.y * cell_h + ascent`
/// (`doc/vector-graphics-extension.md` §7.4), i.e. `origin.y` is the
/// *top of the text's cell row*, not the baseline. So `0.0` is what puts
/// the text inside the bar's own row, exactly where the terminal's own
/// grid text would sit. A nonzero fraction here reads like "nudge the
/// baseline down within the row" but actually adds a whole ascent on top
/// and pushes the label into the *next* row, where it collides with
/// whatever the caller prints after the bar.
const TEXT_Y: f32 = 0.0;

const TRACK_RGBA: Color = Color {
    r: 0.20,
    g: 0.22,
    b: 0.25,
    a: 1.0,
};
const FILL_RGBA: Color = Color {
    r: 0.30,
    g: 0.62,
    b: 1.00,
    a: 1.0,
};
const TEXT_RGBA: Color = Color {
    r: 0.97,
    g: 0.97,
    b: 0.97,
    a: 1.0,
};

// Document indices into the element's command list. Only FG and TEXT
// receive UpdateCommand calls; the background never moves, but the
// constant is here to make the layout obvious.
#[allow(dead_code)]
const CMD_IDX_BG: usize = 0;
const CMD_IDX_FG: usize = 1;
const CMD_IDX_TEXT: usize = 2;

pub trait ProgressUI {
    fn start(&mut self) -> Result<()>;
    /// `current` / `total` in bytes, `rate_bps` in bytes per second.
    /// `total = 0` means "unknown size" — the implementation should
    /// fall back to a count-only display.
    fn update(&mut self, current: u64, total: u64, rate_bps: f64) -> Result<()>;
    /// Remove the UI from the screen (e.g. delete the VGE element)
    /// without printing anything.
    ///
    /// Must be idempotent and safe on a UI that never started, so every
    /// exit path — including the error and Ctrl+C ones — can call it
    /// unconditionally. A VGE element the client forgets to delete
    /// outlives the process: it is host state, and nothing else ever
    /// collects it.
    fn teardown(&mut self) -> Result<()>;
    /// Tear the UI down and print `final_line` on its own row at the
    /// cursor. `\r\n` (not `\n`) because the caller holds the tty in raw
    /// mode, where `OPOST` is off and a bare `\n` would not return the
    /// carriage.
    fn finish(&mut self, final_line: &str) -> Result<()> {
        self.teardown()?;
        let mut out = std::io::stdout().lock();
        write!(out, "{final_line}\r\n")?;
        out.flush()?;
        Ok(())
    }
}

/// Adapter that suppresses an inner `ProgressUI` until the transfer
/// has been running for at least `delay`. Instant transfers
/// (localhost VMs, fast LANs, small files) never spawn a bar at
/// all; longer ones fall through to the inner UI as soon as the
/// threshold is crossed.
///
/// `start()` only records the timestamp; the actual `inner.start()`
/// runs lazily on the first `update()` past the threshold.
/// `finish()` prints just the final line when the bar never
/// materialised, or forwards to `inner.finish()` otherwise.
pub struct DelayedProgress<P: ProgressUI> {
    inner: P,
    delay: std::time::Duration,
    started_at: Option<Instant>,
    showing: bool,
    /// Latched by `teardown`. Without it a late `update` would sail past
    /// the `showing` check and `start()` the inner UI a second time,
    /// resurrecting a bar that was just deleted — and leaving that one
    /// on screen for good, since nothing tears it down twice.
    done: bool,
}

impl<P: ProgressUI> DelayedProgress<P> {
    pub fn new(inner: P, delay: std::time::Duration) -> Self {
        Self {
            inner,
            delay,
            started_at: None,
            showing: false,
            done: false,
        }
    }
}

impl<P: ProgressUI> ProgressUI for DelayedProgress<P> {
    fn start(&mut self) -> Result<()> {
        self.started_at = Some(Instant::now());
        Ok(())
    }

    fn teardown(&mut self) -> Result<()> {
        let was_showing = self.showing;
        self.showing = false;
        self.done = true;
        if !was_showing {
            // The bar never materialised, so there is nothing on screen
            // and — crucially — no VGE element on the host to delete.
            return Ok(());
        }
        self.inner.teardown()
    }

    fn update(&mut self, current: u64, total: u64, rate_bps: f64) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if !self.showing {
            let Some(t0) = self.started_at else {
                return Ok(());
            };
            if t0.elapsed() < self.delay {
                return Ok(());
            }
            self.inner.start()?;
            self.showing = true;
        }
        self.inner.update(current, total, rate_bps)
    }

    // `finish` is the default: teardown (a no-op when the bar never
    // materialised) then the final line, so the caller's output sequence
    // is uniform whether or not the threshold was crossed.
}

// ---- VGE progress ----------------------------------------------------

pub struct VgeProgress {
    element_id: String,
    /// Row at which the bar is drawn (0-indexed for VGE, derived from
    /// the 1-indexed DSR-CPR result minus one).
    origin_y: f32,
    bar_w: f32,
    /// Corner x-radius in cells. The y-radius is always `BAR_H / 2`
    /// (fully rounded pill ends); this is its width-compensated
    /// counterpart — see `corner_rx_cells`.
    corner_rx: f32,
    label: String,
    last_render: Option<Instant>,
    /// Set once `DeleteElement` has been emitted, so `teardown` is
    /// idempotent.
    torn_down: bool,
}

impl VgeProgress {
    pub fn new(
        element_id: String,
        label: String,
        cursor_row_1based: u32,
        term_cols: u32,
        cell_px: (u16, u16),
    ) -> Self {
        let bar_w = bar_width_cells(term_cols);
        Self {
            element_id,
            origin_y: cursor_row_1based.saturating_sub(1) as f32,
            bar_w: bar_w as f32,
            corner_rx: corner_rx_cells(cell_px.0, cell_px.1),
            label,
            last_render: None,
            torn_down: false,
        }
    }

    /// The bar's track (full width) and fill (`w` wide) share a shape;
    /// only the width differs.
    fn bar_path(&self, w: f32) -> Vec<PathSegment> {
        rounded_rect_path(0.0, BAR_Y, w, BAR_H, self.corner_rx, BAR_H / 2.0)
    }

    fn render_text(&self, current: u64, total: u64, rate_bps: f64) -> String {
        format_status(&self.label, current, total, rate_bps)
    }

    /// Emit VGE commands with `REQ_ID_NO_RESPONSE`.
    ///
    /// The bar is pure decoration: nothing here inspects a response, and
    /// `vsend` / `vrecv` discard every `HostFrame::Vge` they receive. A
    /// real request id would make the host ack *every* command
    /// (`VgeEngine::dispatch_frame` acks anything but this sentinel), so
    /// a 30 Hz bar bills the client two `RSP_OK` envelopes per tick —
    /// tens of thousands over a multi-minute transfer. Any of that
    /// backlog still unread when the client exits lands on the shell's
    /// stdin, where zsh's zle binds `ESC _` to `insert-last-word` and
    /// echoes the lot back as caret-notation garbage. Not generating the
    /// acks is the only way to be sure none leak. `vcat`'s upload bar
    /// uses the same sentinel for the same reason.
    fn write_envelope(&self, env: &[u8]) -> Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(env)?;
        out.flush()?;
        Ok(())
    }
}

impl VgeProgress {
    /// The `CreateElement` envelope `start` puts on the wire.
    fn create_envelope(&self) -> Vec<u8> {
        let bg = DrawCmd::FillPath {
            fill: Style::Flat(TRACK_RGBA),
            segments: self.bar_path(self.bar_w),
        };
        let fg = DrawCmd::FillPath {
            fill: Style::Flat(FILL_RGBA),
            segments: self.bar_path(0.0),
        };
        let text = DrawCmd::DrawText {
            origin: Point {
                x: self.bar_w / 2.0,
                y: TEXT_Y,
            },
            align: Align::Center,
            fill: Style::Flat(TEXT_RGBA),
            font_style: FontStyle::default(),
            text: self.render_text(0, 0, 0.0),
        };
        let create = Command::CreateElement(CreateElementBody {
            id: self.element_id.clone(),
            commands: vec![bg, fg, text],
            origin: Point {
                x: 0.0,
                y: self.origin_y,
            },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
            transform: None,
        });
        build_envelope(&[(create, REQ_ID_NO_RESPONSE)])
    }

    /// The paired `UpdateCommand` envelope `update` puts on the wire.
    fn update_envelope(&self, current: u64, total: u64, rate_bps: f64) -> Vec<u8> {
        let frac = if total == 0 {
            0.0
        } else {
            (current as f32 / total as f32).clamp(0.0, 1.0)
        };
        let fill_w = self.bar_w * frac;
        let fg_cmd = DrawCmd::FillPath {
            fill: Style::Flat(FILL_RGBA),
            segments: self.bar_path(fill_w),
        };
        let text_cmd = DrawCmd::DrawText {
            origin: Point {
                x: self.bar_w / 2.0,
                y: TEXT_Y,
            },
            align: Align::Center,
            fill: Style::Flat(TEXT_RGBA),
            font_style: FontStyle::default(),
            text: self.render_text(current, total, rate_bps),
        };
        let upd_fg = Command::UpdateCommand(UpdateCommandBody {
            id: self.element_id.clone(),
            index: CMD_IDX_FG,
            command: fg_cmd,
        });
        let upd_text = Command::UpdateCommand(UpdateCommandBody {
            id: self.element_id.clone(),
            index: CMD_IDX_TEXT,
            command: text_cmd,
        });
        build_envelope(&[
            (upd_fg, REQ_ID_NO_RESPONSE),
            (upd_text, REQ_ID_NO_RESPONSE),
        ])
    }

    /// The `DeleteElement` envelope `teardown` puts on the wire.
    fn delete_envelope(&self) -> Vec<u8> {
        let del = Command::DeleteElement {
            id: self.element_id.clone(),
        };
        build_envelope(&[(del, REQ_ID_NO_RESPONSE)])
    }
}

impl ProgressUI for VgeProgress {
    fn start(&mut self) -> Result<()> {
        // Hand the delete to the signal handler before drawing anything:
        // a SIGTERM/SIGHUP kills the process without unwinding, so this
        // is the only thing that can take the bar down on that path.
        crate::cancel::set_signal_cleanup_envelope(&self.delete_envelope());
        self.write_envelope(&self.create_envelope())?;
        // Move the cursor below the bar so subsequent stdout doesn't
        // overlap. \r\n covers raw + cooked modes.
        let mut out = std::io::stdout().lock();
        out.write_all(b"\r\n")?;
        out.flush()?;
        Ok(())
    }

    fn update(&mut self, current: u64, total: u64, rate_bps: f64) -> Result<()> {
        // Throttle to ~30 Hz so we don't flood the PTY with envelopes.
        let now = Instant::now();
        if let Some(prev) = self.last_render {
            if now.duration_since(prev).as_millis() < 33 && current < total {
                return Ok(());
            }
        }
        self.last_render = Some(now);
        self.write_envelope(&self.update_envelope(current, total, rate_bps))
    }

    fn teardown(&mut self) -> Result<()> {
        if self.torn_down {
            return Ok(());
        }
        self.torn_down = true;
        // The bar is coming down here; a signal arriving later must not
        // re-send a delete for an id that no longer exists.
        crate::cancel::clear_signal_cleanup_envelope();
        self.write_envelope(&self.delete_envelope())
    }
}

// ---- ASCII progress ---------------------------------------------------

pub struct AsciiProgress {
    label: String,
    bar_w: usize,
    last_render: Option<Instant>,
    last_line_len: usize,
}

impl AsciiProgress {
    pub fn new(label: String, term_cols: u32) -> Self {
        Self {
            label,
            bar_w: bar_width_cells(term_cols).clamp(20, 60) as usize,
            last_render: None,
            last_line_len: 0,
        }
    }
}

impl ProgressUI for AsciiProgress {
    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    fn update(&mut self, current: u64, total: u64, rate_bps: f64) -> Result<()> {
        let now = Instant::now();
        if let Some(prev) = self.last_render {
            if now.duration_since(prev).as_millis() < 100 && current < total {
                return Ok(());
            }
        }
        self.last_render = Some(now);

        let bar = render_ascii_bar(current, total, self.bar_w);
        let stats = format_status(&self.label, current, total, rate_bps);
        let line = format!("{bar} {stats}");
        // Carriage-return + erase-to-EOL so a shorter line cleanly
        // overwrites the previous one.
        let mut err = std::io::stderr().lock();
        write!(err, "\r{line}\x1b[K")?;
        err.flush()?;
        self.last_line_len = line.len();
        Ok(())
    }

    fn teardown(&mut self) -> Result<()> {
        // Erase the in-place bar line; `finish` then prints the final
        // line over the cleared row.
        let mut err = std::io::stderr().lock();
        write!(err, "\r\x1b[K")?;
        err.flush()?;
        self.last_line_len = 0;
        Ok(())
    }
}

// ---- shared helpers ---------------------------------------------------

/// Corner x-radius, in cells, that renders as a *visually circular*
/// arc given the terminal's cell aspect. Terminal cells are taller
/// than they are wide, so an arc `ry` cells tall needs to be
/// `ry * (cell_h / cell_w)` cells wide to come out round on screen
/// (see the `ArcEllipseTo` note in `vge_protocol::path`). Degenerate
/// probe values fall back to an isotropic radius.
fn corner_rx_cells(cell_px_w: u16, cell_px_h: u16) -> f32 {
    let ry = BAR_H / 2.0;
    if cell_px_w == 0 || cell_px_h == 0 {
        return ry;
    }
    ry * (f32::from(cell_px_h) / f32::from(cell_px_w))
}

/// Rounded-rectangle path, traced clockwise in y-down cell
/// coordinates. `rx` / `ry` are clamped to half the rect's extents so
/// a nearly-empty progress fill collapses into a smaller pill instead
/// of self-intersecting. A zero-extent rect yields no segments, which
/// draws nothing.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, rx: f32, ry: f32) -> Vec<PathSegment> {
    if w <= 0.0 || h <= 0.0 {
        return Vec::new();
    }
    let rx = rx.clamp(0.0, w / 2.0);
    let ry = ry.clamp(0.0, h / 2.0);
    // Quarter-turn corner arcs: never the large sweep, always
    // clockwise (sweep = true is the positive direction with y down).
    let corner = |dst: Point| PathNode::ArcEllipseTo {
        large: false,
        sweep: true,
        rx,
        ry,
        rotation: 0.0,
        dst,
    };
    vec![PathSegment {
        start: Point { x: x + rx, y },
        nodes: vec![
            PathNode::LineTo {
                dst: Point { x: x + w - rx, y },
            },
            corner(Point { x: x + w, y: y + ry }),
            PathNode::LineTo {
                dst: Point {
                    x: x + w,
                    y: y + h - ry,
                },
            },
            corner(Point {
                x: x + w - rx,
                y: y + h,
            }),
            PathNode::LineTo {
                dst: Point { x: x + rx, y: y + h },
            },
            corner(Point {
                x,
                y: y + h - ry,
            }),
            PathNode::LineTo {
                dst: Point { x, y: y + ry },
            },
            corner(Point { x: x + rx, y }),
            PathNode::ClosePath,
        ],
    }]
}

fn bar_width_cells(term_cols: u32) -> u32 {
    // Leave a 2-cell margin on each side; clamp to a maximum that
    // keeps the bar comfortable on wide screens.
    let usable = term_cols.saturating_sub(4);
    usable.clamp(20, 80)
}

fn render_ascii_bar(current: u64, total: u64, width: usize) -> String {
    let frac = if total == 0 {
        0.0
    } else {
        (current as f64 / total as f64).clamp(0.0, 1.0)
    };
    let filled = (frac * width as f64).round() as usize;
    let mut s = String::with_capacity(width + 2);
    s.push('[');
    for i in 0..width {
        if i < filled {
            s.push('#');
        } else {
            s.push('.');
        }
    }
    s.push(']');
    s
}

fn format_status(label: &str, current: u64, total: u64, rate_bps: f64) -> String {
    let pct = if total == 0 {
        String::new()
    } else {
        let p = (current as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
        format!("{p:>5.1}%  ")
    };
    let sizes = if total == 0 {
        format_bytes(current)
    } else {
        format!("{} / {}", format_bytes(current), format_bytes(total))
    };
    let rate = if rate_bps > 0.0 {
        format!("  {}", format_rate(rate_bps))
    } else {
        String::new()
    };
    format!("{label}  {pct}{sizes}{rate}")
}

fn format_bytes(b: u64) -> String {
    let f = b as f64;
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    if f >= GIB {
        format!("{:.2} GiB", f / GIB)
    } else if f >= MIB {
        format!("{:.1} MiB", f / MIB)
    } else if f >= KIB {
        format!("{:.1} KiB", f / KIB)
    } else {
        format!("{b} B")
    }
}

fn format_rate(bps: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bps >= MIB {
        format!("{:.1} MiB/s", bps / MIB)
    } else if bps >= KIB {
        format!("{:.1} KiB/s", bps / KIB)
    } else {
        format!("{bps:.0} B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_bar_rounds_correctly() {
        let s = render_ascii_bar(50, 100, 10);
        assert_eq!(s, "[#####.....]");
    }

    #[test]
    fn ascii_bar_zero_progress() {
        assert_eq!(render_ascii_bar(0, 100, 10), "[..........]");
    }

    #[test]
    fn ascii_bar_complete() {
        assert_eq!(render_ascii_bar(100, 100, 10), "[##########]");
    }

    #[test]
    fn ascii_bar_unknown_total() {
        // total = 0 short-circuits to 0% rendering.
        assert_eq!(render_ascii_bar(123, 0, 5), "[.....]");
    }

    #[test]
    fn format_bytes_picks_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }

    #[test]
    fn format_status_includes_pct_when_total_known() {
        let s = format_status("vsend: foo", 50, 100, 1024.0 * 1024.0);
        assert!(s.contains("50.0%"));
        assert!(s.contains("MiB"));
    }

    #[test]
    fn format_status_omits_pct_when_total_unknown() {
        let s = format_status("vsend: foo", 50, 0, 0.0);
        assert!(!s.contains('%'));
    }

    #[test]
    fn bar_is_half_height_and_vertically_centred() {
        // The thin bar must stay centred in a full 1-cell row: equal
        // padding above and below, so surrounding output spacing is
        // unchanged.
        assert_eq!(BAR_H, 0.5);
        assert_eq!(BAR_Y, 0.25);
        let pad_above = BAR_Y;
        let pad_below = 1.0 - (BAR_Y + BAR_H);
        assert_eq!(pad_above, pad_below);
        assert_eq!(BAR_Y + BAR_H + pad_below, 1.0, "row must total 1 cell");
    }

    #[test]
    fn corners_are_circular_in_pixel_space() {
        // The point of the aspect compensation: whatever the cell
        // shape, the corner must have the same radius *in pixels* on
        // both axes — that's what makes it read as round rather than
        // as a stretched ellipse. Cell-space radii differing is the
        // expected means, not the end.
        for (cw, ch) in [(10u16, 20u16), (8, 17), (10, 10), (12, 24)] {
            let rx_px = corner_rx_cells(cw, ch) * f32::from(cw);
            let ry_px = (BAR_H / 2.0) * f32::from(ch);
            assert!(
                (rx_px - ry_px).abs() < 1e-3,
                "cell {cw}x{ch}: corner is {rx_px}px wide but {ry_px}px tall"
            );
        }
    }

    #[test]
    fn corner_rx_survives_degenerate_probe_values() {
        // A terminal reporting zero cell dims must not produce NaN/inf
        // geometry; fall back to an isotropic radius instead.
        assert_eq!(corner_rx_cells(0, 20), BAR_H / 2.0);
        assert_eq!(corner_rx_cells(10, 0), BAR_H / 2.0);
        assert!(corner_rx_cells(0, 0).is_finite());
    }

    #[test]
    fn rounded_rect_is_closed_and_pill_capped() {
        let segs = rounded_rect_path(0.0, BAR_Y, 40.0, BAR_H, 0.5, BAR_H / 2.0);
        assert_eq!(segs.len(), 1);
        let nodes = &segs[0].nodes;
        assert!(matches!(nodes.last(), Some(PathNode::ClosePath)));
        // Four quarter-turn corners, none of them a large sweep.
        let arcs: Vec<_> = nodes
            .iter()
            .filter(|n| matches!(n, PathNode::ArcEllipseTo { .. }))
            .collect();
        assert_eq!(arcs.len(), 4);
        for a in arcs {
            let PathNode::ArcEllipseTo { large, sweep, ry, .. } = a else {
                unreachable!()
            };
            assert!(!large);
            assert!(sweep, "corners trace clockwise in y-down coords");
            assert_eq!(*ry, BAR_H / 2.0, "fully rounded pill ends");
        }
    }

    #[test]
    fn rounded_rect_clamps_radius_on_narrow_fill() {
        // A progress fill narrower than two corner radii must collapse
        // to a smaller pill rather than self-intersect.
        let segs = rounded_rect_path(0.0, BAR_Y, 0.4, BAR_H, 0.5, BAR_H / 2.0);
        let PathNode::ArcEllipseTo { rx, .. } = segs[0]
            .nodes
            .iter()
            .find(|n| matches!(n, PathNode::ArcEllipseTo { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(*rx, 0.2, "rx clamped to half the width");
    }

    #[test]
    fn rounded_rect_empty_at_zero_progress() {
        // Zero-width fill (0% progress) draws nothing at all.
        assert!(rounded_rect_path(0.0, BAR_Y, 0.0, BAR_H, 0.5, 0.25).is_empty());
        assert!(rounded_rect_path(0.0, BAR_Y, -1.0, BAR_H, 0.5, 0.25).is_empty());
    }

    #[test]
    fn bar_path_survives_wire_roundtrip() {
        // The geometry helpers above test our own maths; this proves
        // the resulting arc path actually encodes and re-parses through
        // the real VGE codec, so the host renders what we intended.
        use vge_protocol::command::parse;
        use vge_protocol::encode::{encode_command, frame_type_for};

        let bar = VgeProgress::new("p".into(), "vsend: f".into(), 5, 80, (10, 20));
        let segments = bar.bar_path(bar.bar_w * 0.45);
        let cmd = Command::CreateElement(CreateElementBody {
            id: "p".into(),
            commands: vec![DrawCmd::FillPath {
                fill: Style::Flat(FILL_RGBA),
                segments: segments.clone(),
            }],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
            transform: None,
        });
        let body = encode_command(&cmd);
        let parsed = parse(frame_type_for(&cmd), &body).expect("bar path must re-parse");

        // Assert on geometry only: `Color` quantises to u8 on the wire,
        // so the round trip is intentionally lossy for the fill and
        // comparing it would test the codec, not the bar.
        let Command::CreateElement(body) = parsed else {
            panic!("expected CreateElement")
        };
        let [DrawCmd::FillPath { segments: got, .. }] = &body.commands[..] else {
            panic!("expected a single FillPath")
        };
        assert_eq!(*got, segments, "bar geometry must survive the wire");
    }

    #[test]
    fn bar_width_clamps_to_minimum() {
        assert_eq!(bar_width_cells(10), 20);
    }

    #[test]
    fn bar_width_clamps_to_maximum() {
        assert_eq!(bar_width_cells(500), 80);
    }

    /// In-memory ProgressUI for DelayedProgress tests: records every
    /// start/update/finish call so we can assert what flowed through.
    #[derive(Default)]
    struct Recorder {
        starts: u32,
        updates: Vec<(u64, u64, f64)>,
        teardowns: u32,
    }

    impl ProgressUI for Recorder {
        fn start(&mut self) -> Result<()> {
            self.starts += 1;
            Ok(())
        }
        fn update(&mut self, c: u64, t: u64, r: f64) -> Result<()> {
            self.updates.push((c, t, r));
            Ok(())
        }
        fn teardown(&mut self) -> Result<()> {
            self.teardowns += 1;
            Ok(())
        }
    }

    #[test]
    fn delayed_progress_suppresses_inner_when_under_threshold() {
        use std::time::Duration;
        let mut d = DelayedProgress::new(Recorder::default(), Duration::from_secs(10));
        d.start().unwrap();
        d.update(50, 100, 1024.0).unwrap();
        d.update(100, 100, 1024.0).unwrap();
        d.finish("done").unwrap();
        assert_eq!(d.inner.starts, 0, "inner.start should not run for fast transfers");
        assert!(d.inner.updates.is_empty());
        assert_eq!(
            d.inner.teardowns, 0,
            "nothing was drawn, so there is nothing to tear down"
        );
    }

    #[test]
    fn delayed_progress_falls_through_after_threshold() {
        use std::thread::sleep;
        use std::time::Duration;
        let mut d = DelayedProgress::new(Recorder::default(), Duration::from_millis(20));
        d.start().unwrap();
        d.update(10, 100, 0.0).unwrap();
        assert_eq!(d.inner.starts, 0, "still under the threshold");
        sleep(Duration::from_millis(30));
        d.update(50, 100, 0.0).unwrap();
        assert_eq!(d.inner.starts, 1, "threshold crossed; inner.start should run");
        assert_eq!(d.inner.updates.len(), 1, "first post-threshold update forwarded");
        d.update(100, 100, 0.0).unwrap();
        d.finish("done").unwrap();
        assert_eq!(d.inner.updates.len(), 2);
        assert_eq!(d.inner.teardowns, 1, "a revealed bar must be torn down");
    }

    #[test]
    fn teardown_is_idempotent_once_shown() {
        // Every exit path calls teardown unconditionally, and some call
        // it twice (an explicit teardown, then a finish). The second
        // call must not emit a second DeleteElement.
        use std::thread::sleep;
        use std::time::Duration;
        let mut d = DelayedProgress::new(Recorder::default(), Duration::from_millis(1));
        d.start().unwrap();
        sleep(Duration::from_millis(5));
        d.update(10, 100, 0.0).unwrap();
        d.teardown().unwrap();
        d.teardown().unwrap();
        d.finish("done").unwrap();
        assert_eq!(d.inner.teardowns, 1, "teardown must collapse to one delete");

        // A late update must not redraw the bar we just deleted.
        let before = d.inner.starts;
        d.update(90, 100, 0.0).unwrap();
        assert_eq!(d.inner.starts, before, "a torn-down bar must stay down");
    }

    #[test]
    fn vge_progress_never_asks_the_host_for_a_response() {
        // The bar is decoration: no caller inspects a VGE response, and
        // an unread ack backlog is what garbles the shell on exit. Every
        // command it emits must carry the "apply but don't ack" sentinel.
        use vge_protocol::apc::ApcStream;
        use vge_protocol::codec::Reader;
        use vge_protocol::frame::MARKER_C2T;

        // Reach past `write_envelope` (which goes to the real stdout) and
        // assert on the encoder the way the host's parser would see it.
        let bar = VgeProgress::new("p".into(), "vsend: f".into(), 5, 80, (10, 20));
        let create = Command::CreateElement(CreateElementBody {
            id: "p".into(),
            commands: vec![DrawCmd::FillPath {
                fill: Style::Flat(FILL_RGBA),
                segments: bar.bar_path(bar.bar_w),
            }],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
            transform: None,
        });
        let del = Command::DeleteElement { id: "p".into() };
        let env = build_envelope(&[
            (create, REQ_ID_NO_RESPONSE),
            (del, REQ_ID_NO_RESPONSE),
        ]);

        let mut s = ApcStream::with_marker(*MARKER_C2T);
        let out = s.feed(&env);
        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        r.u8().unwrap(); // protocol version
        r.u32().unwrap(); // payload len
        let mut seen = 0;
        while !r.at_end() {
            let _frame_type = r.u8().unwrap();
            let request_id = r.u32().unwrap();
            let body_len = r.u32().unwrap() as usize;
            r.take(body_len).unwrap();
            assert_eq!(
                request_id, REQ_ID_NO_RESPONSE,
                "a progress command with a real request id makes the host ack it"
            );
            seen += 1;
        }
        assert_eq!(seen, 2);
    }

    /// The bug this guards: a cancelled `vsend` / a failed `vrecv` left
    /// the shell echoing screenfuls of `^[_vge^@^@…`. Those were the
    /// host's acks for the bar's own draw commands — one per command, at
    /// 30 Hz, tens of thousands over a long transfer — that nobody read
    /// before the client exited.
    ///
    /// Drive the *real* host engine with the bar's real on-wire bytes:
    /// a full transfer's worth of commands must leave the host with
    /// nothing to say back, while still actually drawing and removing
    /// the element.
    #[test]
    fn host_never_answers_the_bar() {
        use veter_host::vge::VgeEngine;

        let mut engine = VgeEngine::new((10, 20), 1.0);
        let mut bar = VgeProgress::new("vsend-progress-1".into(), "vsend: f".into(), 5, 80, (10, 20));

        engine.process_pty_chunk(&bar.create_envelope());
        assert!(
            engine.state.elements().contains_key("vsend-progress-1"),
            "the bar must still be drawn — quiet means unacked, not ignored"
        );
        assert!(
            engine.take_responses().is_empty(),
            "CreateElement drew an ack the client would have had to read"
        );

        for i in 0..=100u64 {
            engine.process_pty_chunk(&bar.update_envelope(i, 100, 1024.0));
        }
        assert!(
            engine.take_responses().is_empty(),
            "the 30 Hz update stream is what floods the shell; it must be silent"
        );

        bar.torn_down = false; // exercise the envelope teardown() emits
        engine.process_pty_chunk(&bar.delete_envelope());
        assert!(
            !engine.state.elements().contains_key("vsend-progress-1"),
            "teardown must actually remove the host-side element"
        );
        assert!(
            engine.take_responses().is_empty(),
            "DeleteElement is the last thing on the wire before exit; \
             its ack is the one most likely to land on the shell"
        );
    }

    #[test]
    fn status_text_sits_in_the_bar_row() {
        // The renderer puts a DrawText baseline at
        // `origin.y * cell_h + ascent`, so origin.y is the top of the
        // text's row. Any nonzero value pushes the label a whole row down
        // — onto the line the caller prints its own output on.
        assert_eq!(TEXT_Y, 0.0);
    }
}
