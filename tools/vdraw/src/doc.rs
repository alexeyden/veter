//! Document model — Excalidraw's schema is the native format.
//!
//! Geometry is stored in Excalidraw's device-independent pixels at zoom
//! 1, exactly as the `.excalidraw` file has it, so save/load is a plain
//! serde round-trip. Conversion to VGE cell units happens once, at
//! render time (`render.rs`), against the probe's cell pixel size.
//!
//! Unrecognised fields are preserved verbatim through `extra`, so a file
//! written by the real editor round-trips without losing the properties
//! vdraw has no opinion about (`seed`, `versionNonce`, `link`, …).

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    #[serde(default = "default_source")]
    pub source: String,
    pub elements: Vec<Element>,
    #[serde(default)]
    pub app_state: Value,
    #[serde(default)]
    pub files: Value,
}

fn default_source() -> String {
    "vdraw".into()
}

impl Default for Document {
    fn default() -> Self {
        Self {
            kind: "excalidraw".into(),
            version: 2,
            source: default_source(),
            elements: Vec::new(),
            app_state: Value::Object(Map::new()),
            files: Value::Object(Map::new()),
        }
    }
}

impl Document {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing {} as an .excalidraw document", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Rectangle,
    Ellipse,
    Diamond,
    Line,
    Arrow,
    Text,
}

impl Shape {
    pub fn as_str(self) -> &'static str {
        match self {
            Shape::Rectangle => "rectangle",
            Shape::Ellipse => "ellipse",
            Shape::Diamond => "diamond",
            Shape::Line => "line",
            Shape::Arrow => "arrow",
            Shape::Text => "text",
        }
    }

    /// Whether the shape has an interior — i.e. can be filled, and has
    /// a closed outline rather than open endpoints.
    pub fn is_closed(self) -> bool {
        matches!(self, Shape::Rectangle | Shape::Ellipse | Shape::Diamond)
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "rectangle" => Shape::Rectangle,
            "ellipse" => Shape::Ellipse,
            "diamond" => Shape::Diamond,
            "line" => Shape::Line,
            "arrow" => Shape::Arrow,
            "text" => Shape::Text,
            _ => return None,
        })
    }
}

/// Only the fields that survive the trip to VGE. Roughjs-related fields
/// (`roughness`, `seed`) are deliberately absent: VGE strokes are clean
/// and the hand-drawn look has no wire representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Element {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub angle: f32,
    pub stroke_color: String,
    pub background_color: String,
    #[serde(default = "default_fill_style")]
    pub fill_style: String,
    #[serde(default = "one")]
    pub stroke_width: f32,
    #[serde(default = "default_stroke_style")]
    pub stroke_style: String,
    #[serde(default = "hundred")]
    pub opacity: f32,
    #[serde(default)]
    pub is_deleted: bool,
    /// line / arrow: vertices relative to (x, y).
    #[serde(default)]
    pub points: Vec<[f32; 2]>,
    #[serde(default)]
    pub roundness: Option<Roundness>,
    #[serde(default)]
    pub text: String,
    #[serde(default = "default_text_align")]
    pub text_align: String,
    #[serde(default)]
    pub container_id: Option<String>,
    // --- fields the real editor requires but vdraw has no opinion on ---
    #[serde(default)]
    pub group_ids: Vec<String>,
    #[serde(default)]
    pub roughness: f32,
    #[serde(default)]
    pub seed: u32,
    #[serde(default)]
    pub locked: bool,
    /// Everything else in the file, preserved so a round-trip through
    /// vdraw doesn't strip properties it doesn't understand.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn one() -> f32 {
    1.0
}
fn hundred() -> f32 {
    100.0
}
fn default_fill_style() -> String {
    "solid".into()
}
fn default_stroke_style() -> String {
    "solid".into()
}
fn default_text_align() -> String {
    "left".into()
}

/// Excalidraw's corner rounding. `type` 2 is a legacy proportional
/// radius and 3 is the adaptive one current files use; `value` is only
/// present for the legacy form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roundness {
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f32>,
}

/// Adaptive rounding, matching what Excalidraw draws when `value` is
/// absent: a quarter of the short side, capped.
pub const ADAPTIVE_RADIUS_CAP: f32 = 32.0;

impl Element {
    pub fn shape(&self) -> Option<Shape> {
        Shape::from_str(&self.kind)
    }

    /// Corner radius in doc px, resolving Excalidraw's adaptive form.
    pub fn corner_radius(&self) -> f32 {
        match &self.roundness {
            None => 0.0,
            Some(r) => r.value.unwrap_or_else(|| {
                (self.width.min(self.height) * 0.25).min(ADAPTIVE_RADIUS_CAP)
            }),
        }
    }

    pub fn new(id: impl Into<String>, shape: Shape, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            id: id.into(),
            kind: shape.as_str().into(),
            x,
            y,
            width: w,
            height: h,
            angle: 0.0,
            // Matches tools::COLORS[0] — legible on a dark terminal.
            stroke_color: "#c9ced8".into(),
            background_color: "transparent".into(),
            fill_style: default_fill_style(),
            stroke_width: 1.0,
            stroke_style: default_stroke_style(),
            opacity: 100.0,
            is_deleted: false,
            points: Vec::new(),
            roundness: None,
            text: String::new(),
            text_align: default_text_align(),
            container_id: None,
            group_ids: Vec::new(),
            roughness: 0.0,
            seed: 1,
            locked: false,
            extra: Map::new(),
        }
    }

    /// Adaptive rounding, the form current Excalidraw files use.
    pub fn with_adaptive_rounding(mut self) -> Self {
        self.roundness = Some(Roundness {
            kind: 3,
            value: None,
        });
        self
    }

    /// Polyline elements carry their vertices relative to `(x, y)`;
    /// `width`/`height` are the bounding box of those points.
    pub fn polyline(id: impl Into<String>, shape: Shape, pts: &[(f32, f32)]) -> Self {
        let (x0, y0) = pts.iter().fold((f32::MAX, f32::MAX), |(ax, ay), (x, y)| {
            (ax.min(*x), ay.min(*y))
        });
        let (x1, y1) = pts.iter().fold((f32::MIN, f32::MIN), |(ax, ay), (x, y)| {
            (ax.max(*x), ay.max(*y))
        });
        let mut e = Self::new(id, shape, x0, y0, x1 - x0, y1 - y0);
        e.points = pts.iter().map(|(x, y)| [x - x0, y - y0]).collect();
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a file written by the real editor — the shape of
    /// what vdraw has to survive, including fields it ignores.
    const REAL_FILE: &str = r##"{
      "type": "excalidraw",
      "version": 2,
      "source": "https://excalidraw.com",
      "elements": [
        {
          "id": "vJ2k",
          "type": "rectangle",
          "x": 120.5, "y": 240, "width": 200, "height": 100,
          "angle": 0,
          "strokeColor": "#1e1e1e",
          "backgroundColor": "transparent",
          "fillStyle": "solid",
          "strokeWidth": 2,
          "strokeStyle": "solid",
          "roughness": 1,
          "opacity": 100,
          "groupIds": [],
          "frameId": null,
          "index": "a0",
          "roundness": { "type": 3 },
          "seed": 1968410350,
          "version": 42,
          "versionNonce": 1150084233,
          "isDeleted": false,
          "boundElements": null,
          "updated": 1700000000000,
          "link": null,
          "locked": false
        }
      ],
      "appState": { "gridSize": null, "viewBackgroundColor": "#ffffff" },
      "files": {}
    }"##;

    #[test]
    fn parses_a_real_excalidraw_file() {
        let d: Document = serde_json::from_str(REAL_FILE).expect("parse");
        assert_eq!(d.elements.len(), 1);
        let e = &d.elements[0];
        assert_eq!(e.shape(), Some(Shape::Rectangle));
        assert_eq!((e.x, e.y, e.width, e.height), (120.5, 240.0, 200.0, 100.0));
        assert_eq!(e.stroke_width, 2.0);
        assert_eq!(e.background_color, "transparent");
        // The adaptive form carries no explicit radius.
        assert_eq!(e.roundness.as_ref().map(|r| r.kind), Some(3));
        assert!(e.roundness.as_ref().unwrap().value.is_none());
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let d: Document = serde_json::from_str(REAL_FILE).expect("parse");
        let out = serde_json::to_string(&d).expect("serialise");
        let back: serde_json::Value = serde_json::from_str(&out).expect("reparse");
        let el = back["elements"][0].as_object().expect("element object");
        // Fields vdraw has no opinion about must not be dropped. Test
        // for *presence*, not non-null: `frameId` and `link` are
        // legitimately null in the source file.
        for key in ["seed", "versionNonce", "index", "frameId", "updated", "link"] {
            assert!(el.contains_key(key), "{key} was lost in the round trip");
        }
        assert_eq!(el["versionNonce"], 1150084233u64);
        assert!(el["frameId"].is_null(), "null must stay null, not vanish");
        assert_eq!(el["id"], "vJ2k");
        assert_eq!(back["appState"]["viewBackgroundColor"], "#ffffff");
    }

    #[test]
    fn adaptive_rounding_derives_a_radius_from_the_short_side() {
        let e = Element::new("r", Shape::Rectangle, 0.0, 0.0, 200.0, 80.0)
            .with_adaptive_rounding();
        // A quarter of the short side.
        assert_eq!(e.corner_radius(), 20.0);

        // ...capped, so huge shapes don't become lozenges.
        let big = Element::new("r", Shape::Rectangle, 0.0, 0.0, 900.0, 800.0)
            .with_adaptive_rounding();
        assert_eq!(big.corner_radius(), ADAPTIVE_RADIUS_CAP);
    }

    /// vdraw only ever writes the adaptive form, but files from older
    /// editor versions carry `type: 2` with an explicit radius.
    #[test]
    fn legacy_rounding_uses_its_explicit_value() {
        let mut e = Element::new("r", Shape::Rectangle, 0.0, 0.0, 200.0, 80.0);
        e.roundness = Some(Roundness {
            kind: 2,
            value: Some(6.0),
        });
        assert_eq!(e.corner_radius(), 6.0);
    }

    #[test]
    fn square_corners_when_roundness_is_absent() {
        let e = Element::new("r", Shape::Rectangle, 0.0, 0.0, 200.0, 80.0);
        assert_eq!(e.corner_radius(), 0.0);
    }

    #[test]
    fn documents_we_write_are_reloadable() {
        let mut d = Document::default();
        d.elements = vec![
            {
                let mut e = Element::new("r", Shape::Rectangle, 80.0, 80.0, 200.0, 90.0)
                    .with_adaptive_rounding();
                e.background_color = "#e7f5ff".into();
                e.text = "ingest".into();
                e
            },
            Element::polyline("a", Shape::Arrow, &[(280.0, 125.0), (420.0, 125.0)]),
        ];
        let text = serde_json::to_string_pretty(&d).expect("serialise");
        let back: Document = serde_json::from_str(&text).expect("reload");
        assert_eq!(back.elements.len(), d.elements.len());
        assert_eq!(back.kind, "excalidraw");
        for (a, b) in back.elements.iter().zip(d.elements.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.kind, b.kind);
            assert_eq!((a.x, a.y), (b.x, b.y));
            assert_eq!(a.points, b.points);
        }
    }
}
