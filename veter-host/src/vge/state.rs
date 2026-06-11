// VGE engine state: element table, scrollback line tracking, command
// dispatch, and PTY byte plumbing.

use std::cell::Cell;
use std::collections::HashMap;

use rgb::RGBA8;
use vge_protocol::apc::ApcStream;
use vge_protocol::codec::{Point, Reader, Transform};
use vge_protocol::command::{
    self, Command, ConcreteStyle, CreateElementBody, DrawCmd, UpdateCommandBody,
    UpdateCommandsBody, UpdateImageBody, UpdateTextBody, UpdateTextRange, UploadImageBody,
};
use vge_protocol::codec::Point as ProtoPoint;
use vge_protocol::envelope::{
    append_frame, err_body, wrap_t2c_envelope as wrap_envelope, ChunkAckBody, ProbeBody,
};
use vge_protocol::frame::*;

use crate::line_tracker::LineTracker;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_elements: u32,
    pub max_commands_per_element: u32,
    pub max_text_bytes: u32,
    pub max_image_bytes: u32,
    pub max_images: u32,
    pub supported_image_encodings: u8,
    pub max_nesting_depth: u8,
}

impl Default for Limits {
    fn default() -> Self {
        // Recommended budget (spec §10).
        Self {
            max_elements: 4096,
            max_commands_per_element: 4096,
            max_text_bytes: 1_048_576,
            max_image_bytes: 32 * 1024 * 1024,
            max_images: 1024,
            supported_image_encodings: 0b11, // bit0 Raw, bit1 WebP
            max_nesting_depth: 16,
        }
    }
}

/// Opaque renderer-side image handle. The host engine assigns and
/// stores these but never inspects them; the renderer maintains a
/// private mapping from `GpuImageId` to its own GPU texture handle
/// (e.g. `femtovg::ImageId`) and is responsible for creating /
/// deleting the GPU resource on the engine's behalf.
///
/// Decoupling from any particular renderer type keeps the host
/// engines GUI-free so the same code can run inside a headless
/// `vsd` process (see `doc/session-manager.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GpuImageId(pub u64);

/// An uploaded image, kept in CPU memory as straight-alpha RGBA8 plus a
/// lazily-populated renderer texture handle. The GPU side is created the
/// first time the renderer encounters this image; `DropImage` queues
/// `gpu` for deletion on the next frame.
///
/// `gpu` is `Cell<Option<GpuImageId>>` so the renderer can populate it
/// while only holding a `&VgeState` (the renderer doesn't need any
/// other mutation, and `GpuImageId` is `Copy`).
///
/// `source_encoding` and `source_data` retain the original wire-format
/// bytes alongside the decoded pixels so [`VgeEngine::binary_snapshot`]
/// can reship the image in its original form on reattach. Without this,
/// a 50 KiB WebP avatar would inflate to `width × height × 4` bytes
/// every time a renderer reconnects — punishing on bad SSH links.
pub struct UploadedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<RGBA8>,
    pub gpu: Cell<Option<GpuImageId>>,
    /// Encoding byte from the original `UploadImage` body (§8.1):
    /// `0x01` = Raw RGBA8, `0x02` = WebP. Used by `serialize_state`.
    pub source_encoding: u8,
    /// Encoded bytes as they arrived on the wire. For Raw uploads this
    /// matches what `pixels` was decoded from byte-for-byte; for WebP
    /// it's the much smaller compressed form.
    pub source_data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Element {
    /// Some(name) for client-named elements, None for anonymous (§6.1).
    /// Currently unread by the renderer but useful for debugging.
    #[allow(dead_code)]
    pub id: Option<String>,
    pub commands: Vec<DrawCmd>,
    /// Storage key of the parent element, if any. None = top-level
    /// (anchor_line / sub_row are meaningful). Some = child (origin_x /
    /// origin_y are parent-relative; anchor_line is unused).
    pub parent: Option<String>,
    /// Storage keys of direct children, in creation order. Maintained
    /// alongside `parent` so subtree traversal doesn't have to scan
    /// the whole element table.
    pub children: Vec<String>,
    /// Clip rect size (§9.2). If `Some`, descendants and the element's
    /// own commands are clipped to the rect at
    /// `(origin, origin + size)` in the element's coordinate space.
    pub clip_size: Option<ProtoPoint>,
    /// Affine transform (§9.11). Applies to the element's own commands
    /// and its entire subtree, about the element's origin. `None` =
    /// identity. The clip rect is exempt (stays axis-aligned in the
    /// untransformed space).
    pub transform: Option<Transform>,
    pub anchor_line: i64, // absolute scrollback line (top-level only)
    pub sub_row: f32,     // top-level only
    pub origin_x: f32,
    pub origin_y: f32, // for child elements; for top-level, redundant with sub_row
    pub is_visible: bool,
    pub draw_order: i32,
    pub creation_seq: u64,
}

/// Session-scoped state that is shared between the main and alternate
/// screens (§5.4). Image and style tables live here; only the element
/// table is per-screen.
pub struct SharedTables {
    pub styles: HashMap<String, ConcreteStyle>,
    pub images: HashMap<String, UploadedImage>,
}

impl SharedTables {
    pub fn new() -> Self {
        Self {
            styles: HashMap::new(),
            images: HashMap::new(),
        }
    }
}

/// Per-screen element table plus its monotonic counters. The main and
/// alternate screen each own one of these (§5.4).
pub struct ElementSet {
    pub elements: HashMap<String, Element>,
    pub creation_counter: u64,
    pub next_anonymous: u64,
}

impl ElementSet {
    pub fn new() -> Self {
        Self {
            elements: HashMap::new(),
            creation_counter: 0,
            next_anonymous: 0,
        }
    }

    fn next_seq(&mut self) -> u64 {
        let n = self.creation_counter;
        self.creation_counter += 1;
        n
    }

    fn anonymous_key(&mut self) -> String {
        let n = self.next_anonymous;
        self.next_anonymous += 1;
        format!("\0anon\0{n}")
    }
}

pub struct VgeState {
    pub shared: SharedTables,
    pub(in crate::vge) main: ElementSet,
    pub(in crate::vge) alt: Option<ElementSet>,
    pub(in crate::vge) on_alt: bool,
}

impl VgeState {
    pub fn new() -> Self {
        Self {
            shared: SharedTables::new(),
            main: ElementSet::new(),
            alt: None,
            on_alt: false,
        }
    }

    /// Build a [`VgeState`] from parts decoded out of a binary
    /// snapshot. The decoder in [`crate::vge::snapshot`] is the
    /// intended caller — splitting state assembly from decoding lets
    /// the decoder live in its own module without needing access to
    /// private fields.
    pub(in crate::vge) fn from_raw_parts(
        shared: SharedTables,
        main: ElementSet,
        alt: Option<ElementSet>,
        on_alt: bool,
    ) -> Self {
        Self { shared, main, alt, on_alt }
    }

    /// True iff the engine is currently on the alternate screen.
    pub fn on_alt(&self) -> bool {
        self.on_alt
    }

    /// Borrow the active element set (current screen).
    pub fn current(&self) -> &ElementSet {
        if self.on_alt {
            self.alt.as_ref().expect("on_alt without alt set")
        } else {
            &self.main
        }
    }

    fn current_mut(&mut self) -> &mut ElementSet {
        if self.on_alt {
            self.alt.as_mut().expect("on_alt without alt set")
        } else {
            &mut self.main
        }
    }

    /// Convenience accessor for the current screen's element table.
    pub fn elements(&self) -> &HashMap<String, Element> {
        &self.current().elements
    }

    /// Mutable accessor for the current screen's element table.
    pub fn elements_mut(&mut self) -> &mut HashMap<String, Element> {
        &mut self.current_mut().elements
    }

    /// Iterate top-level elements (parent: None) on the current screen
    /// in render order: ascending (draw_order, creation_seq). Children
    /// are walked recursively from the renderer per §9.8.
    pub fn top_level_sorted(&self) -> Vec<&Element> {
        let mut v: Vec<&Element> = self
            .current()
            .elements
            .values()
            .filter(|e| e.parent.is_none())
            .collect();
        v.sort_by_key(|e| (e.draw_order, e.creation_seq));
        v
    }

    /// Iterate the direct children of `parent_key` on the current
    /// screen in render order.
    pub fn children_sorted(&self, parent_key: &str) -> Vec<&Element> {
        let elements = &self.current().elements;
        let parent = match elements.get(parent_key) {
            Some(p) => p,
            None => return Vec::new(),
        };
        let mut v: Vec<&Element> = parent
            .children
            .iter()
            .filter_map(|k| elements.get(k))
            .collect();
        v.sort_by_key(|e| (e.draw_order, e.creation_seq));
        v
    }

    /// Switch to the alt screen with a fresh empty element set, per
    /// §5.4. No-op if already on alt.
    pub fn enter_alt_screen(&mut self) {
        if !self.on_alt {
            self.alt = Some(ElementSet::new());
            self.on_alt = true;
        }
    }

    /// Drop the alt set and restore main, per §5.4. No-op if already on
    /// main.
    pub fn leave_alt_screen(&mut self) {
        if self.on_alt {
            self.alt = None;
            self.on_alt = false;
        }
    }

    /// Wipe everything for §5.6 reset (RIS / DECSTR). Returns any GPU
    /// image handles whose CPU-side counterparts are now gone, so the
    /// caller can free them on the canvas.
    pub fn reset(&mut self) -> Vec<GpuImageId> {
        let mut deletes = Vec::new();
        for (_, img) in self.shared.images.drain() {
            if let Some(gpu) = img.gpu.get() {
                deletes.push(gpu);
            }
        }
        self.shared.styles.clear();
        self.main = ElementSet::new();
        self.alt = None;
        self.on_alt = false;
        deletes
    }
}

/// Decode an UploadImage Raw payload (§8.1, encoding 0x01). Bytes must
/// equal `width*height*4` straight-alpha RGBA8 octets.
fn decode_raw_rgba8(
    data: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<RGBA8>, (u16, &'static str)> {
    let expected = (width as u64) * (height as u64) * 4;
    if data.len() as u64 != expected {
        return Err((ERR_BAD_PAYLOAD, "raw image byte count != width*height*4"));
    }
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for chunk in data.chunks_exact(4) {
        pixels.push(RGBA8::new(chunk[0], chunk[1], chunk[2], chunk[3]));
    }
    Ok(pixels)
}

/// Snapshot-decode entry point. Same Raw / WebP dispatch as the
/// upload path, but maps the protocol-level error into a snapshot
/// error so the VSS engine surfaces a single error type.
pub(in crate::vge) fn decode_image_pixels_from_snapshot(
    encoding: u8,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<Vec<RGBA8>, crate::vge::snapshot::SnapshotError> {
    let res = match encoding {
        0x01 => decode_raw_rgba8(data, width, height),
        0x02 => decode_webp(data, width, height),
        _ => return Err(crate::vge::snapshot::SnapshotError::BadPayload),
    };
    res.map_err(|_| crate::vge::snapshot::SnapshotError::BadPayload)
}

/// Decode an UploadImage WebP payload (§8.1, encoding 0x02). Decoded
/// dimensions must match the announced width/height; mismatch or any
/// decoder error → `err_image_decode`.
fn decode_webp(
    data: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<RGBA8>, (u16, &'static str)> {
    let img = image::load_from_memory_with_format(data, image::ImageFormat::WebP)
        .map_err(|_| (ERR_IMAGE_DECODE, "WebP decode failed"))?;
    if img.width() != width || img.height() != height {
        return Err((ERR_IMAGE_DECODE, "WebP dimensions do not match announced w/h"));
    }
    let rgba = img.to_rgba8();
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for chunk in rgba.as_raw().chunks_exact(4) {
        pixels.push(RGBA8::new(chunk[0], chunk[1], chunk[2], chunk[3]));
    }
    Ok(pixels)
}

/// In-flight chunked image upload (§8.1). Allocated on the first chunk
/// (`chunk_offset == 0`), grown by subsequent chunks, finalized — i.e.
/// decoded + inserted into `shared.images` — on the chunk with
/// `is_last = true`.
///
/// Transient: dropped on `VgeState::reset()` and on snapshot restore.
/// Not snapshotted (a fresh attach cannot honor a sender's mid-stream
/// chunk sequence anyway).
struct PendingUpload {
    encoding: u8,
    width: u32,
    height: u32,
    total_bytes: u32,
    buf: Vec<u8>,
    bytes_received: u32,
}

/// Host-provided accent palette seeded into the reserved `host.*`
/// style namespace (`doc/vector-graphics-extension.md` §7.3). An empty
/// palette means host-themed styles are disabled: nothing is injected
/// and the PRT probe does not advertise `FEAT_VGE_HOST_THEMED_STYLES`.
#[derive(Debug, Clone, Default)]
pub struct HostThemePalette {
    /// Ordered accent slots. `host.accent.{n}` resolves to `accents[n-1]`;
    /// the contextual `host.accent` resolves to `accents[depth % len]`.
    pub accents: Vec<command::Color>,
}

impl HostThemePalette {
    pub fn is_empty(&self) -> bool {
        self.accents.is_empty()
    }

    /// The contextual accent (`host.accent`) for a VGE engine at the given
    /// tree depth, as a straight RGBA8 quad. `None` for an empty palette.
    /// Clients read this from the probe to derive their own shades from
    /// the same accent they reference by `StyleRef`.
    pub fn contextual_rgba8(&self, depth: u32) -> Option<[u8; 4]> {
        if self.accents.is_empty() {
            return None;
        }
        let c = self.accents[(depth as usize) % self.accents.len()];
        let q = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u8;
        Some([q(c.r), q(c.g), q(c.b), q(c.a)])
    }

    /// Style-table entries this palette contributes for a VGE engine at
    /// the given tree depth (host = 0). Empty when the palette is empty.
    fn entries(&self, depth: u32) -> Vec<(String, ConcreteStyle)> {
        if self.accents.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.accents.len() + 1);
        let idx = (depth as usize) % self.accents.len();
        out.push(("host.accent".to_string(), ConcreteStyle::Flat(self.accents[idx])));
        for (i, c) in self.accents.iter().enumerate() {
            out.push((format!("host.accent.{}", i + 1), ConcreteStyle::Flat(*c)));
        }
        out
    }
}

pub struct VgeEngine {
    apc: ApcStream,
    pub state: VgeState,
    pub limits: Limits,
    /// Host accent palette + this engine's tree depth, if seeded. Re-applied
    /// after RIS/DECSTR wipes the style table so `host.*` entries persist.
    host_seed: Option<(HostThemePalette, u32)>,
    cell_px: (u16, u16),
    scale_factor: f32,
    line_tracker: LineTracker,
    pending_response_bytes: Vec<u8>,
    /// Chunked image uploads still in flight, keyed by image id.
    pending_uploads: HashMap<String, PendingUpload>,
    /// Renderer image handles for uploaded images that have been dropped
    /// but whose GPU resources still need releasing. The renderer drains
    /// this on each frame and translates each `GpuImageId` to its own
    /// GPU texture handle to call `delete_image` (or equivalent).
    pending_image_deletes: Vec<GpuImageId>,
    /// Number of `\x1b[6n` DSR cursor-position queries seen in the
    /// byte stream that haven't been answered yet. We need to wait
    /// until vt100 has processed the chunk so the reply reflects
    /// post-process cursor state.
    pending_cursor_queries: u32,
    /// When `false`, `\x1b[6n` queries observed in the byte stream are
    /// not counted into `pending_cursor_queries` and so produce no
    /// auto-reply. PRT in v1 uses this for per-portal VGE engines so
    /// that PRT remains the sole DSR responder inside a portal —
    /// otherwise both PRT and the per-portal VGE would synthesise a
    /// reply for the same query and the inner program would see two.
    auto_reply_dsr: bool,
    /// When `false`, every VGE command is still parsed and applied
    /// (so engine state stays consistent — for snapshot replay etc.)
    /// but **no** response frame is generated. Used by vsd's
    /// session VGE engine: vsd is a state-mirroring middleman, not
    /// the authoritative host, so it must not double-answer Probe,
    /// UploadImage, CreateElement, etc. The real host upstream
    /// (e.g. local veter's per-portal VGE for the SSH pane) is the
    /// sole responder. Without this, the inner program (vcat) gets
    /// two responses to each command; it consumes one and exits, and
    /// the leftover bytes get read by the shell that takes over the
    /// inner PTY — which interprets payload bytes like `0x12` (Ctrl-R
    /// in the payload_len header of a 2-RSP_OK envelope = 18 bytes,
    /// or in the cell_pixel_height field of a ProbeResponse) as
    /// keystrokes, triggering reverse-i-search and other surprises.
    auto_reply_commands: bool,
}

impl VgeEngine {
    pub fn new(cell_px: (u16, u16), scale_factor: f32) -> Self {
        Self {
            apc: ApcStream::new(),
            state: VgeState::new(),
            limits: Limits::default(),
            host_seed: None,
            cell_px,
            scale_factor,
            line_tracker: LineTracker::new(),
            pending_response_bytes: Vec::new(),
            pending_uploads: HashMap::new(),
            pending_image_deletes: Vec::new(),
            pending_cursor_queries: 0,
            auto_reply_dsr: true,
            auto_reply_commands: true,
        }
    }

    /// Seed the reserved `host.*` style namespace from a host-provided
    /// palette, keyed on this engine's `depth` in the portal tree
    /// (`doc/vector-graphics-extension.md` §7.3). The seed is retained so
    /// it can be re-applied after RIS/DECSTR clears the style table.
    /// A no-op for an empty palette.
    pub fn seed_host_styles(&mut self, palette: HostThemePalette, depth: u32) {
        if palette.is_empty() {
            return;
        }
        self.host_seed = Some((palette, depth));
        self.apply_host_styles();
    }

    /// (Re-)write the seeded `host.*` entries into the style table.
    fn apply_host_styles(&mut self) {
        if let Some((palette, depth)) = &self.host_seed {
            for (id, style) in palette.entries(*depth) {
                self.state.shared.styles.insert(id, style);
            }
        }
    }

    /// Disable DSR cursor-query auto-replies. See the field doc on
    /// `auto_reply_dsr` — used by per-portal VGE engines.
    pub fn set_auto_reply_dsr(&mut self, enabled: bool) {
        self.auto_reply_dsr = enabled;
    }

    /// Toggle VGE command auto-replies. See the field doc on
    /// `auto_reply_commands` — vsd uses this so the upstream real
    /// host is the sole responder and the inner program doesn't get
    /// two response envelopes per command.
    pub fn set_auto_reply_commands(&mut self, enabled: bool) {
        self.auto_reply_commands = enabled;
    }

    /// Hand off any image GPU handles whose owners have been dropped.
    /// The renderer should call `canvas.delete_image(id)` for each.
    pub fn take_pending_image_deletes(&mut self) -> Vec<GpuImageId> {
        std::mem::take(&mut self.pending_image_deletes)
    }

    /// Update reported cell pixel dimensions (e.g. on resize/HiDPI change).
    #[allow(dead_code)]
    pub fn set_dimensions(&mut self, cell_px: (u16, u16), scale_factor: f32) {
        self.cell_px = cell_px;
        self.scale_factor = scale_factor;
    }

    pub fn top_of_live_screen(&self) -> i64 {
        self.line_tracker.top_of_live_screen
    }

    /// Serialize the engine's full state as a binary blob for the VSS
    /// extension's `VgeFragment` payload. Captures both main and
    /// alternate element sets, the shared image / style tables (with
    /// original `source_data` preserved), `on_alt`, and engine-level
    /// `cell_px` / `scale_factor`. Decode with
    /// [`Self::restore_from_binary_snapshot`].
    ///
    /// Side-effect-free; does not consume any pending responses or
    /// touch line-tracker / DSR state.
    #[must_use]
    pub fn binary_snapshot(&self) -> Vec<u8> {
        let mut out = Vec::new();
        crate::vge::snapshot::encode_state(
            &self.state,
            self.cell_px,
            self.scale_factor,
            self.line_tracker.top_of_live_screen,
            &mut out,
        );
        out
    }

    /// Replace this engine's state with one decoded from a VSS
    /// snapshot produced by [`Self::binary_snapshot`]. Returns an
    /// error on version mismatch or malformed payload; on error the
    /// engine state is left untouched.
    ///
    /// Side-effect-free: no responses are queued, no callbacks fire,
    /// and the line tracker / DSR pending counts are reset to the
    /// values implied by the snapshot — image GPU handles are left
    /// `None` so the renderer lazily registers them on first paint.
    /// `pending_response_bytes`, `pending_image_deletes`,
    /// `pending_cursor_queries`, and the `auto_reply_*` flags retain
    /// their previous values (they're engine-policy state, not
    /// session state).
    pub fn restore_from_binary_snapshot(
        &mut self,
        bytes: &[u8],
    ) -> Result<(), crate::vge::snapshot::SnapshotError> {
        let decoded = crate::vge::snapshot::decode_state(bytes)?;
        self.state = decoded.state;
        self.cell_px = decoded.cell_px;
        self.scale_factor = decoded.scale_factor;
        // Reset transient state that doesn't belong to the snapshot:
        // any in-flight responses or cursor queries from before the
        // restore are stale.
        self.pending_response_bytes.clear();
        self.pending_image_deletes.clear();
        self.pending_uploads.clear();
        self.pending_cursor_queries = 0;
        // Pin top_of_live_screen to the snapshotted value so element
        // `anchor_line`s line up with where they were drawn in the
        // source engine. The rest of LineTracker stays uninitialised
        // so the next `update()` call (re-)synchronises against this
        // engine's own parser scrollback.
        self.line_tracker = LineTracker::new();
        self.line_tracker.top_of_live_screen = decoded.top_of_live_screen;
        Ok(())
    }

    /// Ingest raw PTY bytes. Returns the passthrough byte slice that
    /// should be forwarded to vt100. Any complete VGE envelopes are
    /// processed and their responses queued in `take_responses()`.
    /// Side-channel events from the APC parser (resets) are applied to
    /// engine state immediately.
    pub fn process_pty_chunk(&mut self, input: &[u8]) -> Vec<u8> {
        let out = self.apc.feed(input);
        for payload in out.payloads {
            self.handle_envelope_payload(&payload);
        }
        for ev in out.events {
            self.handle_terminal_event(ev);
        }
        out.passthrough
    }

    /// React to a side-channel terminal event observed in the byte
    /// stream (resets, cursor-position queries, etc.).
    fn handle_terminal_event(&mut self, ev: vge_protocol::TerminalEvent) {
        use vge_protocol::TerminalEvent::*;
        match ev {
            HardReset | SoftReset => {
                let deletes = self.state.reset();
                self.pending_image_deletes.extend(deletes);
                self.pending_uploads.clear();
                // Reset the line tracker too: scrollback state will be
                // re-derived after vt100 finishes its own reset.
                self.line_tracker = LineTracker::new();
                // §7.3 — RIS/DECSTR wipes the style table; the host re-seeds
                // its reserved `host.*` entries so clients' StyleRefs survive.
                self.apply_host_styles();
            }
            CursorPositionQuery => {
                // Queue; we reply after vt100 processes the chunk so
                // the cursor position reflects post-chunk state.
                if self.auto_reply_dsr {
                    self.pending_cursor_queries += 1;
                }
            }
            EraseDisplay => {
                // vt100 wipes the cells in place but doesn't push them
                // to scrollback, so top_of_live_screen is unchanged.
                // Drop every top-level element anchored at or after
                // top_of_live_screen — those are the ones living in
                // the now-blank live region.
                self.drop_top_level_where(|el, top| el.anchor_line >= top);
            }
            EraseScrollback => {
                // Wipe elements anchored above the live region. Pairs
                // with `clear(1)` which emits `2J` followed by `3J`.
                self.drop_top_level_where(|el, top| el.anchor_line < top);
            }
        }
    }

    /// Delete every top-level element on the current screen for which
    /// `pred(el, top_of_live_screen)` returns true. Children cascade.
    fn drop_top_level_where(&mut self, pred: impl Fn(&Element, i64) -> bool) {
        let top = self.line_tracker.top_of_live_screen;
        let to_delete: Vec<String> = self
            .state
            .elements()
            .iter()
            .filter(|(_, e)| e.parent.is_none() && pred(e, top))
            .map(|(k, _)| k.clone())
            .collect();
        for id in to_delete {
            self.delete_subtree(&id);
        }
    }

    /// Take queued response bytes (an APC envelope) ready to write to the
    /// PTY master.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response_bytes)
    }

    /// Reply to any pending DSR cursor-position queries with the
    /// current cursor location. vt100 reports 0-indexed; DSR is
    /// 1-indexed. The bytes are queued in `pending_response_bytes`,
    /// which the host writes to the PTY master alongside VGE
    /// response envelopes.
    fn answer_pending_cursor_queries<CB: vt100::Callbacks>(
        &mut self,
        parser: &vt100::Parser<CB>,
    ) {
        if self.pending_cursor_queries == 0 {
            return;
        }
        let (row, col) = parser.screen().cursor_position();
        let resp = format!("\x1b[{};{}R", row as u32 + 1, col as u32 + 1);
        for _ in 0..self.pending_cursor_queries {
            self.pending_response_bytes.extend_from_slice(resp.as_bytes());
        }
        self.pending_cursor_queries = 0;
    }

    /// Update top-of-live-screen tracking and react to alt-screen
    /// transitions. Call after every `parser.process(...)`. Also evicts
    /// elements whose anchor_line has fallen off the bottom of
    /// scrollback (main screen only — alt screen has no scrollback).
    pub fn after_vt100_process<CB: vt100::Callbacks>(
        &mut self,
        parser: &mut vt100::Parser<CB>,
    ) {
        // §5.4 — detect screen transitions by polling vt100.
        let now_alt = parser.screen().alternate_screen();
        if now_alt && !self.state.on_alt() {
            self.state.enter_alt_screen();
        } else if !now_alt && self.state.on_alt() {
            self.state.leave_alt_screen();
        }

        // Reply to DSR queries (cursor position is now post-process).
        self.answer_pending_cursor_queries(parser);

        // Scrollback anchoring is only meaningful on the main screen.
        if !self.state.on_alt() {
            self.line_tracker.update(parser);
            self.evict(parser.screen().scrollback_fill());
        }
    }

    fn evict(&mut self, scrollback_fill: usize) {
        if scrollback_fill == 0 && self.line_tracker.top_of_live_screen == 0 {
            // Nothing has ever scrolled; keep pre-scroll anchors (e.g.
            // a tall image whose origin reached above row 0) alive.
            return;
        }
        let oldest_visible =
            self.line_tracker.top_of_live_screen - scrollback_fill as i64;
        // Eviction applies only to top-level elements. Their subtrees
        // cascade.
        let to_evict: Vec<String> = self
            .state
            .elements()
            .iter()
            .filter(|(_, e)| e.parent.is_none() && e.anchor_line < oldest_visible)
            .map(|(k, _)| k.clone())
            .collect();
        for key in to_evict {
            self.delete_subtree(&key);
        }
    }

    fn handle_envelope_payload(&mut self, payload: &[u8]) {
        let mut frames_buf: Vec<u8> = Vec::new();

        let mut r = Reader::new(payload);
        let version = match r.u8() {
            Ok(v) => v,
            Err(_) => return, // can't even respond — corrupt envelope
        };
        if version > PROTOCOL_VERSION {
            // We can't safely parse a future version; respond with
            // unsupported_version and request_id 0.
            append_frame(
                &mut frames_buf,
                RSP_ERR,
                0,
                &err_body(ERR_UNSUPPORTED_VERSION, "protocol_version too new"),
            );
            self.queue_envelope(frames_buf);
            return;
        }
        let payload_len = match r.u32() {
            Ok(n) => n as usize,
            Err(_) => return,
        };
        if r.remaining() < payload_len {
            return;
        }

        let header_end = r.pos();
        let frames_slice = &payload[header_end..header_end + payload_len];
        let mut fr = Reader::new(frames_slice);

        while !fr.at_end() {
            let frame_type = match fr.u8() {
                Ok(t) => t,
                Err(_) => break,
            };
            let request_id = match fr.u32() {
                Ok(v) => v,
                Err(_) => break,
            };
            let body_len = match fr.u32() {
                Ok(n) => n as usize,
                Err(_) => break,
            };
            let body = match fr.take(body_len) {
                Ok(b) => b,
                Err(_) => break,
            };

            self.dispatch_frame(frame_type, request_id, body, &mut frames_buf);
        }

        if !frames_buf.is_empty() {
            self.queue_envelope(frames_buf);
        }
    }

    fn dispatch_frame(
        &mut self,
        frame_type: u8,
        request_id: u32,
        body: &[u8],
        out_frames: &mut Vec<u8>,
    ) {
        // `REQ_ID_NO_RESPONSE` (see vge-protocol §4) is the sender's
        // explicit "apply but don't ack" sentinel — used by
        // `serialize_state` for snapshot replay so the renderer's
        // per-portal engine doesn't echo ack frames back through the
        // PRT chain into the inner program's PTY.
        let quiet = !self.auto_reply_commands || request_id == REQ_ID_NO_RESPONSE;
        match command::parse(frame_type, body) {
            Err(code) => {
                if !quiet {
                    append_frame(out_frames, RSP_ERR, request_id, &err_body(code, ""));
                }
            }
            Ok(cmd) => {
                let result = self.apply_command(cmd);
                if quiet {
                    // State changes are already applied; skip the
                    // response frame entirely.
                    return;
                }
                match result {
                    Ok(rsp_body) => {
                        let frame_type = match frame_type {
                            CMD_PROBE => RSP_PROBE,
                            // UploadImage is chunked — each chunk
                            // produces a ChunkAck (§4) so the sender can
                            // drive a progress UI. cmd_upload_image
                            // emits the ChunkAck body; on the final
                            // chunk (image registered) the body still
                            // carries `bytes_received == total_bytes`,
                            // signaling "done."
                            CMD_UPLOAD_IMAGE => RSP_CHUNK_ACK,
                            _ => RSP_OK,
                        };
                        append_frame(out_frames, frame_type, request_id, &rsp_body);
                    }
                    Err((code, msg)) => {
                        append_frame(
                            out_frames,
                            RSP_ERR,
                            request_id,
                            &err_body(code, msg),
                        );
                    }
                }
            }
        }
    }

    fn apply_command(&mut self, cmd: Command) -> Result<Vec<u8>, (u16, &'static str)> {
        match cmd {
            Command::Probe => {
                // dispatch_frame already swallowed the envelope if
                // auto_reply_probe is off; if we get here, we want
                // to respond.
                let pb = ProbeBody {
                    protocol_version: PROTOCOL_VERSION as u16,
                    cell_pixel_width: self.cell_px.0,
                    cell_pixel_height: self.cell_px.1,
                    scale_factor: self.scale_factor,
                    max_elements: self.limits.max_elements,
                    max_commands_per_element: self.limits.max_commands_per_element,
                    max_text_bytes: self.limits.max_text_bytes,
                    max_image_bytes: self.limits.max_image_bytes,
                    max_images: self.limits.max_images,
                    supported_image_encodings: self.limits.supported_image_encodings,
                    max_nesting_depth: self.limits.max_nesting_depth,
                };
                Ok(pb.encode())
            }
            Command::CreateElement(b) => self.cmd_create_element(b),
            Command::DeleteElement { id } => self.cmd_delete_element(&id),
            Command::UpdateCommands(b) => self.cmd_update_commands(b),
            Command::UpdateCommand(b) => self.cmd_update_command(b),
            Command::UpdateText(b) => self.cmd_update_text(b),
            Command::UpdateOrigin { id, origin } => self.cmd_update_origin(&id, origin),
            Command::UpdateVisibility { id, is_visible } => {
                self.cmd_update_visibility(&id, is_visible)
            }
            Command::UpdateDrawOrder { id, draw_order } => {
                self.cmd_update_draw_order(&id, draw_order)
            }
            Command::ClearAll => {
                self.state.elements_mut().clear();
                Ok(Vec::new())
            }
            Command::SetGlobalStyle { id, style } => self.cmd_set_global_style(id, style),
            Command::UploadImage(b) => self.cmd_upload_image(b),
            Command::DropImage { id } => self.cmd_drop_image(&id),
            Command::UpdateImage(b) => self.cmd_update_image(b),
            Command::UpdateSize { id, new_size } => self.cmd_update_size(&id, new_size),
            Command::UpdateTransform { id, transform } => {
                self.cmd_update_transform(&id, transform)
            }
        }
    }

    fn cmd_update_size(
        &mut self,
        id: &str,
        new_size: ProtoPoint,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        if id.is_empty() {
            return Err((ERR_BAD_PAYLOAD, "empty id"));
        }
        if !new_size.x.is_finite() || !new_size.y.is_finite()
            || new_size.x < 0.0 || new_size.y < 0.0
        {
            return Err((ERR_BAD_PAYLOAD, "size must be finite and non-negative"));
        }
        let el = self
            .state
            .elements_mut()
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        el.clip_size = Some(new_size);
        Ok(Vec::new())
    }

    fn cmd_update_transform(
        &mut self,
        id: &str,
        transform: Transform,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        if id.is_empty() {
            return Err((ERR_BAD_PAYLOAD, "empty id"));
        }
        if !transform.is_finite() {
            return Err((ERR_BAD_PAYLOAD, "transform must be finite"));
        }
        let el = self
            .state
            .elements_mut()
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        el.transform = Some(transform);
        Ok(Vec::new())
    }

    fn cmd_set_global_style(
        &mut self,
        id: String,
        style: ConcreteStyle,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        // ID validation already done by the parser (non-empty, ≤64 bytes).
        // §7.3 — the `host.*` namespace is host-owned; clients may not write it.
        if id.starts_with("host.") {
            return Err((ERR_RESERVED_STYLE_ID, "host.* style ids are host-owned"));
        }
        // Upsert per §7.3 — no error on existing ID.
        self.state.shared.styles.insert(id, style);
        Ok(Vec::new())
    }

    fn cmd_upload_image(
        &mut self,
        b: UploadImageBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        // ID rules (§6.8 / §8.2). Parser already enforces non-empty.
        if b.id.is_empty() || b.id.len() > 64 {
            return Err((ERR_BAD_PAYLOAD, "image id"));
        }

        // Bounds (apply to every chunk so we fail fast on a corrupt
        // mid-stream frame, not just the first).
        let data_len = b.data.len() as u32;
        let end = b.chunk_offset.checked_add(data_len).ok_or((
            ERR_BAD_PAYLOAD,
            "chunk overflow",
        ))?;
        if end > b.total_bytes {
            return Err((ERR_BAD_PAYLOAD, "chunk past total_bytes"));
        }

        if b.chunk_offset == 0 {
            // First chunk for this id — fresh allocation. Validate
            // against the live image table and pending uploads.
            if self.state.shared.images.contains_key(&b.id) {
                return Err((ERR_DUPLICATE_IMAGE_ID, "image id in use"));
            }
            if self.pending_uploads.contains_key(&b.id) {
                return Err((ERR_DUPLICATE_IMAGE_ID, "upload already in progress"));
            }
            // Image budget counts both finalized images and pending
            // uploads — otherwise a sender could exhaust memory by
            // starting `max_images` uploads and never finishing them.
            let live = self.state.shared.images.len() + self.pending_uploads.len();
            if live as u32 >= self.limits.max_images {
                return Err((ERR_TOO_MANY_IMAGES, "image budget exhausted"));
            }
            if (b.total_bytes as u64) > self.limits.max_image_bytes as u64 {
                return Err((ERR_IMAGE_TOO_LARGE, "image exceeds max_image_bytes"));
            }
            // Encoding sniff: reject unknown encodings up front rather
            // than waiting until the last chunk to decode and fail.
            if !matches!(b.encoding, 0x01 | 0x02) {
                return Err((ERR_BAD_PAYLOAD, "unknown image encoding"));
            }

            let mut pending = PendingUpload {
                encoding: b.encoding,
                width: b.width,
                height: b.height,
                total_bytes: b.total_bytes,
                buf: vec![0; b.total_bytes as usize],
                bytes_received: 0,
            };
            pending.buf[..data_len as usize].copy_from_slice(&b.data);
            pending.bytes_received = data_len;

            // Single-shot (offset=0, is_last=true) or stream — branch
            // on the last-flag here so the finalize path is shared.
            return self.absorb_or_finalize(b.id, pending, b.is_last);
        }

        // Subsequent chunk — must have a matching in-flight upload.
        let mut pending = self
            .pending_uploads
            .remove(&b.id)
            .ok_or((ERR_BAD_PAYLOAD, "no upload in progress for id"))?;

        if pending.encoding != b.encoding
            || pending.width != b.width
            || pending.height != b.height
            || pending.total_bytes != b.total_bytes
            || pending.bytes_received != b.chunk_offset
        {
            // Metadata or order drifted — drop the buffer (we already
            // removed it above) and surface the error.
            return Err((ERR_BAD_PAYLOAD, "chunk does not match upload"));
        }

        pending.buf[b.chunk_offset as usize..end as usize].copy_from_slice(&b.data);
        pending.bytes_received = end;

        self.absorb_or_finalize(b.id, pending, b.is_last)
    }

    /// Either store the buffer back as pending (if more chunks remain)
    /// or finalize it: decode → insert into `shared.images`. In both
    /// cases the response body is a [`ChunkAckBody`] carrying the
    /// cumulative `bytes_received` for the image id.
    fn absorb_or_finalize(
        &mut self,
        id: String,
        pending: PendingUpload,
        is_last: bool,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        if !is_last {
            let bytes_received = pending.bytes_received;
            self.pending_uploads.insert(id.clone(), pending);
            return Ok(ChunkAckBody {
                image_id: &id,
                bytes_received,
            }
            .encode());
        }

        // Last chunk: must have the full payload.
        if pending.bytes_received != pending.total_bytes {
            return Err((ERR_BAD_PAYLOAD, "is_last with bytes_received < total"));
        }

        let pixels = match pending.encoding {
            0x01 => decode_raw_rgba8(&pending.buf, pending.width, pending.height)?,
            0x02 => decode_webp(&pending.buf, pending.width, pending.height)?,
            _ => return Err((ERR_BAD_PAYLOAD, "unknown image encoding")),
        };

        // Preserve the on-wire form so snapshot replay can reship the
        // image in its original encoding rather than inflating to RGBA8.
        let bytes_received = pending.bytes_received;
        let source_encoding = pending.encoding;
        let source_data = pending.buf;
        self.state.shared.images.insert(
            id.clone(),
            UploadedImage {
                width: pending.width,
                height: pending.height,
                pixels,
                gpu: Cell::new(None),
                source_encoding,
                source_data,
            },
        );
        Ok(ChunkAckBody {
            image_id: &id,
            bytes_received,
        }
        .encode())
    }

    fn cmd_drop_image(&mut self, id: &str) -> Result<Vec<u8>, (u16, &'static str)> {
        if id.is_empty() {
            return Err((ERR_BAD_PAYLOAD, "empty image id"));
        }
        match self.state.shared.images.remove(id) {
            None => {
                // §8.2: DropImage on an in-progress chunked upload
                // aborts it and releases the id.
                if self.pending_uploads.remove(id).is_some() {
                    Ok(Vec::new())
                } else {
                    Err((ERR_UNKNOWN_IMAGE, "image id not found"))
                }
            }
            Some(img) => {
                if let Some(gpu) = img.gpu.get() {
                    self.pending_image_deletes.push(gpu);
                }
                Ok(Vec::new())
            }
        }
    }

    fn cmd_update_image(
        &mut self,
        b: UpdateImageBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        // Validate without mutating, then commit.
        let el = self
            .state
            .elements()
            .get(&b.id)
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        if b.command_index >= el.commands.len() {
            return Err((ERR_COMMAND_INDEX, "index out of range"));
        }
        if !matches!(el.commands[b.command_index], DrawCmd::DrawImage { .. }) {
            return Err((ERR_BAD_PAYLOAD, "command at index is not DrawImage"));
        }
        if !self.state.shared.images.contains_key(&b.new_image_id) {
            return Err((ERR_UNKNOWN_IMAGE, "new_image_id not found"));
        }
        let el = self.state.elements_mut().get_mut(&b.id).unwrap();
        if let DrawCmd::DrawImage {
            image_id,
            target_rect: _,
            source_rect: _,
        } = &mut el.commands[b.command_index]
        {
            *image_id = b.new_image_id;
        }
        Ok(Vec::new())
    }

    fn validate_commands(&self, cmds: &[DrawCmd]) -> Result<(), (u16, &'static str)> {
        if cmds.len() as u32 > self.limits.max_commands_per_element {
            return Err((ERR_BAD_PAYLOAD, "command list too long"));
        }
        for c in cmds {
            if let DrawCmd::DrawText { text, .. } = c
                && text.len() as u32 > self.limits.max_text_bytes
            {
                return Err((ERR_TEXT_RANGE, "text too long"));
            }
            // §7.5: DrawImage references must resolve at command-processing
            // time, atomically.
            if let DrawCmd::DrawImage { image_id, .. } = c
                && !self.state.shared.images.contains_key(image_id)
            {
                return Err((ERR_UNKNOWN_IMAGE, "DrawImage references unknown image"));
            }
        }
        Ok(())
    }

    fn cmd_create_element(
        &mut self,
        b: CreateElementBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        if !b.id.is_empty() && self.state.elements().contains_key(&b.id) {
            return Err((ERR_DUPLICATE_ID, "id in use"));
        }
        if self.state.elements().len() as u32 >= self.limits.max_elements {
            return Err((ERR_TOO_MANY_ELEMENTS, "element budget exhausted"));
        }
        self.validate_commands(&b.commands)?;

        // §9.4 — validate parent and depth before mutating state.
        if let Some(parent_id) = &b.parent {
            if !self.state.elements().contains_key(parent_id) {
                return Err((ERR_UNKNOWN_ELEMENT, "parent_id not found"));
            }
            // depth = parent's depth + 1
            let parent_depth = self.depth_of(parent_id);
            if parent_depth + 1 >= self.limits.max_nesting_depth as usize {
                return Err((ERR_MAX_NESTING_DEPTH, "would exceed max_nesting_depth"));
            }
        }

        let key = if b.id.is_empty() {
            self.state.current_mut().anonymous_key()
        } else {
            b.id.clone()
        };
        let id_field = if b.id.is_empty() { None } else { Some(b.id) };
        let seq = self.state.current_mut().next_seq();

        // For top-level elements, anchor to scrollback. For children,
        // store origin verbatim (parent-relative).
        let (anchor, sub) = if b.parent.is_none() {
            self.anchor_from_origin(b.origin)
        } else {
            (0, 0.0)
        };

        self.state.elements_mut().insert(
            key.clone(),
            Element {
                id: id_field,
                commands: b.commands,
                parent: b.parent.clone(),
                children: Vec::new(),
                clip_size: b.size,
                transform: b.transform,
                anchor_line: anchor,
                sub_row: sub,
                origin_x: b.origin.x,
                origin_y: b.origin.y,
                is_visible: b.is_visible,
                draw_order: b.draw_order,
                creation_seq: seq,
            },
        );

        // Register with parent's children list.
        if let Some(parent_id) = &b.parent
            && let Some(parent) = self.state.elements_mut().get_mut(parent_id)
        {
            parent.children.push(key);
        }

        Ok(Vec::new())
    }

    /// Walk the parent chain to compute an element's depth. Top-level
    /// elements return 0. Bounded by `max_nesting_depth` (so worst-case
    /// O(16)).
    fn depth_of(&self, id: &str) -> usize {
        let mut depth = 0usize;
        let mut cur = id.to_owned();
        while let Some(el) = self.state.elements().get(&cur) {
            match &el.parent {
                Some(p) => {
                    depth += 1;
                    cur = p.clone();
                }
                None => break,
            }
            if depth > self.limits.max_nesting_depth as usize {
                break; // safety against cycles (shouldn't be possible)
            }
        }
        depth
    }

    fn anchor_from_origin(&self, origin: Point) -> (i64, f32) {
        let floor = origin.y.floor();
        (
            self.line_tracker.top_of_live_screen + floor as i64,
            origin.y - floor,
        )
    }

    fn cmd_delete_element(&mut self, id: &str) -> Result<Vec<u8>, (u16, &'static str)> {
        if id.is_empty() {
            return Err((ERR_BAD_PAYLOAD, "empty id"));
        }
        if !self.state.elements().contains_key(id) {
            return Err((ERR_UNKNOWN_ELEMENT, "id not found"));
        }
        // Detach from parent's children list, then cascade.
        let parent_key = self.state.elements().get(id).and_then(|e| e.parent.clone());
        if let Some(p) = parent_key
            && let Some(parent) = self.state.elements_mut().get_mut(&p)
        {
            parent.children.retain(|c| c != id);
        }
        self.delete_subtree(id);
        Ok(Vec::new())
    }

    /// Remove `id` and all its descendants from the element table.
    /// Queues GPU image handles for deletion if any descendant held one
    /// (currently only DrawImage references images, but images live in
    /// the shared image table — element deletion does not free them).
    fn delete_subtree(&mut self, id: &str) {
        let el = match self.state.elements_mut().remove(id) {
            Some(e) => e,
            None => return,
        };
        for child in el.children {
            self.delete_subtree(&child);
        }
    }

    fn cmd_update_commands(
        &mut self,
        b: UpdateCommandsBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        if !self.state.elements().contains_key(&b.id) {
            return Err((ERR_UNKNOWN_ELEMENT, "id not found"));
        }
        self.validate_commands(&b.commands)?;
        let el = self.state.elements_mut().get_mut(&b.id).unwrap();
        el.commands = b.commands;
        Ok(Vec::new())
    }

    fn cmd_update_command(
        &mut self,
        b: UpdateCommandBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        // Validate without mutating, then commit (so an error leaves
        // state untouched per §4).
        {
            let el = self
                .state
                .elements()
                .get(&b.id)
                .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
            if b.index >= el.commands.len() {
                return Err((ERR_COMMAND_INDEX, "index out of range"));
            }
        }
        if let DrawCmd::DrawText { text, .. } = &b.command
            && text.len() as u32 > self.limits.max_text_bytes
        {
            return Err((ERR_TEXT_RANGE, "text too long"));
        }
        if let DrawCmd::DrawImage { image_id, .. } = &b.command
            && !self.state.shared.images.contains_key(image_id)
        {
            return Err((ERR_UNKNOWN_IMAGE, "DrawImage references unknown image"));
        }
        let el = self.state.elements_mut().get_mut(&b.id).unwrap();
        el.commands[b.index] = b.command;
        Ok(Vec::new())
    }

    fn cmd_update_text(
        &mut self,
        b: UpdateTextBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let max_text = self.limits.max_text_bytes as usize;
        let el = self
            .state
            .elements_mut()
            .get_mut(&b.id)
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        if b.command_index >= el.commands.len() {
            return Err((ERR_COMMAND_INDEX, "index out of range"));
        }
        let DrawCmd::DrawText { text, .. } = &mut el.commands[b.command_index] else {
            return Err((ERR_BAD_PAYLOAD, "command at index is not DrawText"));
        };
        match b.range {
            UpdateTextRange::Whole => {
                if b.replacement.len() > max_text {
                    return Err((ERR_TEXT_RANGE, "replacement exceeds max_text_bytes"));
                }
                *text = b.replacement;
            }
            UpdateTextRange::Range { start, end } => {
                if !(start <= end && end <= text.len()) {
                    return Err((ERR_TEXT_RANGE, "range out of bounds"));
                }
                if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                    return Err((ERR_TEXT_RANGE, "range not on char boundary"));
                }
                let new_len = text.len() - (end - start) + b.replacement.len();
                if new_len > max_text {
                    return Err((ERR_TEXT_RANGE, "result exceeds max_text_bytes"));
                }
                text.replace_range(start..end, &b.replacement);
            }
        }
        Ok(Vec::new())
    }

    fn cmd_update_origin(
        &mut self,
        id: &str,
        origin: Point,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let is_top_level = self
            .state
            .elements()
            .get(id)
            .map(|e| e.parent.is_none())
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        // Re-anchor only for top-level elements (origin.y is
        // scrollback-relative); for children, origin is parent-relative
        // and stored verbatim.
        let (anchor, sub) = if is_top_level {
            self.anchor_from_origin(origin)
        } else {
            (0, 0.0)
        };
        let el = self.state.elements_mut().get_mut(id).unwrap();
        el.anchor_line = anchor;
        el.sub_row = sub;
        el.origin_x = origin.x;
        el.origin_y = origin.y;
        Ok(Vec::new())
    }

    fn cmd_update_visibility(
        &mut self,
        id: &str,
        is_visible: bool,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let el = self
            .state
            .elements_mut()
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        el.is_visible = is_visible;
        Ok(Vec::new())
    }

    fn cmd_update_draw_order(
        &mut self,
        id: &str,
        draw_order: i32,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let el = self
            .state
            .elements_mut()
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        el.draw_order = draw_order;
        Ok(Vec::new())
    }

    fn queue_envelope(&mut self, frames_buf: Vec<u8>) {
        let env = wrap_envelope(&frames_buf);
        self.pending_response_bytes.extend_from_slice(&env);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vge_protocol::codec::{stuff, Reader, Writer};

    fn build_envelope(frames_buf: &[u8]) -> Vec<u8> {
        // Mimics what a client would write to its PTY: ESC _ V G E ...
        // (uppercase marker) with the unstuffed payload byte-stuffed.
        let mut unstuffed = Vec::new();
        unstuffed.push(PROTOCOL_VERSION);
        unstuffed.extend_from_slice(&(frames_buf.len() as u32).to_le_bytes());
        unstuffed.extend_from_slice(frames_buf);

        let mut env = Vec::new();
        env.push(ESC);
        env.push(APC_OPEN);
        env.extend_from_slice(MARKER_C2T);
        stuff(&unstuffed, &mut env);
        env.push(ESC);
        env.push(ST_CLOSE);
        env
    }

    fn append_command(buf: &mut Vec<u8>, frame_type: u8, request_id: u32, body: &[u8]) {
        buf.push(frame_type);
        buf.extend_from_slice(&request_id.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(body);
    }

    /// Manually unwrap a terminal-to-client envelope. The receive-path
    /// ApcStream only matches uppercase `VGE`, so we strip the
    /// `ESC _ vge` prefix and `ESC \` suffix by hand here.
    fn unwrap_t2c_envelope(env: &[u8]) -> Vec<u8> {
        unwrap_envelope(env, MARKER_T2C)
    }

    /// Like [`unwrap_t2c_envelope`] but for client-to-terminal envelopes
    /// (`ESC _ VGE`). `serialize_state` produces these — the snapshot is
    /// commands the receiving engine replays as a fresh client.
    fn unwrap_c2t_envelope(env: &[u8]) -> Vec<u8> {
        unwrap_envelope(env, MARKER_C2T)
    }

    fn unwrap_envelope(env: &[u8], marker: &[u8; 3]) -> Vec<u8> {
        assert!(env.len() >= 7);
        assert_eq!(&env[..2], &[ESC, APC_OPEN]);
        assert_eq!(&env[2..5], marker);
        assert_eq!(&env[env.len() - 2..], &[ESC, ST_CLOSE]);
        let mut out = Vec::new();
        let mut i = 5;
        while i < env.len() - 2 {
            if env[i] == ESC && i + 1 < env.len() - 2 && env[i + 1] == ESC {
                out.push(ESC);
                i += 2;
            } else {
                out.push(env[i]);
                i += 1;
            }
        }
        out
    }

    #[test]
    fn probe_response_envelope_shape() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_PROBE, 7, &[]);
        let env = build_envelope(&frames);

        let passthrough = engine.process_pty_chunk(&env);
        assert!(passthrough.is_empty(), "envelope should not leak to vt100");

        let response = engine.take_responses();
        assert!(!response.is_empty(), "probe must produce a response");

        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _payload_len = r.u32().unwrap();
        assert_eq!(r.u8().unwrap(), RSP_PROBE);
        assert_eq!(r.u32().unwrap(), 7); // request_id echoed
        assert_eq!(r.u32().unwrap(), 32); // body_len for ProbeBody (incl. max_nesting_depth)
    }

    #[test]
    fn auto_reply_commands_disabled_suppresses_responses() {
        // vsd disables command auto-reply on its session VGE
        // engine so the upstream real host (e.g. local veter) is the
        // sole responder. Envelopes must still be consumed (not
        // leaked to vt100) and state changes still applied — only
        // the response frames are suppressed.
        let mut engine = VgeEngine::new((9, 20), 1.0);
        engine.set_auto_reply_commands(false);

        // Probe — would normally produce a ProbeResponse.
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_PROBE, 42, &[]);
        let env = build_envelope(&frames);
        let passthrough = engine.process_pty_chunk(&env);
        assert!(passthrough.is_empty(), "Probe envelope consumed");
        assert!(
            engine.take_responses().is_empty(),
            "no ProbeResponse should be queued with auto_reply_commands=false"
        );

        // UploadImage — would normally produce an RSP_OK. State must
        // still update (image gets stored) so future snapshot replay
        // is correct.
        let body = upload_raw_2x2("logo");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 7, &body);
        let env = build_envelope(&frames);
        let passthrough = engine.process_pty_chunk(&env);
        assert!(passthrough.is_empty(), "UploadImage envelope consumed");
        assert!(
            engine.take_responses().is_empty(),
            "no RSP_OK should be queued with auto_reply_commands=false"
        );
        // Sanity: state mirror was actually updated.
        assert!(
            engine.state.shared.images.contains_key("logo"),
            "image must be stored even when responses are suppressed"
        );
    }

    #[test]
    fn probe_body_fields() {
        let mut engine = VgeEngine::new((9, 20), 1.5);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_PROBE, 0, &[]);
        let env = build_envelope(&frames);
        engine.process_pty_chunk(&env);
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);

        // Skip header (u8 version + u4 length + u8 frame + u4 reqid + u4 body_len = 14 bytes)
        let body = &payload[14..];
        let mut r = Reader::new(body);
        assert_eq!(r.u16().unwrap(), 0); // protocol_version
        assert_eq!(r.u16().unwrap(), 9); // cell_w
        assert_eq!(r.u16().unwrap(), 20); // cell_h
        assert_eq!(r.f32().unwrap(), 1.5); // scale_factor
        assert_eq!(r.u32().unwrap(), 4096); // max_elements
        assert_eq!(r.u32().unwrap(), 4096); // max_commands_per_element
        assert_eq!(r.u32().unwrap(), 1_048_576); // max_text_bytes
        assert_eq!(r.u32().unwrap(), 32 * 1024 * 1024); // max_image_bytes
        assert_eq!(r.u32().unwrap(), 1024); // max_images
        assert_eq!(r.u8().unwrap(), 0b11); // supported_image_encodings (Raw|WebP)
    }

    #[test]
    fn create_then_delete_element() {
        let mut engine = VgeEngine::new((9, 20), 1.0);

        // Build a CreateElement body for id="rect", a single FillRectangles
        // command with a flat white rect, origin (5, 3), visible.
        let mut body = Writer::new();
        body.str("rect");
        body.varu(1); // n_commands
        body.u8(OP_FILL_RECTANGLES);
        body.u8(STYLE_FLAT);
        body.u8(COLOR_RGBA8888);
        body.u8(0xFF);
        body.u8(0xFF);
        body.u8(0xFF);
        body.u8(0xFF);
        body.varu(1); // n_rects
        body.f32(0.0);
        body.f32(0.0);
        body.f32(2.0);
        body.f32(1.0);
        body.f32(5.0); // origin.x
        body.f32(3.0); // origin.y
        body.u8(1); // is_visible
        for b in 0i32.to_le_bytes() {
            body.u8(b);
        }

        let mut frames = Vec::new();
        append_command(&mut frames, CMD_CREATE_ELEMENT, 1, &body.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        assert!(engine.state.elements().contains_key("rect"));
        let el = &engine.state.elements()["rect"];
        assert_eq!(el.anchor_line, 3); // top_of_live_screen=0 + floor(3.0)
        assert_eq!(el.origin_x, 5.0);
        assert!(el.is_visible);

        // Now delete it.
        let mut body = Writer::new();
        body.str("rect");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_DELETE_ELEMENT, 2, &body.buf);
        let _ = engine.take_responses(); // discard previous OK
        engine.process_pty_chunk(&build_envelope(&frames));

        assert!(!engine.state.elements().contains_key("rect"));
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _version = r.u8().unwrap();
        let _payload_len = r.u32().unwrap();
        assert_eq!(r.u8().unwrap(), RSP_OK);
        assert_eq!(r.u32().unwrap(), 2); // request_id echoed
        assert_eq!(r.u32().unwrap(), 0); // empty body
    }

    #[test]
    fn duplicate_id_returns_err() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut body = Writer::new();
        body.str("dup");
        body.varu(0); // 0 commands
        body.f32(0.0);
        body.f32(0.0);
        body.u8(1);
        for b in 0i32.to_le_bytes() {
            body.u8(b);
        }
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_CREATE_ELEMENT, 1, &body.buf);
        append_command(&mut frames, CMD_CREATE_ELEMENT, 2, &body.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        // Skip envelope header.
        let mut r = Reader::new(&payload);
        let _version = r.u8().unwrap();
        let _payload_len = r.u32().unwrap();

        // First frame: Ok for request_id 1.
        assert_eq!(r.u8().unwrap(), RSP_OK);
        assert_eq!(r.u32().unwrap(), 1);
        let body_len = r.u32().unwrap() as usize;
        let _ = r.take(body_len).unwrap();

        // Second frame: Err for request_id 2.
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        assert_eq!(r.u32().unwrap(), 2);
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        let mut er = Reader::new(err_body);
        assert_eq!(er.u16().unwrap(), ERR_DUPLICATE_ID);
    }

    #[test]
    fn set_global_style_upserts() {
        let mut engine = VgeEngine::new((9, 20), 1.0);

        // Two SetGlobalStyle frames with the same ID — second wins.
        let mut body1 = Writer::new();
        body1.str("accent");
        body1.u8(STYLE_FLAT);
        body1.u8(COLOR_RGBA8888);
        body1.u8(0xFF);
        body1.u8(0x00);
        body1.u8(0x00);
        body1.u8(0xFF);

        let mut body2 = Writer::new();
        body2.str("accent");
        body2.u8(STYLE_FLAT);
        body2.u8(COLOR_RGBA8888);
        body2.u8(0x00);
        body2.u8(0xFF);
        body2.u8(0x00);
        body2.u8(0xFF);

        let mut frames = Vec::new();
        append_command(&mut frames, CMD_SET_GLOBAL_STYLE, 1, &body1.buf);
        append_command(&mut frames, CMD_SET_GLOBAL_STYLE, 2, &body2.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        assert_eq!(engine.state.shared.styles.len(), 1);
        let s = engine.state.shared.styles.get("accent").unwrap();
        match s {
            command::ConcreteStyle::Flat(c) => {
                assert!((c.g - 1.0).abs() < 1e-3, "second set should win (green)");
            }
            _ => panic!("wrong concrete style kind"),
        }
    }

    fn palette_rgb() -> HostThemePalette {
        let c = |r, g, b| command::Color { r, g, b, a: 1.0 };
        HostThemePalette {
            accents: vec![c(1.0, 0.0, 0.0), c(0.0, 1.0, 0.0), c(0.0, 0.0, 1.0)],
        }
    }

    fn flat_g(styles: &HashMap<String, ConcreteStyle>, id: &str) -> f32 {
        match styles.get(id).unwrap_or_else(|| panic!("missing {id}")) {
            ConcreteStyle::Flat(c) => c.g,
            _ => panic!("{id} not flat"),
        }
    }

    #[test]
    fn set_global_style_rejects_host_namespace() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let body = set_global_style_flat("host.accent", 0x11, 0x22, 0x33, 0xFF);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_SET_GLOBAL_STYLE, 7, &body);
        engine.process_pty_chunk(&build_envelope(&frames));

        let payload = unwrap_t2c_envelope(&engine.take_responses());
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        assert_eq!(r.u32().unwrap(), 7);
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        assert_eq!(Reader::new(err_body).u16().unwrap(), ERR_RESERVED_STYLE_ID);
        // Table untouched.
        assert!(!engine.state.shared.styles.contains_key("host.accent"));
    }

    #[test]
    fn seed_host_styles_keys_accent_on_depth() {
        // Depth 1 → `host.accent` rotates to slot 1 (green); the numbered
        // slots are fixed regardless of depth.
        let mut engine = VgeEngine::new((9, 20), 1.0);
        engine.seed_host_styles(palette_rgb(), 1);
        let styles = &engine.state.shared.styles;
        assert!((flat_g(styles, "host.accent") - 1.0).abs() < 1e-3);
        assert!((flat_g(styles, "host.accent.1") - 0.0).abs() < 1e-3);
        assert!((flat_g(styles, "host.accent.2") - 1.0).abs() < 1e-3);
        assert!((flat_g(styles, "host.accent.3") - 0.0).abs() < 1e-3);
    }

    #[test]
    fn seed_host_styles_reinjected_after_ris() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        engine.seed_host_styles(palette_rgb(), 0);
        assert!(engine.state.shared.styles.contains_key("host.accent"));
        // RIS wipes the table; the host re-seeds its reserved entries.
        engine.process_pty_chunk(b"\x1bc");
        assert!((flat_g(&engine.state.shared.styles, "host.accent") - 0.0).abs() < 1e-3);
        assert!(engine.state.shared.styles.contains_key("host.accent.3"));
    }

    #[test]
    fn empty_palette_seeds_nothing() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        engine.seed_host_styles(HostThemePalette::default(), 0);
        assert!(engine.state.shared.styles.is_empty());
    }

    #[test]
    fn create_element_with_unknown_style_ref_succeeds() {
        // Element with Style::Ref to a missing global ID must still be
        // created — it'll render magenta but the protocol path is fine.
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut body = Writer::new();
        body.str("widget");
        body.varu(1); // 1 command
        body.u8(OP_FILL_RECTANGLES);
        body.u8(STYLE_REF);
        body.str("does-not-exist");
        body.varu(1);
        body.f32(0.0);
        body.f32(0.0);
        body.f32(1.0);
        body.f32(1.0);
        body.f32(0.0);
        body.f32(0.0);
        body.u8(1);
        for b in 0i32.to_le_bytes() {
            body.u8(b);
        }
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_CREATE_ELEMENT, 1, &body.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        assert!(engine.state.elements().contains_key("widget"));
    }

    #[test]
    fn unknown_command_returns_err_keeps_state() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(&mut frames, 0x99, 5, &[]);
        engine.process_pty_chunk(&build_envelope(&frames));
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        assert_eq!(r.u32().unwrap(), 5);
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        let mut er = Reader::new(err_body);
        assert_eq!(er.u16().unwrap(), ERR_UNKNOWN_COMMAND);
        assert!(engine.state.elements().is_empty());
    }

    /// Build an `UploadImage` body for a 2x2 RGBA8 raw image (red,
    /// green, blue, white). Single-chunk form: offset=0, is_last=true,
    /// total_bytes = payload length.
    fn upload_raw_2x2(id: &str) -> Vec<u8> {
        let pixels: [u8; 16] = [
            0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF,
        ];
        let mut w = Writer::new();
        w.str(id);
        w.u8(0x01); // encoding Raw
        w.u32(2); // width
        w.u32(2); // height
        w.u32(pixels.len() as u32); // total_bytes
        w.u32(0); // chunk_offset
        w.bool(true); // is_last
        w.bytes(&pixels);
        w.buf
    }

    #[test]
    fn upload_raw_succeeds() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let body = upload_raw_2x2("logo");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &body);
        engine.process_pty_chunk(&build_envelope(&frames));

        assert_eq!(engine.state.shared.images.len(), 1);
        let img = engine.state.shared.images.get("logo").unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 4);
    }

    #[test]
    fn upload_raw_byte_count_mismatch_rejected() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // Announce 4x4 (64 bytes) but send only 16. is_last on this
        // first-and-only chunk means the host hits the
        // bytes_received < total_bytes finalization check.
        let mut w = Writer::new();
        w.str("bad");
        w.u8(0x01);
        w.u32(4);
        w.u32(4);
        w.u32(64); // total_bytes
        w.u32(0);
        w.bool(true); // is_last with not enough data
        w.bytes(&[0u8; 16]);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &w.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        assert!(engine.state.shared.images.is_empty());
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        let _ = r.u32();
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        let mut er = Reader::new(err_body);
        assert_eq!(er.u16().unwrap(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn upload_image_unknown_encoding_rejected() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut w = Writer::new();
        w.str("x");
        w.u8(0x99); // unknown encoding
        w.u32(1);
        w.u32(1);
        w.u32(4);
        w.u32(0);
        w.bool(true);
        w.bytes(&[0u8; 4]);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &w.buf);
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.shared.images.is_empty());
    }

    #[test]
    fn upload_duplicate_id_rejected() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let body = upload_raw_2x2("dup");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &body);
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 2, &body);
        engine.process_pty_chunk(&build_envelope(&frames));

        assert_eq!(engine.state.shared.images.len(), 1);
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        // Frame 1: ChunkAck (UploadImage uses the chunked-upload
        // response frame, even for single-chunk uploads).
        assert_eq!(r.u8().unwrap(), RSP_CHUNK_ACK);
        let _ = r.u32();
        let n1 = r.u32().unwrap() as usize;
        let _ = r.take(n1).unwrap();
        // Frame 2: Err(ERR_DUPLICATE_IMAGE_ID).
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        let _ = r.u32();
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        let mut er = Reader::new(err_body);
        assert_eq!(er.u16().unwrap(), ERR_DUPLICATE_IMAGE_ID);
    }

    /// Build one UploadImage chunk envelope body (just the command body
    /// — caller still wraps with append_command).
    fn upload_chunk_body(
        id: &str,
        encoding: u8,
        width: u32,
        height: u32,
        total_bytes: u32,
        chunk_offset: u32,
        is_last: bool,
        data: &[u8],
    ) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(id);
        w.u8(encoding);
        w.u32(width);
        w.u32(height);
        w.u32(total_bytes);
        w.u32(chunk_offset);
        w.bool(is_last);
        w.bytes(data);
        w.buf
    }

    #[test]
    fn upload_chunked_finalizes_on_last() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // 2x2 image (16 bytes) split into two 8-byte chunks.
        let pixels: [u8; 16] = [
            0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF,
        ];
        let chunk_a = upload_chunk_body("stream", 0x01, 2, 2, 16, 0, false, &pixels[..8]);
        let chunk_b = upload_chunk_body("stream", 0x01, 2, 2, 16, 8, true, &pixels[8..]);

        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &chunk_a);
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 2, &chunk_b);
        engine.process_pty_chunk(&build_envelope(&frames));

        // Image is only registered after the last chunk.
        assert_eq!(engine.state.shared.images.len(), 1);
        let img = engine.state.shared.images.get("stream").unwrap();
        assert_eq!(img.source_data.as_slice(), &pixels);
        // No leftover pending entry.
        assert!(engine.pending_uploads.is_empty());

        // Both frames produced a ChunkAck with the cumulative byte
        // count for this id.
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8(); // protocol_version
        let _ = r.u32(); // payload_length
        for expected_bytes in [8u32, 16u32] {
            assert_eq!(r.u8().unwrap(), RSP_CHUNK_ACK);
            let _ = r.u32(); // request_id
            let body_len = r.u32().unwrap() as usize;
            let body = r.take(body_len).unwrap();
            let mut br = Reader::new(body);
            assert_eq!(br.string().unwrap(), "stream");
            assert_eq!(br.u32().unwrap(), expected_bytes);
        }
    }

    #[test]
    fn drop_image_aborts_in_progress_upload() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // Start a chunked upload but never finish it, then DropImage it
        // (§8.2: abort). The slot is released and the id is reusable.
        let chunk_a = upload_chunk_body("part", 0x01, 2, 2, 16, 0, false, &[0u8; 8]);
        let mut drop_body = Writer::new();
        drop_body.str("part");
        let restart = upload_raw_2x2("part");

        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &chunk_a);
        append_command(&mut frames, CMD_DROP_IMAGE, 2, &drop_body.buf);
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 3, &restart);
        engine.process_pty_chunk(&build_envelope(&frames));

        // The abandoned slot is gone and the fresh single-shot upload
        // under the same id finalized.
        assert!(engine.pending_uploads.is_empty());
        assert!(engine.state.shared.images.contains_key("part"));

        // Responses: ChunkAck, Ok (drop), ChunkAck — no errors.
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        for expected in [RSP_CHUNK_ACK, RSP_OK, RSP_CHUNK_ACK] {
            assert_eq!(r.u8().unwrap(), expected);
            let _ = r.u32();
            let body_len = r.u32().unwrap() as usize;
            let _ = r.take(body_len).unwrap();
        }
    }

    #[test]
    fn upload_chunked_out_of_order_rejected() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // First chunk arrives, second chunk lies about its offset.
        let chunk_a = upload_chunk_body("oo", 0x01, 2, 2, 16, 0, false, &[0u8; 8]);
        let chunk_b = upload_chunk_body("oo", 0x01, 2, 2, 16, 12, true, &[0u8; 4]);

        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &chunk_a);
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 2, &chunk_b);
        engine.process_pty_chunk(&build_envelope(&frames));

        // Pending entry was dropped by the failing second chunk; no
        // image was registered.
        assert!(engine.state.shared.images.is_empty());
        assert!(engine.pending_uploads.is_empty());
    }

    #[test]
    fn drop_image_removes_from_table() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let body = upload_raw_2x2("logo");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &body);
        let mut drop_body = Writer::new();
        drop_body.str("logo");
        append_command(&mut frames, CMD_DROP_IMAGE, 2, &drop_body.buf);
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.shared.images.is_empty());
    }

    #[test]
    fn drop_unknown_image_errors() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut w = Writer::new();
        w.str("nope");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_DROP_IMAGE, 1, &w.buf);
        engine.process_pty_chunk(&build_envelope(&frames));
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        let _ = r.u32();
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        let mut er = Reader::new(err_body);
        assert_eq!(er.u16().unwrap(), ERR_UNKNOWN_IMAGE);
    }

    /// Build a `CreateElement` body containing a single DrawImage that
    /// references `image_id`.
    fn create_element_with_draw_image(elem_id: &str, image_id: &str) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(elem_id);
        w.varu(1); // 1 command
        w.u8(OP_DRAW_IMAGE);
        // target_rect (0,0,4,2)
        w.f32(0.0);
        w.f32(0.0);
        w.f32(4.0);
        w.f32(2.0);
        w.str(image_id);
        w.u8(0); // source_rect flag: 0 = whole image (§7.5)
        // origin
        w.f32(0.0);
        w.f32(0.0);
        // is_visible
        w.u8(1);
        // draw_order
        for b in 0i32.to_le_bytes() {
            w.u8(b);
        }
        w.buf
    }

    #[test]
    fn create_element_referencing_known_image_succeeds() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let upload = upload_raw_2x2("logo");
        let create = create_element_with_draw_image("widget", "logo");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &upload);
        append_command(&mut frames, CMD_CREATE_ELEMENT, 2, &create);
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements().contains_key("widget"));
    }

    #[test]
    fn create_element_with_unknown_image_atomically_fails() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let create = create_element_with_draw_image("widget", "missing");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_CREATE_ELEMENT, 1, &create);
        engine.process_pty_chunk(&build_envelope(&frames));
        // Atomic failure: no element added.
        assert!(engine.state.elements().is_empty());
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        let _ = r.u32();
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        let mut er = Reader::new(err_body);
        assert_eq!(er.u16().unwrap(), ERR_UNKNOWN_IMAGE);
    }

    #[test]
    fn update_image_swaps_id() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let upload_a = upload_raw_2x2("a");
        let upload_b = upload_raw_2x2("b");
        let create = create_element_with_draw_image("widget", "a");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &upload_a);
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 2, &upload_b);
        append_command(&mut frames, CMD_CREATE_ELEMENT, 3, &create);
        // UpdateImage to point at "b" instead.
        let mut upd = Writer::new();
        upd.str("widget");
        upd.varu(0); // command_index
        upd.str("b");
        append_command(&mut frames, CMD_UPDATE_IMAGE, 4, &upd.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        let el = engine.state.elements().get("widget").unwrap();
        match &el.commands[0] {
            command::DrawCmd::DrawImage { image_id, .. } => assert_eq!(image_id, "b"),
            _ => panic!("expected DrawImage"),
        }
    }

    #[test]
    fn update_image_to_missing_image_errors() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let upload = upload_raw_2x2("a");
        let create = create_element_with_draw_image("widget", "a");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &upload);
        append_command(&mut frames, CMD_CREATE_ELEMENT, 2, &create);
        let mut upd = Writer::new();
        upd.str("widget");
        upd.varu(0);
        upd.str("nonexistent");
        append_command(&mut frames, CMD_UPDATE_IMAGE, 3, &upd.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        // Element still references the original.
        let el = engine.state.elements().get("widget").unwrap();
        match &el.commands[0] {
            command::DrawCmd::DrawImage { image_id, .. } => assert_eq!(image_id, "a"),
            _ => panic!("expected DrawImage"),
        }
    }

    #[test]
    fn drop_image_in_use_keeps_element() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let upload = upload_raw_2x2("logo");
        let create = create_element_with_draw_image("widget", "logo");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &upload);
        append_command(&mut frames, CMD_CREATE_ELEMENT, 2, &create);
        let mut drop_body = Writer::new();
        drop_body.str("logo");
        append_command(&mut frames, CMD_DROP_IMAGE, 3, &drop_body.buf);
        engine.process_pty_chunk(&build_envelope(&frames));
        // Element still exists, but image table entry is gone.
        // Render-time fallback to magenta is GUI-only and not asserted here.
        assert!(engine.state.elements().contains_key("widget"));
        assert!(!engine.state.shared.images.contains_key("logo"));
    }

    /// Build a v2-layout `CreateElement` body with `extra_flags`.
    /// `parent` and `size` are the optional pieces; pass None to skip.
    fn create_with_tree(
        id: &str,
        origin: (f32, f32),
        parent: Option<&str>,
        size: Option<(f32, f32)>,
    ) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(id);
        w.varu(0); // n_commands = 0 (bare grouping element)
        w.f32(origin.0);
        w.f32(origin.1);
        w.u8(1); // is_visible
        for b in 0i32.to_le_bytes() {
            w.u8(b);
        }
        if parent.is_some() || size.is_some() {
            let mut flags: u8 = 0;
            if parent.is_some() {
                flags |= 0b01;
            }
            if size.is_some() {
                flags |= 0b10;
            }
            w.u8(flags);
            if let Some(p) = parent {
                w.str(p);
            }
            if let Some((sx, sy)) = size {
                w.f32(sx);
                w.f32(sy);
            }
        }
        w.buf
    }

    #[test]
    fn create_child_element_succeeds() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("root", (0.0, 0.0), None, Some((40.0, 10.0))),
        );
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            2,
            &create_with_tree("child", (3.0, 2.0), Some("root"), None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements().contains_key("root"));
        assert!(engine.state.elements().contains_key("child"));
        assert_eq!(engine.state.elements()["root"].children, vec!["child".to_string()]);
        assert_eq!(engine.state.elements()["child"].parent.as_deref(), Some("root"));
    }

    #[test]
    fn create_with_unknown_parent_atomically_fails() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("orphan", (0.0, 0.0), Some("does-not-exist"), None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements().is_empty());
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        let _ = r.u32();
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        let mut er = Reader::new(err_body);
        assert_eq!(er.u16().unwrap(), ERR_UNKNOWN_ELEMENT);
    }

    #[test]
    fn delete_parent_cascades_to_children() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("root", (0.0, 0.0), None, None),
        );
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            2,
            &create_with_tree("c1", (0.0, 0.0), Some("root"), None),
        );
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            3,
            &create_with_tree("c2", (0.0, 0.0), Some("root"), None),
        );
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            4,
            &create_with_tree("gc", (0.0, 0.0), Some("c1"), None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert_eq!(engine.state.elements().len(), 4);

        let mut del = Writer::new();
        del.str("root");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_DELETE_ELEMENT, 5, &del.buf);
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements().is_empty());
    }

    #[test]
    fn nesting_depth_cap_enforced() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // Depth limit = 16 by default. Build a chain of 16 elements
        // (e0 → e1 → … → e15), then attempt e16, which should fail.
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            0,
            &create_with_tree("e0", (0.0, 0.0), None, None),
        );
        for i in 1..16 {
            let id = format!("e{i}");
            let parent = format!("e{}", i - 1);
            append_command(
                &mut frames,
                CMD_CREATE_ELEMENT,
                i as u32,
                &create_with_tree(&id, (0.0, 0.0), Some(&parent), None),
            );
        }
        // 16th level child should be rejected.
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            16,
            &create_with_tree("e16", (0.0, 0.0), Some("e15"), None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements().contains_key("e15"));
        assert!(!engine.state.elements().contains_key("e16"));
        // Last response must be err_max_nesting_depth.
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        // Walk all 17 frames; the last is the failure.
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        let mut last_was_err_depth = false;
        while !r.at_end() {
            let ty = r.u8().unwrap();
            let _ = r.u32();
            let body_len = r.u32().unwrap() as usize;
            let body = r.take(body_len).unwrap();
            if ty == RSP_ERR {
                let mut er = Reader::new(body);
                if er.u16().unwrap() == ERR_MAX_NESTING_DEPTH {
                    last_was_err_depth = true;
                }
            }
        }
        assert!(last_was_err_depth);
    }

    #[test]
    fn update_size_sets_clip() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("widget", (0.0, 0.0), None, None),
        );
        // Element starts with no clip.
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements()["widget"].clip_size.is_none());

        // UpdateSize sets it.
        let mut body = Writer::new();
        body.str("widget");
        body.f32(40.0);
        body.f32(10.0);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPDATE_SIZE, 2, &body.buf);
        engine.process_pty_chunk(&build_envelope(&frames));
        let sz = engine.state.elements()["widget"].clip_size.unwrap();
        assert_eq!(sz.x, 40.0);
        assert_eq!(sz.y, 10.0);
    }

    // --- §9.11 / §9.12 transform tests ---

    fn update_transform_body(id: &str, t: [f32; 6]) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(id);
        for v in t {
            w.f32(v);
        }
        w.buf
    }

    #[test]
    fn update_transform_sets_field() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("widget", (0.0, 0.0), None, None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements()["widget"].transform.is_none());

        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_UPDATE_TRANSFORM,
            2,
            &update_transform_body("widget", [0.0, 1.0, -1.0, 0.0, 2.5, -3.0]),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        let t = engine.state.elements()["widget"].transform.unwrap();
        assert_eq!((t.a, t.b, t.c, t.d, t.e, t.f), (0.0, 1.0, -1.0, 0.0, 2.5, -3.0));
    }

    #[test]
    fn update_transform_unknown_element_errors() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_UPDATE_TRANSFORM,
            1,
            &update_transform_body("ghost", [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        assert_eq!(r.u8().unwrap(), RSP_ERR);
        let _ = r.u32();
        let body_len = r.u32().unwrap() as usize;
        let err_body = r.take(body_len).unwrap();
        let mut er = Reader::new(err_body);
        assert_eq!(er.u16().unwrap(), ERR_UNKNOWN_ELEMENT);
    }

    #[test]
    fn update_transform_non_finite_rejected_state_unchanged() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("widget", (0.0, 0.0), None, None),
        );
        append_command(
            &mut frames,
            CMD_UPDATE_TRANSFORM,
            2,
            &update_transform_body("widget", [f32::NAN, 0.0, 0.0, 1.0, 0.0, 0.0]),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        // Atomic failure: transform stays unset. (The parser rejects the
        // frame, so the engine never sees a typed command.)
        assert!(engine.state.elements()["widget"].transform.is_none());
    }

    #[test]
    fn create_element_with_transform_flag_sets_field() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // create_with_tree without extras, then append the §9.4 trailing
        // block by hand: extra_flags bit2 + 6 floats.
        let mut body = create_with_tree("rot", (1.0, 2.0), None, None);
        body.push(0b100);
        for v in [0.5f32, 0.866, -0.866, 0.5, 0.0, 0.0] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_CREATE_ELEMENT, 1, &body);
        engine.process_pty_chunk(&build_envelope(&frames));
        let t = engine.state.elements()["rot"].transform.unwrap();
        assert_eq!(t.a, 0.5);
        assert_eq!(t.c, -0.866);
    }

    // --- §5.4 alt-screen tests ---

    #[test]
    fn alt_screen_swap_preserves_main_elements() {
        let mut engine = VgeEngine::new((9, 20), 1.0);

        // Create a main-screen element via VGE.
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("main-el", (0.0, 0.0), None, None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements().contains_key("main-el"));

        // Manually flip to alt screen (mimicking what alt-screen
        // detection would do after vt100 saw DECSET 1049 h).
        engine.state.enter_alt_screen();
        assert!(engine.state.on_alt());
        // Alt screen starts empty.
        assert!(engine.state.elements().is_empty());

        // Create something on alt.
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            2,
            &create_with_tree("alt-el", (0.0, 0.0), None, None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.elements().contains_key("alt-el"));
        assert!(!engine.state.elements().contains_key("main-el"));

        // Leave alt — main set restored, alt set dropped.
        engine.state.leave_alt_screen();
        assert!(!engine.state.on_alt());
        assert!(engine.state.elements().contains_key("main-el"));
        assert!(!engine.state.elements().contains_key("alt-el"));
    }

    #[test]
    fn alt_screen_shares_image_and_style_tables() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // Upload an image and a style on main.
        let upload = upload_raw_2x2("logo");
        let mut style_body = Writer::new();
        style_body.str("accent");
        style_body.u8(STYLE_FLAT);
        style_body.u8(COLOR_RGBA8888);
        style_body.u8(0xFF);
        style_body.u8(0x00);
        style_body.u8(0x00);
        style_body.u8(0xFF);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &upload);
        append_command(&mut frames, CMD_SET_GLOBAL_STYLE, 2, &style_body.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        // Switch to alt — image + style still resolvable.
        engine.state.enter_alt_screen();
        assert!(engine.state.shared.images.contains_key("logo"));
        assert!(engine.state.shared.styles.contains_key("accent"));

        // Drop image while on alt; main shouldn't see it on return either.
        let mut drop_body = Writer::new();
        drop_body.str("logo");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_DROP_IMAGE, 3, &drop_body.buf);
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(!engine.state.shared.images.contains_key("logo"));
        engine.state.leave_alt_screen();
        assert!(!engine.state.shared.images.contains_key("logo"));
    }

    // --- §5.6 reset tests ---

    #[test]
    fn ris_wipes_state() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let upload = upload_raw_2x2("logo");
        let mut style_body = Writer::new();
        style_body.str("a");
        style_body.u8(STYLE_FLAT);
        style_body.u8(COLOR_RGBA8888);
        style_body.u8(0xFF);
        style_body.u8(0x00);
        style_body.u8(0x00);
        style_body.u8(0xFF);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &upload);
        append_command(&mut frames, CMD_SET_GLOBAL_STYLE, 2, &style_body.buf);
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            3,
            &create_with_tree("el", (0.0, 0.0), None, None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert_eq!(engine.state.elements().len(), 1);
        assert_eq!(engine.state.shared.images.len(), 1);
        assert_eq!(engine.state.shared.styles.len(), 1);

        // Now feed RIS as a raw byte stream. Engine sees the event and
        // wipes itself.
        engine.process_pty_chunk(b"\x1bc");
        assert!(engine.state.elements().is_empty());
        assert!(engine.state.shared.images.is_empty());
        assert!(engine.state.shared.styles.is_empty());
        assert!(!engine.state.on_alt());
    }

    #[test]
    fn decstr_wipes_state() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("el", (0.0, 0.0), None, None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(!engine.state.elements().is_empty());

        engine.process_pty_chunk(b"\x1b[!p");
        assert!(engine.state.elements().is_empty());
    }

    #[test]
    fn ed_3_drops_scrollback_elements_keeps_live() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // Live element.
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("live", (0.0, 0.0), None, None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));

        // Forge a scrollback element.
        let mut scroll_el =
            engine.state.elements().get("live").cloned().unwrap();
        scroll_el.id = Some("scroll".into());
        scroll_el.anchor_line = -5;
        engine
            .state
            .elements_mut()
            .insert("scroll".into(), scroll_el);

        // ESC[3J — erase scrollback only.
        engine.process_pty_chunk(b"\x1b[3J");

        assert!(engine.state.elements().contains_key("live"));
        assert!(!engine.state.elements().contains_key("scroll"));
    }

    #[test]
    fn vt100_3j_clears_text_scrollback() {
        // Sanity check: the vendored vt100 fork actually drops its
        // scrollback rows on `ESC[3J`. Without this fork the standard
        // crate silently ignored mode 3.
        let mut parser = vt100::Parser::new(3, 10, 100);
        // Push enough rows to populate scrollback.
        for _ in 0..10 {
            parser.process(b"hello\r\n");
        }
        parser.screen_mut().set_scrollback(usize::MAX);
        assert!(parser.screen().scrollback() > 0);
        parser.process(b"\x1b[3J");
        parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn clear_sequence_drops_live_and_scrollback_elements() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("live", (0.0, 0.0), None, None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        let mut scroll_el =
            engine.state.elements().get("live").cloned().unwrap();
        scroll_el.id = Some("scroll".into());
        scroll_el.anchor_line = -5;
        engine
            .state
            .elements_mut()
            .insert("scroll".into(), scroll_el);

        // Full `clear` sequence.
        engine.process_pty_chunk(b"\x1b[H\x1b[2J\x1b[3J");
        assert!(engine.state.elements().is_empty());
    }

    #[test]
    fn ed_drops_live_region_elements_keeps_scrollback() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // Create one element at the current cursor row (anchored at
        // top_of_live_screen + 0). It lives in the live region.
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            1,
            &create_with_tree("live", (0.0, 0.0), None, None),
        );
        engine.process_pty_chunk(&build_envelope(&frames));
        assert_eq!(engine.state.elements().len(), 1);

        // Forge a second element anchored to a scrollback line by
        // writing it directly into the table (no easy way to anchor to
        // a negative line via the public API without scrolling first).
        let scrollback_line: i64 = -5;
        let mut scrollback_el =
            engine.state.elements().get("live").cloned().unwrap();
        scrollback_el.id = Some("scroll".into());
        scrollback_el.anchor_line = scrollback_line;
        engine
            .state
            .elements_mut()
            .insert("scroll".into(), scrollback_el);

        // ESC[2J — full screen erase.
        engine.process_pty_chunk(b"\x1b[2J");

        // Live element gone, scrollback element survives.
        assert!(!engine.state.elements().contains_key("live"));
        assert!(engine.state.elements().contains_key("scroll"));
    }

    #[test]
    fn reset_while_on_alt_returns_to_main() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        engine.state.enter_alt_screen();
        assert!(engine.state.on_alt());
        engine.process_pty_chunk(b"\x1bc");
        assert!(!engine.state.on_alt());
    }

    #[test]
    fn dsr_query_emits_cursor_position_reply() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut parser = vt100::Parser::new(24, 80, 100);
        // Move the cursor to (row=4, col=11) by writing some text.
        // vt100 reports 0-indexed; DSR replies must be 1-indexed.
        let pre = b"\n\n\n\n           "; // 4 newlines + 11 spaces
        let pass = engine.process_pty_chunk(pre);
        parser.process(&pass);
        engine.after_vt100_process(&mut parser);
        // Drain any responses queued so far (none expected from text).
        let _ = engine.take_responses();

        // Send a cursor-position query; engine queues, vt100 ingests
        // the bytes (passthrough), then we drive after_vt100_process.
        let pass = engine.process_pty_chunk(b"\x1b[6n");
        parser.process(&pass);
        engine.after_vt100_process(&mut parser);

        let reply = engine.take_responses();
        let s = std::str::from_utf8(&reply).unwrap();
        assert_eq!(s, "\x1b[5;12R");
    }

    #[test]
    fn width_resize_does_not_drift_element_anchors() {
        // Regression: the old probe-and-hash line tracker hashed the
        // topmost history row across the *current* column count, so a
        // width shrink read as a phantom scrollback eviction and
        // shifted every anchor up one row per resize step.
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut parser = vt100::Parser::new(10, 80, 100);
        parser.process(&b"line\r\n".repeat(15));
        engine.after_vt100_process(&mut parser);
        let top_before = engine.top_of_live_screen();

        // Window drag: several width steps, each followed by a
        // SIGWINCH-triggered shell redraw.
        for cols in [70u16, 60, 50] {
            parser.screen_mut().set_size(10, cols);
            parser.process(b"$ ");
            engine.after_vt100_process(&mut parser);
        }
        assert_eq!(engine.top_of_live_screen(), top_before);
    }

    #[test]
    fn vertical_resize_roundtrip_keeps_anchor_above_cursor() {
        // Regression: shrinking the window vertically used to truncate
        // the bottom of the vt100 grid, clamping the cursor up into
        // the rows an image (anchored just above the prompt) occupies;
        // growing back then left the prompt typing across the image.
        // With xterm-style push/pull resize the cursor's absolute line
        // — and its distance to the anchor — must survive a
        // shrink/grow cycle.
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let mut parser = vt100::Parser::new(10, 80, 100);
        parser.process(&b"line\r\n".repeat(15));
        engine.after_vt100_process(&mut parser);

        // Element anchored 4 rows above the cursor, like a vcat image
        // above the prompt.
        let cursor_abs = engine.top_of_live_screen()
            + i64::from(parser.screen().cursor_position().0);
        let anchor = cursor_abs - 4;

        for rows in [6u16, 12, 10] {
            parser.screen_mut().set_size(rows, 80);
            parser.process(b"$ ");
            engine.after_vt100_process(&mut parser);
            let cursor_abs_now = engine.top_of_live_screen()
                + i64::from(parser.screen().cursor_position().0);
            assert_eq!(
                cursor_abs_now - anchor,
                4,
                "anchor-to-cursor distance changed at {rows} rows"
            );
        }
    }

    // --- snapshot/replay roundtrip tests ------------------------------

    /// Build a CreateElement body with a single FillRectangles command
    /// using a flat RGBA color, no parent, no clip.
    fn create_top_level_rect(
        id: &str,
        origin_x: f32,
        origin_y: f32,
        draw_order: i32,
        color_g: u8,
    ) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(id);
        w.varu(1); // n_commands
        w.u8(OP_FILL_RECTANGLES);
        // Style: Flat RGBA8888
        w.u8(STYLE_FLAT);
        w.u8(0x01); // RGBA8888
        w.u8(0x00);
        w.u8(color_g);
        w.u8(0x00);
        w.u8(0xFF);
        w.varu(1); // n_rects
        w.f32(0.0);
        w.f32(0.0);
        w.f32(2.0);
        w.f32(1.0);
        // origin
        w.f32(origin_x);
        w.f32(origin_y);
        // is_visible
        w.u8(1);
        // draw_order
        for b in draw_order.to_le_bytes() {
            w.u8(b);
        }
        w.buf
    }

    /// Build a CreateElement for a child of `parent_id`.
    fn create_child_rect(
        id: &str,
        parent_id: &str,
        origin_x: f32,
        origin_y: f32,
        draw_order: i32,
    ) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(id);
        w.varu(1);
        w.u8(OP_FILL_RECTANGLES);
        w.u8(STYLE_FLAT);
        w.u8(0x01);
        w.u8(0xFF);
        w.u8(0xFF);
        w.u8(0x00);
        w.u8(0xFF);
        w.varu(1);
        w.f32(0.0);
        w.f32(0.0);
        w.f32(1.0);
        w.f32(1.0);
        w.f32(origin_x);
        w.f32(origin_y);
        w.u8(1);
        for b in draw_order.to_le_bytes() {
            w.u8(b);
        }
        // extra_flags: bit0 = has_parent
        w.u8(0b0000_0001);
        w.str(parent_id);
        w.buf
    }

    /// Build a `SetGlobalStyle` body for a flat RGBA8 color.
    fn set_global_style_flat(id: &str, r: u8, g: u8, b: u8, a: u8) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(id);
        w.u8(STYLE_FLAT);
        w.u8(0x01); // RGBA8888
        w.u8(r);
        w.u8(g);
        w.u8(b);
        w.u8(a);
        w.buf
    }

    #[test]
    fn req_id_no_response_suppresses_ack_but_applies_state() {
        // The middleman-snapshot contract: a command sent with the
        // `REQ_ID_NO_RESPONSE` sentinel still mutates engine state, but
        // the engine must not emit a response frame. Snapshot replay
        // through a downstream renderer depends on this — without it,
        // the renderer's acks round-trip back into the inner PTY and
        // get interpreted as keystrokes by whatever shell is reading.
        let mut eng = VgeEngine::new((9, 20), 1.0);

        let frames = vec![(Command::Probe, REQ_ID_NO_RESPONSE)];
        let env = vge_protocol::encode::build_envelope(&frames);
        let passthrough = eng.process_pty_chunk(&env);
        assert!(passthrough.is_empty(), "VGE envelope must not leak");
        assert!(
            eng.take_responses().is_empty(),
            "REQ_ID_NO_RESPONSE must suppress the ack frame"
        );

        // Sanity check: a normal req_id _does_ produce a response.
        let frames = vec![(Command::Probe, 7u32)];
        let env = vge_protocol::encode::build_envelope(&frames);
        eng.process_pty_chunk(&env);
        assert!(
            !eng.take_responses().is_empty(),
            "non-sentinel req_id must still get an ack"
        );
    }

    // ---- Binary snapshot (VSS) round-trip tests --------------------

    fn drive_with_envelope(engine: &mut VgeEngine, frames: &[u8]) {
        let env = build_envelope(frames);
        let _passthrough = engine.process_pty_chunk(&env);
        let _ = engine.take_responses();
    }

    /// Build a UploadImage Raw command body (encoding 0x01) for a
    /// 2×2 image. Single-chunk: offset=0, is_last=true.
    fn raw_upload_body(id: &str, color: [u8; 4]) -> Vec<u8> {
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend_from_slice(&color);
        }
        let mut w = vge_protocol::codec::Writer::new();
        w.str(id);
        w.u8(0x01); // encoding=Raw
        w.u32(2);
        w.u32(2);
        w.u32(data.len() as u32);
        w.u32(0);
        w.bool(true);
        w.bytes(&data);
        w.buf
    }

    fn populate_engine() -> VgeEngine {
        let mut e = VgeEngine::new((9, 20), 1.25);

        // Upload an image.
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &raw_upload_body("pic", [1, 2, 3, 4]));
        drive_with_envelope(&mut e, &frames);

        // Set a global style: SetGlobalStyle id="accent" Flat(red).
        let mut body = vge_protocol::codec::Writer::new();
        body.str("accent");
        body.u8(STYLE_FLAT);
        body.u8(COLOR_RGBA8888);
        body.u8(255); body.u8(0); body.u8(0); body.u8(255);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_SET_GLOBAL_STYLE, 2, &body.buf);
        drive_with_envelope(&mut e, &frames);

        // Create an element with one FillRectangles command. Body
        // layout per command.rs CMD_CREATE_ELEMENT decoder:
        //   id, commands, origin (Point), is_visible, draw_order,
        //   optional (extra_flags + parent + size) trailing block.
        let mut body = vge_protocol::codec::Writer::new();
        body.str("el1");
        // commands list — varu count then each DrawCmd.
        body.varu(1);
        body.u8(OP_FILL_RECTANGLES);
        // Style::Flat(blue)
        body.u8(STYLE_FLAT);
        body.u8(COLOR_RGBA8888);
        body.u8(0); body.u8(0); body.u8(255); body.u8(255);
        // rects list
        body.varu(1);
        body.f32(1.0); body.f32(2.0); body.f32(3.0); body.f32(4.0);
        // origin Point (top-level element — anchor logic uses this).
        body.f32(0.0);
        body.f32(0.0);
        // is_visible, draw_order
        body.u8(1);
        body.i32(7);
        // Omit trailing extra block (no parent, no size).
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_CREATE_ELEMENT, 3, &body.buf);
        drive_with_envelope(&mut e, &frames);

        // A second element carrying a transform (§9.11), so snapshot
        // roundtrips cover the v3 field.
        let mut frames = Vec::new();
        append_command(
            &mut frames,
            CMD_CREATE_ELEMENT,
            4,
            &create_with_tree("el2", (3.0, 1.0), None, None),
        );
        append_command(
            &mut frames,
            CMD_UPDATE_TRANSFORM,
            5,
            &update_transform_body("el2", [0.0, 1.0, -1.0, 0.0, 0.5, 1.5]),
        );
        drive_with_envelope(&mut e, &frames);

        e
    }

    #[test]
    fn binary_snapshot_round_trips_byte_equal() {
        let e1 = populate_engine();
        let bytes1 = e1.binary_snapshot();
        let mut e2 = VgeEngine::new((9, 20), 1.0); // different defaults
        e2.restore_from_binary_snapshot(&bytes1).expect("restore");
        let bytes2 = e2.binary_snapshot();
        assert_eq!(bytes1, bytes2);
        // Sanity checks on restored state.
        assert!(e2.state.shared.images.contains_key("pic"));
        assert!(e2.state.shared.styles.contains_key("accent"));
        assert_eq!(e2.state.elements().len(), 2);
        let t = e2.state.elements()["el2"].transform.unwrap();
        assert_eq!((t.a, t.b, t.c, t.d, t.e, t.f), (0.0, 1.0, -1.0, 0.0, 0.5, 1.5));
        assert!(e2.state.elements()["el1"].transform.is_none());
    }

    #[test]
    fn binary_snapshot_preserves_image_source_data() {
        let e1 = populate_engine();
        let bytes1 = e1.binary_snapshot();
        let mut e2 = VgeEngine::new((1, 1), 1.0);
        e2.restore_from_binary_snapshot(&bytes1).unwrap();
        let img1 = e1.state.shared.images.get("pic").unwrap();
        let img2 = e2.state.shared.images.get("pic").unwrap();
        assert_eq!(img1.source_encoding, img2.source_encoding);
        assert_eq!(img1.source_data, img2.source_data);
        assert_eq!(img1.width, img2.width);
        assert_eq!(img1.height, img2.height);
        // Decoded pixels are recomputed; they must match too.
        assert_eq!(img1.pixels, img2.pixels);
        // GPU handle is renderer-private — restored copy must start fresh.
        assert!(img2.gpu.get().is_none());
    }

    #[test]
    fn binary_snapshot_empty_engine_round_trips() {
        let e1 = VgeEngine::new((9, 20), 1.5);
        let bytes1 = e1.binary_snapshot();
        let mut e2 = VgeEngine::new((1, 1), 1.0);
        e2.restore_from_binary_snapshot(&bytes1).unwrap();
        let bytes2 = e2.binary_snapshot();
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn binary_snapshot_carries_cell_px_and_scale() {
        let e1 = VgeEngine::new((11, 24), 2.5);
        let bytes = e1.binary_snapshot();
        let mut e2 = VgeEngine::new((1, 1), 1.0);
        e2.restore_from_binary_snapshot(&bytes).unwrap();
        // Round-trip another snapshot from the restored engine; both
        // engines must serialize identically.
        let bytes2 = e2.binary_snapshot();
        assert_eq!(bytes, bytes2);
    }

    #[test]
    fn binary_snapshot_version_mismatch_rejects() {
        let e = populate_engine();
        let mut bytes = e.binary_snapshot();
        // First two bytes are u16 SNAPSHOT_KIND_VERSION; corrupt them.
        bytes[0] = 0xFF;
        bytes[1] = 0xFF;
        let mut e2 = VgeEngine::new((9, 20), 1.0);
        let err = e2.restore_from_binary_snapshot(&bytes).unwrap_err();
        assert!(
            matches!(err, crate::vge::snapshot::SnapshotError::KindVersionMismatch { .. }),
            "{err:?}",
        );
    }

    #[test]
    fn binary_snapshot_trailing_bytes_reject() {
        let e = populate_engine();
        let mut bytes = e.binary_snapshot();
        bytes.push(0xAA);
        let mut e2 = VgeEngine::new((9, 20), 1.0);
        let err = e2.restore_from_binary_snapshot(&bytes).unwrap_err();
        assert!(
            matches!(err, crate::vge::snapshot::SnapshotError::TrailingBytes),
            "{err:?}",
        );
    }

    #[test]
    fn binary_snapshot_truncated_rejects() {
        let e = populate_engine();
        let bytes = e.binary_snapshot();
        let mut e2 = VgeEngine::new((9, 20), 1.0);
        let err = e2
            .restore_from_binary_snapshot(&bytes[..bytes.len() - 1])
            .unwrap_err();
        assert!(
            matches!(err, crate::vge::snapshot::SnapshotError::BadPayload),
            "{err:?}",
        );
    }
}

