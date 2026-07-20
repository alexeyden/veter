//! Bottom-centre tool palette and its per-tool options row.
//!
//! Layout and hit-testing come from one place: `layout()` produces the
//! hotspot list, and both rendering and click routing read it. A button
//! can't drift out of sync with the region that activates it, because
//! there is only one rectangle.
//!
//! Chrome is a top-level VGE element (no `parent`), so the canvas
//! camera transform never reaches it — the palette stays put and stays
//! the same size while the diagram pans and zooms underneath.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, DrawCmd, FontStyle, Style};
use vge_protocol::path::PathSegment;

use crate::tools::{
    COLORS, FILLS, LINE_TYPES, LineType, THICKNESSES, TOOLS, Tool, ToolState,
};

pub const CHROME_ID: &str = "chrome.bar";

const BTN_W: f32 = 5.0;
const BTN_H: f32 = 2.0;
const OPT_W: f32 = 4.0;
const SWATCH_W: f32 = 3.0;
const OPT_H: f32 = 2.0;
/// Blank cells between option groups, inside the options panel.
const GROUP_GAP: f32 = 2.0;
/// Blank rows between the palette and the options panel.
const PANEL_GAP: f32 = 1.0;
/// Corner radii, in pixels — converted per axis so they stay circular
/// on an anisotropic cell grid rather than turning into ovals.
const PANEL_RADIUS_PX: f32 = 7.0;
const PILL_RADIUS_PX: f32 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Tool(Tool),
    Thickness(f32),
    /// Stroke colour.
    Color(&'static str),
    /// Background colour; `"transparent"` means outline-only.
    Fill(&'static str),
    Line(LineType),
}

#[derive(Debug, Clone, Copy)]
pub struct Hotspot {
    pub rect: Rect,
    pub action: Action,
    pub active: bool,
}

impl Hotspot {
    fn contains(&self, col: f32, row: f32) -> bool {
        col >= self.rect.x
            && col < self.rect.x + self.rect.w
            && row >= self.rect.y
            && row < self.rect.y + self.rect.h
    }
}

pub struct Chrome {
    pub hotspots: Vec<Hotspot>,
    panels: Vec<Rect>,
}

impl Chrome {
    /// Which control, if any, is under a screen cell. Also answers
    /// "did this click land on chrome at all" — a `Some` means the
    /// canvas must not also act on the event.
    pub fn hit(&self, col: u16, row: u16) -> Option<Action> {
        let (c, r) = (col as f32, row as f32);
        self.hotspots
            .iter()
            .find(|h| h.contains(c, r))
            .map(|h| h.action)
    }

    /// True if the cell is anywhere over the chrome panels, including
    /// padding between buttons. Prevents a drag started on the palette
    /// background from panning the canvas.
    pub fn covers(&self, col: u16, row: u16) -> bool {
        let (c, r) = (col as f32, row as f32);
        self.panels
            .iter()
            .any(|p| c >= p.x && c < p.x + p.w && r >= p.y && r < p.y + p.h)
    }
}

/// Build the palette (and, when the tool has any, the options row) for a
/// terminal of `cols` x `rows`. The block is centred horizontally and
/// anchored to the bottom, above the status line.
pub fn layout(cols: u16, rows: u16, state: &ToolState) -> Chrome {
    let cols = cols as f32;
    let rows = rows as f32;
    let mut hotspots = Vec::new();
    let mut panels = Vec::new();

    let show_options = state.tool.has_options();
    // Stack upward from the status line at `rows - 1`. The options row
    // is reserved even when empty (Select), so the palette holds still
    // instead of hopping a row every time the tool changes.
    let options_top = rows - 1.0 - OPT_H;
    let palette_top = options_top - BTN_H - PANEL_GAP;

    // --- palette ---
    let palette_w = BTN_W * TOOLS.len() as f32;
    let palette_x = ((cols - palette_w) / 2.0).max(0.0).floor();
    panels.push(Rect {
        x: palette_x,
        y: palette_top,
        w: palette_w,
        h: BTN_H,
    });
    for (i, t) in TOOLS.into_iter().enumerate() {
        hotspots.push(Hotspot {
            rect: Rect {
                x: palette_x + i as f32 * BTN_W,
                y: palette_top,
                w: BTN_W,
                h: BTN_H,
            },
            action: Action::Tool(t),
            active: t == state.tool,
        });
    }

    // --- options ---
    if show_options {
        let show_line = state.tool.has_line_type();
        let show_fill = state.tool.has_fill();
        let thick_w = OPT_W * THICKNESSES.len() as f32;
        let line_w = if show_line {
            OPT_W * LINE_TYPES.len() as f32
        } else {
            0.0
        };
        let color_w = SWATCH_W * COLORS.len() as f32;
        let fill_w = if show_fill {
            SWATCH_W * FILLS.len() as f32
        } else {
            0.0
        };
        let groups = 2.0 + f32::from(show_line) + f32::from(show_fill);
        let total = thick_w + line_w + color_w + fill_w + GROUP_GAP * (groups - 1.0);
        let mut x = ((cols - total) / 2.0).max(0.0).floor();
        panels.push(Rect {
            x,
            y: options_top,
            w: total,
            h: OPT_H,
        });

        for t in THICKNESSES {
            hotspots.push(Hotspot {
                rect: Rect {
                    x,
                    y: options_top,
                    w: OPT_W,
                    h: OPT_H,
                },
                action: Action::Thickness(t),
                active: (t - state.thickness).abs() < f32::EPSILON,
            });
            x += OPT_W;
        }
        if show_line {
            x += GROUP_GAP;
            for lt in LINE_TYPES {
                hotspots.push(Hotspot {
                    rect: Rect {
                        x,
                        y: options_top,
                        w: OPT_W,
                        h: OPT_H,
                    },
                    action: Action::Line(lt),
                    active: lt == state.line_type,
                });
                x += OPT_W;
            }
        }
        x += GROUP_GAP;
        for c in COLORS {
            hotspots.push(Hotspot {
                rect: Rect {
                    x,
                    y: options_top,
                    w: SWATCH_W,
                    h: OPT_H,
                },
                action: Action::Color(c),
                active: c == state.color,
            });
            x += SWATCH_W;
        }
        if show_fill {
            x += GROUP_GAP;
            for c in FILLS {
                hotspots.push(Hotspot {
                    rect: Rect {
                        x,
                        y: options_top,
                        w: SWATCH_W,
                        h: OPT_H,
                    },
                    action: Action::Fill(c),
                    active: c == state.fill,
                });
                x += SWATCH_W;
            }
        }
    }

    Chrome { hotspots, panels }
}

// ------------------------------------------------------------ painting

fn rgba(r: u8, g: u8, b: u8, a: f32) -> Style {
    Style::Flat(Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    })
}

fn panel_bg() -> Style {
    rgba(28, 30, 36, 0.94)
}
fn panel_border() -> Style {
    rgba(255, 255, 255, 0.12)
}
fn active_bg() -> Style {
    rgba(52, 116, 214, 0.95)
}
fn ink() -> Style {
    rgba(214, 218, 226, 1.0)
}
fn ink_active() -> Style {
    rgba(255, 255, 255, 1.0)
}

/// Snap a cell-space coordinate so it lands on a whole pixel.
fn snap(v: f32, cell_px: f32) -> f32 {
    (v * cell_px).round() / cell_px
}

/// Pull a rect onto whole-pixel edges.
///
/// Chrome geometry is expressed in fractional cells (a 0.2-cell inset is
/// 3.4px), so every edge would otherwise land mid-pixel and be spread
/// across two pixel rows by anti-aliasing. Each corner lands at a
/// different fractional offset, which is what makes one corner of a
/// pill look worse than the other three. Unlike canvas shapes, chrome
/// is screen-fixed and never scaled, so snapping it is safe.
fn snap_rect(r: Rect, cell_w: f32, cell_h: f32) -> Rect {
    let x = snap(r.x, cell_w);
    let y = snap(r.y, cell_h);
    let x1 = snap(r.x + r.w, cell_w);
    let y1 = snap(r.y + r.h, cell_h);
    Rect {
        x,
        y,
        w: x1 - x,
        h: y1 - y,
    }
}

/// A rounded rectangle as a path. Radii come from a pixel measurement
/// and are converted per axis, so the corners read as circular arcs
/// rather than ovals on the anisotropic cell grid.
fn rounded_panel(r: Rect, radius_px: f32, cell_w: f32, cell_h: f32) -> Vec<PathSegment> {
    let r = snap_rect(r, cell_w, cell_h);
    crate::render::rounded_rect_path(
        r.x,
        r.y,
        r.x + r.w,
        r.y + r.h,
        radius_px / cell_w,
        radius_px / cell_h,
    )
}

pub fn draw(chrome: &Chrome, cell_w: f32, cell_h: f32) -> Vec<DrawCmd> {
    let mut cmds = Vec::new();

    for p in &chrome.panels {
        cmds.push(DrawCmd::OutlineFillPath {
            fill: panel_bg(),
            stroke: panel_border(),
            line_width: 0.06,
            segments: rounded_panel(*p, PANEL_RADIUS_PX, cell_w, cell_h),
        });
    }

    for h in &chrome.hotspots {
        if h.active {
            // Inset so adjacent active pills don't touch the panel edge.
            cmds.push(DrawCmd::FillPath {
                fill: active_bg(),
                segments: rounded_panel(
                    Rect {
                        x: h.rect.x + 0.2,
                        y: h.rect.y + 0.2,
                        w: h.rect.w - 0.4,
                        h: h.rect.h - 0.4,
                    },
                    PILL_RADIUS_PX,
                    cell_w,
                    cell_h,
                ),
            });
        }
        let fg = if h.active { ink_active() } else { ink() };
        let cx = h.rect.x + h.rect.w / 2.0;
        let cy = h.rect.y + h.rect.h / 2.0;
        match h.action {
            Action::Tool(t) => cmds.extend(tool_icon(t, cx, cy, fg, cell_w, cell_h)),
            Action::Thickness(px) => cmds.push(DrawCmd::DrawLines {
                stroke: fg,
                // The sample line shows the real width, converted the
                // same way `render.rs` converts it.
                line_width: (px / cell_w).max(0.05),
                lines: vec![(
                    Point { x: cx - 1.2, y: cy },
                    Point { x: cx + 1.2, y: cy },
                )],
            }),
            Action::Line(lt) => cmds.push(line_sample(lt, cx, cy, fg)),
            Action::Color(css) | Action::Fill(css) => {
                let r = Rect {
                    x: cx - 0.9,
                    y: cy - 0.45,
                    w: 1.8,
                    h: 0.9,
                };
                if css == "transparent" {
                    // No fill to show, so draw the empty box plus a
                    // diagonal — the usual "none" convention.
                    cmds.push(DrawCmd::DrawLineLoop {
                        stroke: fg.clone(),
                        line_width: 0.06,
                        points: vec![
                            Point { x: r.x, y: r.y },
                            Point { x: r.x + r.w, y: r.y },
                            Point {
                                x: r.x + r.w,
                                y: r.y + r.h,
                            },
                            Point { x: r.x, y: r.y + r.h },
                        ],
                    });
                    cmds.push(DrawCmd::DrawLines {
                        stroke: fg,
                        line_width: 0.06,
                        lines: vec![(
                            Point { x: r.x, y: r.y + r.h },
                            Point { x: r.x + r.w, y: r.y },
                        )],
                    });
                } else {
                    cmds.push(DrawCmd::OutlineFillRectangles {
                        fill: swatch_style(css),
                        stroke: fg,
                        line_width: 0.06,
                        rects: vec![r],
                    });
                }
            }
        }
    }

    cmds
}

fn swatch_style(css: &str) -> Style {
    let hex = css.strip_prefix('#').unwrap_or("000000");
    let v = u32::from_str_radix(hex, 16).unwrap_or(0);
    rgba(
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
        1.0,
    )
}

fn line_sample(lt: LineType, cx: f32, cy: f32, fg: Style) -> DrawCmd {
    let (x0, x1) = (cx - 1.3, cx + 1.3);
    match lt {
        LineType::Solid => DrawCmd::DrawLines {
            stroke: fg,
            line_width: 0.12,
            lines: vec![(Point { x: x0, y: cy }, Point { x: x1, y: cy })],
        },
        LineType::Dashed | LineType::Dotted => {
            let (seg, gap) = if lt == LineType::Dashed {
                (0.7, 0.35)
            } else {
                (0.18, 0.32)
            };
            let mut lines = Vec::new();
            let mut x = x0;
            while x < x1 {
                let e = (x + seg).min(x1);
                lines.push((Point { x, y: cy }, Point { x: e, y: cy }));
                x = e + gap;
            }
            DrawCmd::DrawLines {
                stroke: fg,
                line_width: 0.12,
                lines,
            }
        }
    }
}

/// Icons are drawn about `(cx, cy)` with half-extents derived from a
/// target *pixel* size, so they read as square despite anisotropic
/// cells rather than being squashed to the cell aspect.
fn tool_icon(t: Tool, cx: f32, cy: f32, fg: Style, cell_w: f32, cell_h: f32) -> Vec<DrawCmd> {
    let size_px = cell_h * 1.15;
    let hx = size_px / 2.0 / cell_w;
    let hy = size_px / 2.0 / cell_h;
    let lw = 0.10;

    // Map unit-square coords (origin top-left) into the icon box.
    let u = |x: f32, y: f32| Point {
        x: cx + (x - 0.5) * 2.0 * hx,
        y: cy + (y - 0.5) * 2.0 * hy,
    };

    match t {
        Tool::Select => vec![DrawCmd::FillPolygon {
            fill: fg,
            points: vec![
                u(0.18, 0.05),
                u(0.18, 0.92),
                u(0.40, 0.68),
                u(0.56, 1.00),
                u(0.70, 0.92),
                u(0.54, 0.62),
                u(0.82, 0.58),
            ],
        }],
        Tool::Box => vec![DrawCmd::DrawLineLoop {
            stroke: fg,
            line_width: lw,
            points: vec![u(0.1, 0.2), u(0.9, 0.2), u(0.9, 0.8), u(0.1, 0.8)],
        }],
        Tool::Ellipse => vec![DrawCmd::DrawLinePath {
            stroke: fg,
            line_width: lw,
            segments: vec![vge_protocol::path::PathSegment {
                start: u(0.05, 0.5),
                nodes: vec![
                    arc_node(hx * 0.95, hy * 0.6, u(0.95, 0.5)),
                    arc_node(hx * 0.95, hy * 0.6, u(0.05, 0.5)),
                    vge_protocol::path::PathNode::ClosePath,
                ],
            }],
        }],
        Tool::Diamond => vec![DrawCmd::DrawLineLoop {
            stroke: fg,
            line_width: lw,
            points: vec![u(0.5, 0.08), u(0.95, 0.5), u(0.5, 0.92), u(0.05, 0.5)],
        }],
        Tool::Line => vec![DrawCmd::DrawLines {
            stroke: fg,
            line_width: lw,
            lines: vec![(u(0.08, 0.85), u(0.92, 0.15))],
        }],
        Tool::Arrow => vec![
            DrawCmd::DrawLines {
                stroke: fg.clone(),
                line_width: lw,
                lines: vec![(u(0.08, 0.88), u(0.80, 0.24))],
            },
            DrawCmd::FillPolygon {
                fill: fg,
                points: vec![u(0.95, 0.08), u(0.60, 0.20), u(0.86, 0.46)],
            },
        ],
        // No glyph geometry needed — the primary font already has a T.
        Tool::Text => vec![DrawCmd::DrawText {
            origin: Point {
                x: cx,
                y: cy - 0.5,
            },
            align: Align::Center,
            fill: fg,
            font_style: FontStyle(0x01),
            text: "T".into(),
        }],
    }
}

fn arc_node(rx: f32, ry: f32, dst: Point) -> vge_protocol::path::PathNode {
    vge_protocol::path::PathNode::ArcEllipseTo {
        large: false,
        sweep: true,
        rx,
        ry,
        rotation: 0.0,
        dst,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(tool: Tool) -> ToolState {
        ToolState {
            tool,
            ..Default::default()
        }
    }

    #[test]
    fn every_tool_button_hits_its_own_tool() {
        let st = state(Tool::Box);
        let ch = layout(120, 40, &st);
        for t in TOOLS {
            let h = ch
                .hotspots
                .iter()
                .find(|h| h.action == Action::Tool(t))
                .expect("tool hotspot");
            let col = (h.rect.x + h.rect.w / 2.0) as u16;
            let row = (h.rect.y + h.rect.h / 2.0) as u16;
            assert_eq!(ch.hit(col, row), Some(Action::Tool(t)), "{}", t.label());
        }
    }

    #[test]
    fn select_tool_shows_no_options() {
        let ch = layout(120, 40, &state(Tool::Select));
        assert!(
            ch.hotspots
                .iter()
                .all(|h| matches!(h.action, Action::Tool(_))),
            "select must not expose option controls"
        );
    }

    #[test]
    fn palette_stays_put_when_the_options_row_appears() {
        let without = layout(120, 40, &state(Tool::Select));
        let with = layout(120, 40, &state(Tool::Box));
        let tool_row = |c: &Chrome| {
            c.hotspots
                .iter()
                .find(|h| matches!(h.action, Action::Tool(_)))
                .expect("tool hotspot")
                .rect
                .y
        };
        assert_eq!(
            tool_row(&without),
            tool_row(&with),
            "palette must not hop when options show/hide"
        );
    }

    #[test]
    fn text_tool_hides_line_type_but_keeps_colour() {
        let ch = layout(120, 40, &state(Tool::Text));
        assert!(!ch.hotspots.iter().any(|h| matches!(h.action, Action::Line(_))));
        assert!(ch.hotspots.iter().any(|h| matches!(h.action, Action::Color(_))));
        assert!(ch.hotspots.iter().any(|h| matches!(h.action, Action::Thickness(_))));
    }

    #[test]
    fn fill_swatches_appear_only_for_closed_shapes() {
        for t in [Tool::Box, Tool::Ellipse, Tool::Diamond] {
            let ch = layout(120, 40, &state(t));
            assert!(
                ch.hotspots.iter().any(|h| matches!(h.action, Action::Fill(_))),
                "{} should offer a fill",
                t.label()
            );
        }
        for t in [Tool::Line, Tool::Arrow, Tool::Text, Tool::Select] {
            let ch = layout(120, 40, &state(t));
            assert!(
                !ch.hotspots.iter().any(|h| matches!(h.action, Action::Fill(_))),
                "{} has no interior to fill",
                t.label()
            );
        }
    }

    #[test]
    fn stroke_and_fill_swatches_are_distinct_actions() {
        let ch = layout(120, 40, &state(Tool::Box));
        // The same colour string can appear in both palettes; clicking
        // one must not change the other.
        let strokes: Vec<_> = ch
            .hotspots
            .iter()
            .filter(|h| matches!(h.action, Action::Color(_)))
            .collect();
        let fills: Vec<_> = ch
            .hotspots
            .iter()
            .filter(|h| matches!(h.action, Action::Fill(_)))
            .collect();
        assert_eq!(strokes.len(), COLORS.len());
        assert_eq!(fills.len(), FILLS.len());
        for s in &strokes {
            for f in &fills {
                assert_ne!(s.rect.x, f.rect.x, "swatch groups must not overlap");
            }
        }
    }

    /// The options row grew when fills were added; it has to keep
    /// fitting an ordinary terminal.
    #[test]
    fn the_widest_options_row_fits_in_eighty_columns() {
        for t in TOOLS {
            let ch = layout(80, 40, &state(t));
            for h in &ch.hotspots {
                assert!(
                    h.rect.x >= 0.0 && h.rect.x + h.rect.w <= 80.0,
                    "{} control runs off screen at 80 cols: {:?}",
                    t.label(),
                    h.action
                );
            }
        }
    }

    /// Fractional-pixel edges are what smear a rounded corner across
    /// two pixel rows; every chrome edge must land on a whole pixel.
    #[test]
    fn chrome_rects_snap_to_whole_pixels() {
        let (cw, ch) = (8.0, 17.0);
        // A pill-shaped rect with the awkward fractional insets.
        let r = Rect {
            x: 47.0 + 0.2,
            y: 34.0 + 0.2,
            w: 5.0 - 0.4,
            h: 2.0 - 0.4,
        };
        let s = snap_rect(r, cw, ch);
        for (v, cell) in [
            (s.x, cw),
            (s.y, ch),
            (s.x + s.w, cw),
            (s.y + s.h, ch),
        ] {
            let px = v * cell;
            assert!(
                (px - px.round()).abs() < 1e-3,
                "edge at {px}px is not on a pixel boundary"
            );
        }
        // Snapping must not move an edge by more than half a pixel.
        assert!((s.x - r.x).abs() * cw <= 0.5 + 1e-3);
        assert!((s.y - r.y).abs() * ch <= 0.5 + 1e-3);
    }

    #[test]
    fn the_two_panels_are_visually_separated() {
        let ch = layout(120, 40, &state(Tool::Box));
        assert_eq!(ch.panels.len(), 2, "palette + options");
        let (palette, options) = (ch.panels[0], ch.panels[1]);
        assert!(
            palette.y + palette.h < options.y,
            "panels are flush: palette ends {}, options start {}",
            palette.y + palette.h,
            options.y
        );
        // No control may sit in the gap between them.
        let gap_row = (palette.y + palette.h) as u16;
        assert!(ch.hit(60, gap_row).is_none());
    }

    #[test]
    fn hotspots_never_overlap() {
        let ch = layout(120, 40, &state(Tool::Box));
        for (i, a) in ch.hotspots.iter().enumerate() {
            for b in &ch.hotspots[i + 1..] {
                let disjoint = a.rect.x + a.rect.w <= b.rect.x
                    || b.rect.x + b.rect.w <= a.rect.x
                    || a.rect.y + a.rect.h <= b.rect.y
                    || b.rect.y + b.rect.h <= a.rect.y;
                assert!(disjoint, "{:?} overlaps {:?}", a.action, b.action);
            }
        }
    }

    #[test]
    fn exactly_one_control_per_group_is_active() {
        let st = state(Tool::Box);
        let ch = layout(120, 40, &st);
        let count = |f: fn(&Action) -> bool| {
            ch.hotspots.iter().filter(|h| f(&h.action) && h.active).count()
        };
        assert_eq!(count(|a| matches!(a, Action::Tool(_))), 1);
        assert_eq!(count(|a| matches!(a, Action::Thickness(_))), 1);
        assert_eq!(count(|a| matches!(a, Action::Color(_))), 1);
        assert_eq!(count(|a| matches!(a, Action::Line(_))), 1);
    }

    #[test]
    fn chrome_sits_above_the_status_line() {
        let ch = layout(120, 40, &state(Tool::Box));
        for h in &ch.hotspots {
            assert!(
                h.rect.y + h.rect.h <= 39.0,
                "control overlaps status row: {:?}",
                h.action
            );
        }
    }

    #[test]
    fn covers_includes_panel_padding_between_groups() {
        let ch = layout(120, 40, &state(Tool::Box));
        let p = ch.panels.last().expect("options panel");
        // A gap cell between two option groups is not a hotspot but
        // must still count as chrome, so dragging there doesn't pan.
        let mid = (p.y + p.h / 2.0) as u16;
        let mut found_gap = false;
        for c in (p.x as u16)..((p.x + p.w) as u16) {
            if ch.hit(c, mid).is_none() {
                assert!(ch.covers(c, mid));
                found_gap = true;
            }
        }
        assert!(found_gap, "expected at least one gap cell");
    }
}
