//! Binary state snapshot for [`super::state::VgeEngine`] (VSS
//! extension's `VgeFragment` payload). Captures shared image / style
//! tables, both main and alternate element sets, the active-screen
//! flag, and engine-level cell dimensions / scale factor. Designed to
//! round-trip byte-equal so vsd can ship snapshots and the renderer
//! can rebuild engine state without re-parsing any VGE commands.
//!
//! See `doc/session-manager.md` §4.3 for the protocol-level role.
//!
//! The codec reuses `vge_protocol::codec::{Reader, Writer}` and the
//! protocol's typed `DrawCmd` / `ConcreteStyle` codecs (`write_draw_cmd`,
//! `read_draw_cmd`, `write_concrete_style`, `read_concrete_style`).

use std::cell::Cell;
use std::collections::HashMap;

use vge_protocol::codec::{Point as ProtoPoint, Reader, Writer};
use vge_protocol::command::read_draw_cmd;
use vge_protocol::command::read_concrete_style;
use vge_protocol::encode::{write_concrete_style, write_draw_cmd};

use super::state::{Element, ElementSet, SharedTables, UploadedImage, VgeState};

/// Bumped on any breaking change to the binary layout below. Strict
/// match — see [`super::state::VgeEngine::restore_from_binary_snapshot`].
///
/// History:
/// - v2: include `top_of_live_screen` so element `anchor_line` values
///   stay aligned with the receiving engine's `LineTracker`.
/// - v1: initial layout.
pub(crate) const SNAPSHOT_KIND_VERSION: u16 = 2;

/// Error returned when a VGE binary snapshot cannot be decoded.
#[derive(Debug, Clone)]
pub enum SnapshotError {
    /// The leading `u16 kind_version` did not match
    /// [`SNAPSHOT_KIND_VERSION`].
    KindVersionMismatch { got: u16, want: u16 },
    /// A typed primitive could not be decoded (truncated, malformed).
    BadPayload,
    /// All structured frames decoded but extra bytes remain at the
    /// payload tail.
    TrailingBytes,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KindVersionMismatch { got, want } => {
                write!(f, "VGE snapshot kind version mismatch: got {got}, want {want}")
            }
            Self::BadPayload => f.write_str("VGE snapshot payload malformed or truncated"),
            Self::TrailingBytes => f.write_str("VGE snapshot has trailing bytes"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<vge_protocol::codec::DecodeError> for SnapshotError {
    fn from(_: vge_protocol::codec::DecodeError) -> Self {
        SnapshotError::BadPayload
    }
}

// ---- VgeState ---------------------------------------------------------

pub(crate) fn encode_state(
    state: &VgeState,
    cell_px: (u16, u16),
    scale_factor: f32,
    top_of_live_screen: i64,
    out: &mut Vec<u8>,
) {
    let mut w = Writer::new();
    w.u16(SNAPSHOT_KIND_VERSION);

    w.u16(cell_px.0);
    w.u16(cell_px.1);
    w.f32(scale_factor);
    // The element `anchor_line` values reference absolute scrollback
    // line indices in the *source* engine's `LineTracker`. Ship the
    // value here so the receiving engine can pin its own tracker to
    // the same origin — without this, anchors render at row
    // `anchor_line - 0` instead of `anchor_line - top_of_live_screen`,
    // i.e. far off-screen or in the wrong row.
    w.i64(top_of_live_screen);

    encode_shared(&state.shared, &mut w);
    encode_element_set(&state.main, &mut w);
    if let Some(alt) = &state.alt {
        w.bool(true);
        encode_element_set(alt, &mut w);
    } else {
        w.bool(false);
    }
    w.bool(state.on_alt);

    out.extend_from_slice(&w.buf);
}

pub(crate) struct DecodedState {
    pub state: VgeState,
    pub cell_px: (u16, u16),
    pub scale_factor: f32,
    pub top_of_live_screen: i64,
}

pub(crate) fn decode_state(bytes: &[u8]) -> Result<DecodedState, SnapshotError> {
    let mut r = Reader::new(bytes);
    let kind = r.u16()?;
    if kind != SNAPSHOT_KIND_VERSION {
        return Err(SnapshotError::KindVersionMismatch {
            got: kind,
            want: SNAPSHOT_KIND_VERSION,
        });
    }

    let cell_px = (r.u16()?, r.u16()?);
    let scale_factor = r.f32()?;
    let top_of_live_screen = r.i64()?;

    let shared = decode_shared(&mut r)?;
    let main = decode_element_set(&mut r)?;
    let has_alt = r.bool()?;
    let alt = if has_alt {
        Some(decode_element_set(&mut r)?)
    } else {
        None
    };
    let on_alt = r.bool()?;
    if on_alt && alt.is_none() {
        return Err(SnapshotError::BadPayload);
    }
    if !r.at_end() {
        return Err(SnapshotError::TrailingBytes);
    }

    Ok(DecodedState {
        state: VgeState::from_raw_parts(shared, main, alt, on_alt),
        cell_px,
        scale_factor,
        top_of_live_screen,
    })
}

// ---- SharedTables -----------------------------------------------------

fn encode_shared(shared: &SharedTables, w: &mut Writer) {
    w.varu(shared.images.len() as u64);
    for (id, img) in &shared.images {
        w.str(id);
        encode_uploaded_image(img, w);
    }
    w.varu(shared.styles.len() as u64);
    for (id, style) in &shared.styles {
        w.str(id);
        write_concrete_style(w, style);
    }
}

fn decode_shared(r: &mut Reader) -> Result<SharedTables, SnapshotError> {
    let mut images = HashMap::new();
    let n_images = r.varu()? as usize;
    for _ in 0..n_images {
        let id = r.string()?.to_owned();
        let img = decode_uploaded_image(r)?;
        images.insert(id, img);
    }
    let mut styles = HashMap::new();
    let n_styles = r.varu()? as usize;
    for _ in 0..n_styles {
        let id = r.string()?.to_owned();
        let style = read_concrete_style(r)?;
        styles.insert(id, style);
    }
    Ok(SharedTables { styles, images })
}

// ---- UploadedImage ----------------------------------------------------

fn encode_uploaded_image(img: &UploadedImage, w: &mut Writer) {
    w.u32(img.width);
    w.u32(img.height);
    w.u8(img.source_encoding);
    w.bytes(&img.source_data);
    // GPU handle is renderer-private. `pixels` is the decoded form of
    // `source_data` and the renderer recomputes it lazily — we don't
    // ship either over the wire.
}

fn decode_uploaded_image(r: &mut Reader) -> Result<UploadedImage, SnapshotError> {
    let width = r.u32()?;
    let height = r.u32()?;
    let source_encoding = r.u8()?;
    let source_data = r.bytes()?.to_vec();
    // Decode the bytes to populate `pixels` on the receiving side so the
    // renderer doesn't have to redo it from scratch the first time it
    // paints the image. If the bytes were Raw (0x01) we can short-circuit;
    // for WebP (0x02) we have to ask `image` to decode.
    let pixels = super::state::decode_image_pixels_from_snapshot(
        source_encoding,
        width,
        height,
        &source_data,
    )?;
    Ok(UploadedImage {
        width,
        height,
        pixels,
        gpu: Cell::new(None),
        source_encoding,
        source_data,
    })
}

// ---- ElementSet -------------------------------------------------------

fn encode_element_set(set: &ElementSet, w: &mut Writer) {
    w.varu(set.elements.len() as u64);
    for (key, elem) in &set.elements {
        w.str(key);
        encode_element(elem, w);
    }
    w.u64(set.creation_counter);
    w.u64(set.next_anonymous);
}

fn decode_element_set(r: &mut Reader) -> Result<ElementSet, SnapshotError> {
    let n = r.varu()? as usize;
    let mut elements = HashMap::with_capacity(n);
    for _ in 0..n {
        let key = r.string()?.to_owned();
        let elem = decode_element(r)?;
        elements.insert(key, elem);
    }
    let creation_counter = r.u64()?;
    let next_anonymous = r.u64()?;
    Ok(ElementSet {
        elements,
        creation_counter,
        next_anonymous,
    })
}

// ---- Element ----------------------------------------------------------

fn encode_element(elem: &Element, w: &mut Writer) {
    match &elem.id {
        Some(s) => {
            w.bool(true);
            w.str(s);
        }
        None => w.bool(false),
    }
    w.varu(elem.commands.len() as u64);
    for cmd in &elem.commands {
        write_draw_cmd(w, cmd);
    }
    match &elem.parent {
        Some(s) => {
            w.bool(true);
            w.str(s);
        }
        None => w.bool(false),
    }
    w.varu(elem.children.len() as u64);
    for child in &elem.children {
        w.str(child);
    }
    match &elem.clip_size {
        Some(p) => {
            w.bool(true);
            w.f32(p.x);
            w.f32(p.y);
        }
        None => w.bool(false),
    }
    w.i64(elem.anchor_line);
    w.f32(elem.sub_row);
    w.f32(elem.origin_x);
    w.f32(elem.origin_y);
    w.bool(elem.is_visible);
    w.i32(elem.draw_order);
    w.u64(elem.creation_seq);
}

fn decode_element(r: &mut Reader) -> Result<Element, SnapshotError> {
    let id = if r.bool()? {
        Some(r.string()?.to_owned())
    } else {
        None
    };
    let n_cmds = r.varu()? as usize;
    let mut commands = Vec::with_capacity(n_cmds);
    for _ in 0..n_cmds {
        commands.push(read_draw_cmd(r)?);
    }
    let parent = if r.bool()? {
        Some(r.string()?.to_owned())
    } else {
        None
    };
    let n_children = r.varu()? as usize;
    let mut children = Vec::with_capacity(n_children);
    for _ in 0..n_children {
        children.push(r.string()?.to_owned());
    }
    let clip_size = if r.bool()? {
        Some(ProtoPoint {
            x: r.f32()?,
            y: r.f32()?,
        })
    } else {
        None
    };
    let anchor_line = r.i64()?;
    let sub_row = r.f32()?;
    let origin_x = r.f32()?;
    let origin_y = r.f32()?;
    let is_visible = r.bool()?;
    let draw_order = r.i32()?;
    let creation_seq = r.u64()?;
    Ok(Element {
        id,
        commands,
        parent,
        children,
        clip_size,
        anchor_line,
        sub_row,
        origin_x,
        origin_y,
        is_visible,
        draw_order,
        creation_seq,
    })
}
