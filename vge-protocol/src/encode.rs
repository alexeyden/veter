// Command encoders — the inverse of command::parse. Used by clients
// (and the vge-cli test harness) to produce wire bytes from typed
// commands. Each encoder produces just the *body*; pair with
// `frame_type_for(...)` and `envelope::append_frame` /
// `envelope::wrap_c2t_envelope` to get a full PTY-ready envelope.

use crate::codec::{Point, Writer};
use crate::command::{
    Align, Color, Command, ConcreteStyle, CreateElementBody, DrawCmd, Style, UpdateCommandBody,
    UpdateCommandsBody, UpdateImageBody, UpdateTextBody, UpdateTextRange, UploadImageBody,
};
use crate::envelope::{append_frame, wrap_c2t_envelope};
use crate::frame::*;
use crate::path::{PathNode, PathSegment};

/// Frame-type code (the u8 that goes in front of every frame body) for
/// a given command. Mirrors `command::parse`'s dispatch.
pub fn frame_type_for(cmd: &Command) -> u8 {
    match cmd {
        Command::Probe => CMD_PROBE,
        Command::CreateElement(_) => CMD_CREATE_ELEMENT,
        Command::DeleteElement { .. } => CMD_DELETE_ELEMENT,
        Command::UpdateCommands(_) => CMD_UPDATE_COMMANDS,
        Command::UpdateCommand(_) => CMD_UPDATE_COMMAND,
        Command::UpdateText(_) => CMD_UPDATE_TEXT,
        Command::UpdateOrigin { .. } => CMD_UPDATE_ORIGIN,
        Command::UpdateVisibility { .. } => CMD_UPDATE_VISIBILITY,
        Command::UpdateDrawOrder { .. } => CMD_UPDATE_DRAW_ORDER,
        Command::ClearAll => CMD_CLEAR_ALL,
        Command::SetGlobalStyle { .. } => CMD_SET_GLOBAL_STYLE,
        Command::UploadImage(_) => CMD_UPLOAD_IMAGE,
        Command::DropImage { .. } => CMD_DROP_IMAGE,
        Command::UpdateImage(_) => CMD_UPDATE_IMAGE,
        Command::UpdateSize { .. } => CMD_UPDATE_SIZE,
    }
}

/// Encode a `Command` as the body bytes for a single frame.
pub fn encode_command(cmd: &Command) -> Vec<u8> {
    let mut w = Writer::with_capacity(64);
    match cmd {
        Command::Probe => {}
        Command::CreateElement(b) => write_create_element(&mut w, b),
        Command::DeleteElement { id } => w.str(id),
        Command::UpdateCommands(b) => write_update_commands(&mut w, b),
        Command::UpdateCommand(b) => write_update_command(&mut w, b),
        Command::UpdateText(b) => write_update_text(&mut w, b),
        Command::UpdateOrigin { id, origin } => {
            w.str(id);
            write_point(&mut w, *origin);
        }
        Command::UpdateVisibility { id, is_visible } => {
            w.str(id);
            w.u8(if *is_visible { 1 } else { 0 });
        }
        Command::UpdateDrawOrder { id, draw_order } => {
            w.str(id);
            for b in draw_order.to_le_bytes() {
                w.u8(b);
            }
        }
        Command::ClearAll => {}
        Command::SetGlobalStyle { id, style } => {
            w.str(id);
            write_concrete_style(&mut w, style);
        }
        Command::UploadImage(b) => write_upload_image(&mut w, b),
        Command::DropImage { id } => w.str(id),
        Command::UpdateImage(b) => write_update_image(&mut w, b),
        Command::UpdateSize { id, new_size } => {
            w.str(id);
            write_point(&mut w, *new_size);
        }
    }
    w.buf
}

/// Bundle a slice of (command, request_id) pairs into a single
/// client→terminal APC envelope ready to write to a PTY.
pub fn build_envelope(commands: &[(Command, u32)]) -> Vec<u8> {
    let mut frames = Vec::new();
    for (cmd, req_id) in commands {
        let body = encode_command(cmd);
        append_frame(&mut frames, frame_type_for(cmd), *req_id, &body);
    }
    wrap_c2t_envelope(&frames)
}

// --- helpers ---

fn write_point(w: &mut Writer, p: Point) {
    w.f32(p.x);
    w.f32(p.y);
}

fn write_rect(w: &mut Writer, r: crate::codec::Rect) {
    w.f32(r.x);
    w.f32(r.y);
    w.f32(r.w);
    w.f32(r.h);
}

fn write_color(w: &mut Writer, c: Color) {
    w.u8(COLOR_RGBA8888);
    w.u8((c.r.clamp(0.0, 1.0) * 255.0).round() as u8);
    w.u8((c.g.clamp(0.0, 1.0) * 255.0).round() as u8);
    w.u8((c.b.clamp(0.0, 1.0) * 255.0).round() as u8);
    w.u8((c.a.clamp(0.0, 1.0) * 255.0).round() as u8);
}

fn write_style(w: &mut Writer, s: &Style) {
    match s {
        Style::Flat(c) => {
            w.u8(STYLE_FLAT);
            write_color(w, *c);
        }
        Style::LinearGradient { p0, p1, c0, c1 } => {
            w.u8(STYLE_LINEAR_GRADIENT);
            write_point(w, *p0);
            write_point(w, *p1);
            write_color(w, *c0);
            write_color(w, *c1);
        }
        Style::RadialGradient {
            center,
            outer,
            c_inner,
            c_outer,
        } => {
            w.u8(STYLE_RADIAL_GRADIENT);
            write_point(w, *center);
            write_point(w, *outer);
            write_color(w, *c_inner);
            write_color(w, *c_outer);
        }
        Style::Ref(id) => {
            w.u8(STYLE_REF);
            w.str(id);
        }
    }
}

fn write_concrete_style(w: &mut Writer, s: &ConcreteStyle) {
    write_style(w, &s.as_style());
}

fn write_path_segments(w: &mut Writer, segs: &[PathSegment]) {
    w.varu(segs.len() as u64);
    for s in segs {
        write_point(w, s.start);
        w.varu(s.nodes.len() as u64);
        for n in &s.nodes {
            write_path_node(w, n);
        }
    }
}

fn write_path_node(w: &mut Writer, n: &PathNode) {
    match n {
        PathNode::LineTo { dst } => {
            w.u8(0);
            write_point(w, *dst);
        }
        PathNode::HorizontalLineTo { x } => {
            w.u8(1);
            w.f32(*x);
        }
        PathNode::VerticalLineTo { y } => {
            w.u8(2);
            w.f32(*y);
        }
        PathNode::CubicBezierTo { c0, c1, dst } => {
            w.u8(3);
            write_point(w, *c0);
            write_point(w, *c1);
            write_point(w, *dst);
        }
        PathNode::ArcEllipseTo {
            large,
            sweep,
            rx,
            ry,
            rotation,
            dst,
        } => {
            w.u8(4);
            let mut flags: u8 = 0;
            if *large {
                flags |= 0x01;
            }
            if *sweep {
                flags |= 0x02;
            }
            w.u8(flags);
            w.f32(*rx);
            w.f32(*ry);
            w.f32(*rotation);
            write_point(w, *dst);
        }
        PathNode::ClosePath => {
            w.u8(5);
        }
        PathNode::QuadraticBezierTo { c, dst } => {
            w.u8(6);
            write_point(w, *c);
            write_point(w, *dst);
        }
    }
}

fn write_draw_cmd(w: &mut Writer, cmd: &DrawCmd) {
    match cmd {
        DrawCmd::FillRectangles { fill, rects } => {
            w.u8(OP_FILL_RECTANGLES);
            write_style(w, fill);
            w.varu(rects.len() as u64);
            for r in rects {
                write_rect(w, *r);
            }
        }
        DrawCmd::DrawText {
            origin,
            align,
            fill,
            font_style,
            text,
        } => {
            w.u8(OP_DRAW_TEXT);
            write_point(w, *origin);
            w.u8(match align {
                Align::Left => 0,
                Align::Center => 1,
                Align::Right => 2,
            });
            write_style(w, fill);
            w.u8(font_style.0);
            w.str(text);
        }
        DrawCmd::FillPolygon { fill, points } => {
            w.u8(OP_FILL_POLYGON);
            write_style(w, fill);
            w.varu(points.len() as u64);
            for p in points {
                write_point(w, *p);
            }
        }
        DrawCmd::FillPath { fill, segments } => {
            w.u8(OP_FILL_PATH);
            write_style(w, fill);
            write_path_segments(w, segments);
        }
        DrawCmd::DrawLines {
            stroke,
            line_width,
            lines,
        } => {
            w.u8(OP_DRAW_LINES);
            write_style(w, stroke);
            w.f32(*line_width);
            w.varu(lines.len() as u64);
            for (a, b) in lines {
                write_point(w, *a);
                write_point(w, *b);
            }
        }
        DrawCmd::DrawLineLoop {
            stroke,
            line_width,
            points,
        } => {
            w.u8(OP_DRAW_LINE_LOOP);
            write_style(w, stroke);
            w.f32(*line_width);
            w.varu(points.len() as u64);
            for p in points {
                write_point(w, *p);
            }
        }
        DrawCmd::DrawLineStrip {
            stroke,
            line_width,
            points,
        } => {
            w.u8(OP_DRAW_LINE_STRIP);
            write_style(w, stroke);
            w.f32(*line_width);
            w.varu(points.len() as u64);
            for p in points {
                write_point(w, *p);
            }
        }
        DrawCmd::DrawLinePath {
            stroke,
            line_width,
            segments,
        } => {
            w.u8(OP_DRAW_LINE_PATH);
            write_style(w, stroke);
            w.f32(*line_width);
            write_path_segments(w, segments);
        }
        DrawCmd::OutlineFillPolygon {
            fill,
            stroke,
            line_width,
            points,
        } => {
            w.u8(OP_OUTLINE_FILL_POLYGON);
            write_style(w, fill);
            write_style(w, stroke);
            w.f32(*line_width);
            w.varu(points.len() as u64);
            for p in points {
                write_point(w, *p);
            }
        }
        DrawCmd::OutlineFillRectangles {
            fill,
            stroke,
            line_width,
            rects,
        } => {
            w.u8(OP_OUTLINE_FILL_RECTANGLES);
            write_style(w, fill);
            write_style(w, stroke);
            w.f32(*line_width);
            w.varu(rects.len() as u64);
            for r in rects {
                write_rect(w, *r);
            }
        }
        DrawCmd::OutlineFillPath {
            fill,
            stroke,
            line_width,
            segments,
        } => {
            w.u8(OP_OUTLINE_FILL_PATH);
            write_style(w, fill);
            write_style(w, stroke);
            w.f32(*line_width);
            write_path_segments(w, segments);
        }
        DrawCmd::DrawImage {
            target_rect,
            image_id,
        } => {
            w.u8(OP_DRAW_IMAGE);
            write_rect(w, *target_rect);
            w.str(image_id);
        }
    }
}

fn write_upload_image(w: &mut Writer, b: &UploadImageBody) {
    w.str(&b.id);
    w.u8(b.encoding);
    w.u32(b.width);
    w.u32(b.height);
    w.bytes(&b.data);
}

fn write_update_image(w: &mut Writer, b: &UpdateImageBody) {
    w.str(&b.id);
    w.varu(b.command_index as u64);
    w.str(&b.new_image_id);
}

fn write_create_element(w: &mut Writer, b: &CreateElementBody) {
    w.str(&b.id);
    w.varu(b.commands.len() as u64);
    for c in &b.commands {
        write_draw_cmd(w, c);
    }
    write_point(w, b.origin);
    w.u8(if b.is_visible { 1 } else { 0 });
    for byte in b.draw_order.to_le_bytes() {
        w.u8(byte);
    }
    // §9.4 trailing block. Emit only if at least one optional field is
    // present, so v1-style bodies stay backwards-compatible on the wire.
    let has_parent = b.parent.is_some();
    let has_size = b.size.is_some();
    if has_parent || has_size {
        let mut flags: u8 = 0;
        if has_parent {
            flags |= 0b01;
        }
        if has_size {
            flags |= 0b10;
        }
        w.u8(flags);
        if let Some(p) = &b.parent {
            w.str(p);
        }
        if let Some(sz) = &b.size {
            write_point(w, *sz);
        }
    }
}

fn write_update_commands(w: &mut Writer, b: &UpdateCommandsBody) {
    w.str(&b.id);
    w.varu(b.commands.len() as u64);
    for c in &b.commands {
        write_draw_cmd(w, c);
    }
}

fn write_update_command(w: &mut Writer, b: &UpdateCommandBody) {
    w.str(&b.id);
    w.varu(b.index as u64);
    write_draw_cmd(w, &b.command);
}

fn write_update_text(w: &mut Writer, b: &UpdateTextBody) {
    w.str(&b.id);
    w.varu(b.command_index as u64);
    match &b.range {
        UpdateTextRange::Whole => {
            w.u8(0);
        }
        UpdateTextRange::Range { start, end } => {
            w.u8(1);
            w.varu(*start as u64);
            w.varu(*end as u64);
        }
    }
    w.str(&b.replacement);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Rect;
    use crate::command::{parse, FontStyle};

    fn roundtrip(cmd: Command) {
        let body = encode_command(&cmd);
        let parsed = parse(frame_type_for(&cmd), &body).expect("encoded command must re-parse");
        // Compare by debug-string since most fields don't impl PartialEq.
        assert_eq!(format!("{cmd:?}"), format!("{parsed:?}"));
    }

    fn red() -> Color {
        Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }
    }

    #[test]
    fn probe_roundtrip() {
        roundtrip(Command::Probe);
    }

    #[test]
    fn clear_all_roundtrip() {
        roundtrip(Command::ClearAll);
    }

    #[test]
    fn create_element_with_rect_roundtrip() {
        roundtrip(Command::CreateElement(CreateElementBody {
            id: "foo".into(),
            commands: vec![DrawCmd::FillRectangles {
                fill: Style::Flat(red()),
                rects: vec![Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 2.0,
                    h: 1.0,
                }],
            }],
            origin: Point { x: 5.0, y: 3.0 },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
        }));
    }

    #[test]
    fn create_element_with_text_roundtrip() {
        roundtrip(Command::CreateElement(CreateElementBody {
            id: "label".into(),
            commands: vec![DrawCmd::DrawText {
                origin: Point { x: 0.0, y: 0.5 },
                align: Align::Center,
                fill: Style::Flat(red()),
                font_style: FontStyle(0x05), // Bold + Underline
                text: "hello".into(),
            }],
            origin: Point { x: 10.0, y: 4.0 },
            is_visible: true,
            draw_order: 1,
            parent: None,
            size: None,
        }));
    }

    #[test]
    fn fill_polygon_with_gradient_roundtrip() {
        roundtrip(Command::CreateElement(CreateElementBody {
            id: "tri".into(),
            commands: vec![DrawCmd::FillPolygon {
                fill: Style::LinearGradient {
                    p0: Point { x: 0.0, y: 0.0 },
                    p1: Point { x: 5.0, y: 5.0 },
                    c0: red(),
                    c1: Color {
                        r: 0.0,
                        g: 0.0,
                        b: 1.0,
                        a: 1.0,
                    },
                },
                points: vec![
                    Point { x: 0.0, y: 0.0 },
                    Point { x: 4.0, y: 0.0 },
                    Point { x: 2.0, y: 3.0 },
                ],
            }],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
        }));
    }

    #[test]
    fn outline_fill_path_roundtrip() {
        roundtrip(Command::CreateElement(CreateElementBody {
            id: "p".into(),
            commands: vec![DrawCmd::OutlineFillPath {
                fill: Style::Ref("accent".into()),
                stroke: Style::Flat(red()),
                line_width: 0.1,
                segments: vec![PathSegment {
                    start: Point { x: 0.0, y: 0.0 },
                    nodes: vec![
                        PathNode::LineTo { dst: Point { x: 1.0, y: 1.0 } },
                        PathNode::CubicBezierTo {
                            c0: Point { x: 0.5, y: 1.5 },
                            c1: Point { x: 1.5, y: 0.5 },
                            dst: Point { x: 2.0, y: 2.0 },
                        },
                        PathNode::ClosePath,
                    ],
                }],
            }],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
        }));
    }

    #[test]
    fn set_global_style_roundtrip() {
        roundtrip(Command::SetGlobalStyle {
            id: "accent".into(),
            style: ConcreteStyle::Flat(red()),
        });
    }

    #[test]
    fn upload_image_raw_roundtrip() {
        roundtrip(Command::UploadImage(UploadImageBody {
            id: "logo".into(),
            encoding: 0x01,
            width: 2,
            height: 2,
            data: vec![
                0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
                0xFF, 0xFF,
            ],
        }));
    }

    #[test]
    fn upload_image_webp_roundtrip() {
        // WebP body is opaque to the codec — exercise it with a small
        // fake byte buffer; round-trip just compares bytes.
        roundtrip(Command::UploadImage(UploadImageBody {
            id: "frame".into(),
            encoding: 0x02,
            width: 16,
            height: 16,
            data: (0..200).map(|i| (i & 0xFF) as u8).collect(),
        }));
    }

    #[test]
    fn drop_image_roundtrip() {
        roundtrip(Command::DropImage { id: "logo".into() });
    }

    #[test]
    fn update_image_roundtrip() {
        roundtrip(Command::UpdateImage(UpdateImageBody {
            id: "elem".into(),
            command_index: 3,
            new_image_id: "frame_2".into(),
        }));
    }

    #[test]
    fn create_element_with_parent_and_clip_roundtrip() {
        roundtrip(Command::CreateElement(CreateElementBody {
            id: "scrollable".into(),
            commands: vec![],
            origin: Point { x: 5.0, y: 3.0 },
            is_visible: true,
            draw_order: 0,
            parent: Some("root".into()),
            size: Some(Point { x: 40.0, y: 12.0 }),
        }));
    }

    #[test]
    fn create_element_parent_only_roundtrip() {
        roundtrip(Command::CreateElement(CreateElementBody {
            id: "child".into(),
            commands: vec![],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: 0,
            parent: Some("parent".into()),
            size: None,
        }));
    }

    #[test]
    fn create_element_size_only_roundtrip() {
        roundtrip(Command::CreateElement(CreateElementBody {
            id: "viewport".into(),
            commands: vec![],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: Some(Point { x: 80.0, y: 24.0 }),
        }));
    }

    #[test]
    fn update_size_roundtrip() {
        roundtrip(Command::UpdateSize {
            id: "viewport".into(),
            new_size: Point { x: 50.0, y: 10.0 },
        });
    }

    #[test]
    fn create_element_with_draw_image_roundtrip() {
        roundtrip(Command::CreateElement(CreateElementBody {
            id: "pic".into(),
            commands: vec![DrawCmd::DrawImage {
                target_rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 8.0,
                    h: 6.0,
                },
                image_id: "logo".into(),
            }],
            origin: Point { x: 5.0, y: 3.0 },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
        }));
    }

    #[test]
    fn build_envelope_round_trips_through_parser() {
        use crate::apc::ApcStream;
        use crate::codec::Reader;

        let env = build_envelope(&[
            (Command::Probe, 1),
            (Command::ClearAll, 2),
        ]);
        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);

        // Walk the unstuffed payload manually.
        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _len = r.u32().unwrap();
        // Frame 1: Probe, request_id 1, empty body.
        assert_eq!(r.u8().unwrap(), CMD_PROBE);
        assert_eq!(r.u32().unwrap(), 1);
        assert_eq!(r.u32().unwrap(), 0);
        // Frame 2: ClearAll, request_id 2, empty body.
        assert_eq!(r.u8().unwrap(), CMD_CLEAR_ALL);
        assert_eq!(r.u32().unwrap(), 2);
        assert_eq!(r.u32().unwrap(), 0);
        assert!(r.at_end());
    }
}
