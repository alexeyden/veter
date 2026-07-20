//! Active tool and the style options that apply to what it draws.
//!
//! Options are deliberately minimal — thickness, colour, line type —
//! and are shared across tools rather than stored per tool, so a colour
//! picked while drawing boxes still applies when switching to arrows.

use crate::doc::{Element, Shape};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Box,
    Ellipse,
    Diamond,
    Line,
    Arrow,
    Text,
}

/// Palette order, left to right.
pub const TOOLS: [Tool; 7] = [
    Tool::Select,
    Tool::Box,
    Tool::Ellipse,
    Tool::Diamond,
    Tool::Line,
    Tool::Arrow,
    Tool::Text,
];

impl Tool {
    pub fn key(self) -> char {
        match self {
            Tool::Select => 's',
            Tool::Box => 'b',
            Tool::Ellipse => 'e',
            Tool::Diamond => 'd',
            Tool::Line => 'l',
            Tool::Arrow => 'a',
            Tool::Text => 't',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Box => "box",
            Tool::Ellipse => "ellipse",
            Tool::Diamond => "diamond",
            Tool::Line => "line",
            Tool::Arrow => "arrow",
            Tool::Text => "text",
        }
    }

    pub fn from_key(c: char) -> Option<Self> {
        let lower = c.to_ascii_lowercase();
        TOOLS.into_iter().find(|t| t.key() == lower)
    }

    /// The document shape this tool creates, if any.
    pub fn shape(self) -> Option<Shape> {
        Some(match self {
            Tool::Select => return None,
            Tool::Box => Shape::Rectangle,
            Tool::Ellipse => Shape::Ellipse,
            Tool::Diamond => Shape::Diamond,
            Tool::Line => Shape::Line,
            Tool::Arrow => Shape::Arrow,
            Tool::Text => Shape::Text,
        })
    }

    /// Whether dragging with this tool creates a shape. Text is placed
    /// by click and typing rather than dragged out (phase 5), so it is
    /// excluded even though it has a shape.
    pub fn creates_by_drag(self) -> bool {
        matches!(
            self,
            Tool::Box | Tool::Ellipse | Tool::Diamond | Tool::Line | Tool::Arrow
        )
    }

    /// Select has nothing to configure; text has no line type, but the
    /// row is still shown so the colour swatches stay reachable.
    pub fn has_options(self) -> bool {
        self != Tool::Select
    }

    pub fn has_line_type(self) -> bool {
        self != Tool::Select && self != Tool::Text
    }

    /// Only closed shapes can be filled; lines, arrows and text have no
    /// interior, so showing them a background palette would be a lie.
    pub fn has_fill(self) -> bool {
        matches!(self, Tool::Box | Tool::Ellipse | Tool::Diamond)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineType {
    Solid,
    Dashed,
    Dotted,
}

pub const LINE_TYPES: [LineType; 3] = [LineType::Solid, LineType::Dashed, LineType::Dotted];

impl LineType {
    /// The `strokeStyle` string the Excalidraw schema uses.
    pub fn as_str(self) -> &'static str {
        match self {
            LineType::Solid => "solid",
            LineType::Dashed => "dashed",
            LineType::Dotted => "dotted",
        }
    }
}

/// Stroke widths in doc px, matching Excalidraw's thin/bold/extra-bold.
pub const THICKNESSES: [f32; 3] = [1.0, 2.0, 4.0];

/// Background swatches, Excalidraw's default fill palette. The first
/// entry is `"transparent"` — an outline-only shape, and the default.
pub const FILLS: [&str; 5] = [
    "transparent",
    "#ffc9c9", // red tint
    "#b2f2bb", // green tint
    "#a5d8ff", // blue tint
    "#ffec99", // yellow tint
];

/// Stroke swatches. The first entry is the default, and is a light grey
/// rather than Excalidraw's near-black `#1e1e1e`: veter's canvas is the
/// terminal background, which is usually dark, so a near-black default
/// draws shapes that are effectively invisible.
pub const COLORS: [&str; 6] = [
    "#c9ced8", // light grey
    "#e03131", // red
    "#2f9e44", // green
    "#1971c2", // blue
    "#f08c00", // amber
    "#7048e8", // violet
];

#[derive(Debug, Clone, Copy)]
pub struct ToolState {
    pub tool: Tool,
    pub thickness: f32,
    pub color: &'static str,
    pub fill: &'static str,
    pub line_type: LineType,
}

impl Default for ToolState {
    fn default() -> Self {
        Self {
            tool: Tool::Select,
            thickness: THICKNESSES[0],
            color: COLORS[0],
            fill: FILLS[0],
            line_type: LineType::Solid,
        }
    }
}

impl ToolState {
    /// Build a document element from the active tool and options.
    /// Returns `None` for tools that create nothing (Select).
    ///
    /// `(x, y, w, h)` is the drag extent in doc px, with `w`/`h` signed
    /// so a backwards drag is expressible. Polyline shapes use the two
    /// corners as endpoints and so keep the direction; box-like shapes
    /// have it normalised away here.
    pub fn new_element(
        &self,
        id: impl Into<String>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> Option<Element> {
        let shape = self.tool.shape()?;
        let mut e = match shape {
            Shape::Line | Shape::Arrow => {
                Element::polyline(id, shape, &[(x, y), (x + w, y + h)])
            }
            // A drag up-and-left produces negative extent; rects, ellipses
            // and diamonds need the top-left corner and positive size.
            _ => Element::new(
                id,
                shape,
                x.min(x + w),
                y.min(y + h),
                w.abs(),
                h.abs(),
            ),
        };
        e.stroke_color = self.color.into();
        e.stroke_width = self.thickness;
        e.stroke_style = self.line_type.as_str().into();
        if self.tool.has_fill() {
            e.background_color = self.fill.into();
        }
        if shape == Shape::Rectangle {
            e = e.with_adaptive_rounding();
        }
        Some(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_unique_key() {
        let mut keys: Vec<char> = TOOLS.iter().map(|t| t.key()).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "duplicate tool key");
    }

    #[test]
    fn keys_round_trip_and_ignore_case() {
        for t in TOOLS {
            assert_eq!(Tool::from_key(t.key()), Some(t));
            assert_eq!(Tool::from_key(t.key().to_ascii_uppercase()), Some(t));
        }
        assert_eq!(Tool::from_key('z'), None);
    }

    #[test]
    fn select_creates_nothing() {
        let st = ToolState::default();
        assert!(st.tool.shape().is_none());
        assert!(st.new_element("x", 0.0, 0.0, 10.0, 10.0).is_none());
    }

    #[test]
    fn new_element_carries_the_active_options() {
        let st = ToolState {
            tool: Tool::Box,
            thickness: THICKNESSES[2],
            color: COLORS[3],
            fill: FILLS[2],
            line_type: LineType::Dashed,
        };
        let e = st.new_element("e1", 10.0, 20.0, 100.0, 50.0).expect("element");
        assert_eq!(e.kind, "rectangle");
        assert_eq!(e.stroke_color, COLORS[3]);
        assert_eq!(e.stroke_width, THICKNESSES[2]);
        // Must match the strings render.rs dispatches dashing on.
        assert_eq!(e.stroke_style, "dashed");
        assert_eq!((e.x, e.y, e.width, e.height), (10.0, 20.0, 100.0, 50.0));
    }

    #[test]
    fn fill_applies_to_closed_shapes_only() {
        let base = ToolState {
            fill: FILLS[3],
            ..Default::default()
        };
        let boxed = ToolState {
            tool: Tool::Box,
            ..base
        };
        let e = boxed.new_element("b", 0.0, 0.0, 10.0, 10.0).expect("box");
        assert_eq!(e.background_color, FILLS[3]);

        // An arrow has no interior; it must stay transparent even when a
        // fill is selected, or the saved file claims a fill it can't show.
        let arrow = ToolState {
            tool: Tool::Arrow,
            ..base
        };
        let a = arrow.new_element("a", 0.0, 0.0, 10.0, 10.0).expect("arrow");
        assert_eq!(a.background_color, "transparent");
    }

    #[test]
    fn transparent_is_the_default_fill() {
        let st = ToolState {
            tool: Tool::Box,
            ..Default::default()
        };
        let e = st.new_element("b", 0.0, 0.0, 10.0, 10.0).expect("box");
        assert_eq!(e.background_color, "transparent");
    }

    #[test]
    fn backwards_drag_normalises_for_box_shapes() {
        let st = ToolState {
            tool: Tool::Box,
            ..Default::default()
        };
        // Dragged up and to the left from (100, 100).
        let e = st.new_element("e1", 100.0, 100.0, -60.0, -40.0).expect("element");
        assert_eq!((e.x, e.y), (40.0, 60.0));
        assert_eq!((e.width, e.height), (60.0, 40.0));
    }

    #[test]
    fn backwards_drag_keeps_direction_for_arrows() {
        let st = ToolState {
            tool: Tool::Arrow,
            ..Default::default()
        };
        let e = st.new_element("a1", 100.0, 100.0, -60.0, 0.0).expect("element");
        // The tip is the drag end, so the last point must be the left one.
        assert_eq!(e.points.first().copied(), Some([60.0, 0.0]));
        assert_eq!(e.points.last().copied(), Some([0.0, 0.0]));
    }

    #[test]
    fn polyline_tools_use_the_drag_as_endpoints() {
        let st = ToolState {
            tool: Tool::Arrow,
            ..Default::default()
        };
        let e = st.new_element("a1", 10.0, 20.0, 60.0, 40.0).expect("element");
        assert_eq!(e.kind, "arrow");
        assert_eq!(e.points.len(), 2);
        // Points are relative to the element's bounding-box origin.
        assert_eq!(e.points[0], [0.0, 0.0]);
        assert_eq!(e.points[1], [60.0, 40.0]);
        assert_eq!((e.x, e.y), (10.0, 20.0));
    }

    #[test]
    fn tool_keys_do_not_collide_with_view_bindings() {
        // main.rs owns these for zoom / reset / quit.
        for reserved in ['q', '0', '+', '-', '='] {
            assert_eq!(Tool::from_key(reserved), None, "{reserved} is taken");
        }
    }
}
