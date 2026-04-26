// VGE engine state: element table, scrollback line tracking, command
// dispatch, and PTY byte plumbing.

use std::cell::Cell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use femtovg::ImageId;
use rgb::RGBA8;
use vge_protocol::apc::ApcStream;
use vge_protocol::codec::{Point, Reader};
use vge_protocol::command::{
    self, Command, ConcreteStyle, CreateElementBody, DrawCmd, UpdateCommandBody,
    UpdateCommandsBody, UpdateImageBody, UpdateTextBody, UpdateTextRange, UploadImageBody,
};
use vge_protocol::envelope::{append_frame, err_body, wrap_t2c_envelope as wrap_envelope, ProbeBody};
use vge_protocol::frame::*;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_elements: u32,
    pub max_commands_per_element: u32,
    pub max_text_bytes: u32,
    pub max_image_bytes: u32,
    pub max_images: u32,
    pub supported_image_encodings: u8,
}

impl Default for Limits {
    fn default() -> Self {
        // Recommended budget (spec §10). Phase III now actually
        // implements images, so the image fields are no longer zero.
        Self {
            max_elements: 4096,
            max_commands_per_element: 4096,
            max_text_bytes: 1_048_576,
            max_image_bytes: 32 * 1024 * 1024,
            max_images: 1024,
            supported_image_encodings: 0b11, // bit0 Raw, bit1 WebP
        }
    }
}

/// An uploaded image, kept in CPU memory as straight-alpha RGBA8 plus a
/// lazily-populated femtovg texture handle. The GPU side is created the
/// first time the renderer encounters this image; `DropImage` queues
/// `gpu` for deletion on the next frame.
///
/// `gpu` is `Cell<Option<ImageId>>` so the renderer can populate it
/// while only holding a `&VgeState` (the renderer doesn't need any
/// other mutation, and `ImageId` is `Copy`).
pub struct UploadedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<RGBA8>,
    pub gpu: Cell<Option<ImageId>>,
    /// Byte size of the *uploaded* representation (Raw bytes or WebP
    /// file). Currently used only for diagnostics; will feed a
    /// per-process byte-budget enforcement layer in a future phase.
    #[allow(dead_code)]
    pub byte_size: u32,
}

#[derive(Debug, Clone)]
pub struct Element {
    /// Some(name) for client-named elements, None for anonymous (§6.1).
    /// Currently unread by the renderer but useful for debugging.
    #[allow(dead_code)]
    pub id: Option<String>,
    pub commands: Vec<DrawCmd>,
    pub anchor_line: i64, // absolute scrollback line
    pub sub_row: f32,
    pub origin_x: f32,
    pub is_visible: bool,
    pub draw_order: i32,
    pub creation_seq: u64,
}

pub struct VgeState {
    pub elements: HashMap<String, Element>,
    /// Global style table (§7.3). Resolved at render time when an element
    /// references a style by ID.
    ///
    /// Session-scoped: this and `images` will move to a separate
    /// `SharedTables` struct when alt-screen support lands (§5.4).
    pub styles: HashMap<String, ConcreteStyle>,
    /// Image table (§8). Uploaded images live here keyed by their
    /// client-supplied ID. Same session scoping note as `styles`.
    pub images: HashMap<String, UploadedImage>,
    creation_counter: u64,
    next_anonymous: u64,
}

impl VgeState {
    pub fn new() -> Self {
        Self {
            elements: HashMap::new(),
            styles: HashMap::new(),
            images: HashMap::new(),
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

    /// Iterate elements in render order: ascending (draw_order, creation_seq).
    pub fn render_sorted(&self) -> Vec<&Element> {
        let mut v: Vec<&Element> = self.elements.values().collect();
        v.sort_by_key(|e| (e.draw_order, e.creation_seq));
        v
    }
}

/// Tracks `top_of_live_screen` (the absolute scrollback line index of
/// vt100's first live-screen row) by probing the parser before/after
/// `parser.process(...)` calls.
struct LineTracker {
    top_of_live_screen: i64,
    prev_history_size: usize,
    /// Cap (max scrollback) of the parser; cached after first probe. Used
    /// to know when we're at saturation and must look for evicted rows.
    history_cap: usize,
    /// Hash of vt100's topmost history row from the previous probe; used
    /// to detect eviction-induced scrolls when the history size is capped.
    prev_top_hash: u64,
    initialized: bool,
}

impl LineTracker {
    fn new() -> Self {
        Self {
            top_of_live_screen: 0,
            prev_history_size: 0,
            history_cap: 0,
            prev_top_hash: 0,
            initialized: false,
        }
    }

    fn update(&mut self, parser: &mut vt100::Parser) {
        let (history_size, top_hash) = probe_history(parser);

        if !self.initialized {
            self.prev_history_size = history_size;
            self.history_cap = history_size; // initial guess; refined below
            self.prev_top_hash = top_hash;
            self.initialized = true;
            return;
        }

        // Track the largest history size we've ever seen as the cap.
        if history_size > self.history_cap {
            self.history_cap = history_size;
        }

        if history_size > self.prev_history_size {
            // Pre-saturation growth: every new history line corresponds to
            // one live-screen scroll, with no eviction.
            let added = history_size - self.prev_history_size;
            self.top_of_live_screen += added as i64;
        } else if history_size == self.prev_history_size
            && self.history_cap > 0
            && history_size == self.history_cap
            && top_hash != self.prev_top_hash
        {
            // At cap, history size doesn't grow but the topmost row
            // changed — at least one eviction. We can't tell exactly how
            // many between probes; counting 1 is a known limitation under
            // heavy paste (documented in the plan).
            self.top_of_live_screen += 1;
        }

        self.prev_history_size = history_size;
        self.prev_top_hash = top_hash;
    }
}

/// Probe the parser for (history_size, hash_of_topmost_history_row).
/// Restores the user's scrollback offset before returning.
fn probe_history(parser: &mut vt100::Parser) -> (usize, u64) {
    let saved = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(usize::MAX);
    let history_size = parser.screen().scrollback();
    // Topmost history row is row 0 when scrolled to the maximum.
    let mut hasher = DefaultHasher::new();
    if history_size > 0 {
        let cols = parser.screen().size().1;
        for col in 0..cols {
            if let Some(cell) = parser.screen().cell(0, col) {
                let s = cell.contents();
                s.hash(&mut hasher);
                // Include color/attrs lightly so identical-glyph but
                // differently-styled lines still differ.
                if let vt100::Color::Rgb(r, g, b) = cell.fgcolor() {
                    r.hash(&mut hasher);
                    g.hash(&mut hasher);
                    b.hash(&mut hasher);
                }
            }
        }
    }
    parser.screen_mut().set_scrollback(saved);
    (history_size, hasher.finish())
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

pub struct VgeEngine {
    apc: ApcStream,
    pub state: VgeState,
    pub limits: Limits,
    cell_px: (u16, u16),
    scale_factor: f32,
    line_tracker: LineTracker,
    pending_response_bytes: Vec<u8>,
    /// femtovg image handles for uploaded images that have been dropped
    /// but whose GPU resources still need releasing. The renderer drains
    /// this on each frame.
    pending_image_deletes: Vec<ImageId>,
}

impl VgeEngine {
    pub fn new(cell_px: (u16, u16), scale_factor: f32) -> Self {
        Self {
            apc: ApcStream::new(),
            state: VgeState::new(),
            limits: Limits::default(),
            cell_px,
            scale_factor,
            line_tracker: LineTracker::new(),
            pending_response_bytes: Vec::new(),
            pending_image_deletes: Vec::new(),
        }
    }

    /// Hand off any image GPU handles whose owners have been dropped.
    /// The renderer should call `canvas.delete_image(id)` for each.
    pub fn take_pending_image_deletes(&mut self) -> Vec<ImageId> {
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

    /// Largest scrollback history size observed so far. Used for
    /// diagnostics; eviction logic uses it directly via line_tracker.
    #[allow(dead_code)]
    pub fn history_cap(&self) -> usize {
        self.line_tracker.history_cap
    }

    /// Ingest raw PTY bytes. Returns the passthrough byte slice that
    /// should be forwarded to vt100. Any complete VGE envelopes are
    /// processed and their responses queued in `take_responses()`.
    pub fn process_pty_chunk(&mut self, input: &[u8]) -> Vec<u8> {
        let out = self.apc.feed(input);
        for payload in out.payloads {
            self.handle_envelope_payload(&payload);
        }
        out.passthrough
    }

    /// Take queued response bytes (an APC envelope) ready to write to the
    /// PTY master.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response_bytes)
    }

    /// Update top-of-live-screen tracking. Call after every
    /// `parser.process(...)`. Also evicts elements whose anchor_line has
    /// fallen off the bottom of scrollback.
    pub fn after_vt100_process(&mut self, parser: &mut vt100::Parser) {
        self.line_tracker.update(parser);
        self.evict();
    }

    fn evict(&mut self) {
        if self.line_tracker.history_cap == 0 {
            return;
        }
        let oldest_visible = self.line_tracker.top_of_live_screen
            - self.line_tracker.history_cap as i64;
        self.state
            .elements
            .retain(|_, e| e.anchor_line >= oldest_visible);
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
        match command::parse(frame_type, body) {
            Err(code) => {
                append_frame(
                    out_frames,
                    RSP_ERR,
                    request_id,
                    &err_body(code, ""),
                );
            }
            Ok(cmd) => match self.apply_command(cmd) {
                Ok(rsp_body) => {
                    let frame_type = match frame_type {
                        CMD_PROBE => RSP_PROBE,
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
            },
        }
    }

    fn apply_command(&mut self, cmd: Command) -> Result<Vec<u8>, (u16, &'static str)> {
        match cmd {
            Command::Probe => {
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
                self.state.elements.clear();
                Ok(Vec::new())
            }
            Command::SetGlobalStyle { id, style } => self.cmd_set_global_style(id, style),
            Command::UploadImage(b) => self.cmd_upload_image(b),
            Command::DropImage { id } => self.cmd_drop_image(&id),
            Command::UpdateImage(b) => self.cmd_update_image(b),
        }
    }

    fn cmd_set_global_style(
        &mut self,
        id: String,
        style: ConcreteStyle,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        // ID validation already done by the parser (non-empty, ≤64 bytes).
        // Upsert per §7.3 — no error on existing ID.
        self.state.styles.insert(id, style);
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
        if self.state.images.contains_key(&b.id) {
            return Err((ERR_DUPLICATE_IMAGE_ID, "image id in use"));
        }
        if self.state.images.len() as u32 >= self.limits.max_images {
            return Err((ERR_TOO_MANY_IMAGES, "image budget exhausted"));
        }
        if b.data.len() as u64 > self.limits.max_image_bytes as u64 {
            return Err((ERR_IMAGE_TOO_LARGE, "image exceeds max_image_bytes"));
        }

        let pixels = match b.encoding {
            0x01 => decode_raw_rgba8(&b.data, b.width, b.height)?,
            0x02 => decode_webp(&b.data, b.width, b.height)?,
            _ => return Err((ERR_BAD_PAYLOAD, "unknown image encoding")),
        };

        self.state.images.insert(
            b.id,
            UploadedImage {
                width: b.width,
                height: b.height,
                pixels,
                gpu: Cell::new(None),
                byte_size: b.data.len() as u32,
            },
        );
        Ok(Vec::new())
    }

    fn cmd_drop_image(&mut self, id: &str) -> Result<Vec<u8>, (u16, &'static str)> {
        if id.is_empty() {
            return Err((ERR_BAD_PAYLOAD, "empty image id"));
        }
        match self.state.images.remove(id) {
            None => Err((ERR_UNKNOWN_IMAGE, "image id not found")),
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
            .elements
            .get(&b.id)
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        if b.command_index >= el.commands.len() {
            return Err((ERR_COMMAND_INDEX, "index out of range"));
        }
        if !matches!(el.commands[b.command_index], DrawCmd::DrawImage { .. }) {
            return Err((ERR_BAD_PAYLOAD, "command at index is not DrawImage"));
        }
        if !self.state.images.contains_key(&b.new_image_id) {
            return Err((ERR_UNKNOWN_IMAGE, "new_image_id not found"));
        }
        let el = self.state.elements.get_mut(&b.id).unwrap();
        if let DrawCmd::DrawImage {
            image_id,
            target_rect: _,
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
                && !self.state.images.contains_key(image_id)
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
        if !b.id.is_empty() && self.state.elements.contains_key(&b.id) {
            return Err((ERR_DUPLICATE_ID, "id in use"));
        }
        if self.state.elements.len() as u32 >= self.limits.max_elements {
            return Err((ERR_TOO_MANY_ELEMENTS, "element budget exhausted"));
        }
        self.validate_commands(&b.commands)?;

        let (anchor, sub) = self.anchor_from_origin(b.origin);
        let key = if b.id.is_empty() {
            self.state.anonymous_key()
        } else {
            b.id.clone()
        };
        let id_field = if b.id.is_empty() { None } else { Some(b.id) };
        let seq = self.state.next_seq();
        self.state.elements.insert(
            key,
            Element {
                id: id_field,
                commands: b.commands,
                anchor_line: anchor,
                sub_row: sub,
                origin_x: b.origin.x,
                is_visible: b.is_visible,
                draw_order: b.draw_order,
                creation_seq: seq,
            },
        );
        Ok(Vec::new())
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
        if self.state.elements.remove(id).is_none() {
            return Err((ERR_UNKNOWN_ELEMENT, "id not found"));
        }
        Ok(Vec::new())
    }

    fn cmd_update_commands(
        &mut self,
        b: UpdateCommandsBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        if !self.state.elements.contains_key(&b.id) {
            return Err((ERR_UNKNOWN_ELEMENT, "id not found"));
        }
        self.validate_commands(&b.commands)?;
        let el = self.state.elements.get_mut(&b.id).unwrap();
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
                .elements
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
            && !self.state.images.contains_key(image_id)
        {
            return Err((ERR_UNKNOWN_IMAGE, "DrawImage references unknown image"));
        }
        let el = self.state.elements.get_mut(&b.id).unwrap();
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
            .elements
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
        let (anchor, sub) = self.anchor_from_origin(origin);
        let el = self
            .state
            .elements
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_ELEMENT, "id not found"))?;
        el.anchor_line = anchor;
        el.sub_row = sub;
        el.origin_x = origin.x;
        Ok(Vec::new())
    }

    fn cmd_update_visibility(
        &mut self,
        id: &str,
        is_visible: bool,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let el = self
            .state
            .elements
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
            .elements
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
        assert!(env.len() >= 7);
        assert_eq!(&env[..2], &[ESC, APC_OPEN]);
        assert_eq!(&env[2..5], MARKER_T2C);
        assert_eq!(&env[env.len() - 2..], &[ESC, ST_CLOSE]);
        // Un-stuff the body.
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
        assert_eq!(r.u32().unwrap(), 31); // body_len for ProbeBody
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
        assert_eq!(r.u16().unwrap(), 1); // protocol_version
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

        assert!(engine.state.elements.contains_key("rect"));
        let el = &engine.state.elements["rect"];
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

        assert!(!engine.state.elements.contains_key("rect"));
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

        assert_eq!(engine.state.styles.len(), 1);
        let s = engine.state.styles.get("accent").unwrap();
        match s {
            command::ConcreteStyle::Flat(c) => {
                assert!((c.g - 1.0).abs() < 1e-3, "second set should win (green)");
            }
            _ => panic!("wrong concrete style kind"),
        }
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

        assert!(engine.state.elements.contains_key("widget"));
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
        assert!(engine.state.elements.is_empty());
    }

    /// Build an `UploadImage` body for a 2x2 RGBA8 raw image (red,
    /// green, blue, white).
    fn upload_raw_2x2(id: &str) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(id);
        w.u8(0x01); // encoding Raw
        w.u32(2); // width
        w.u32(2); // height
        let pixels = [
            0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF,
        ];
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

        assert_eq!(engine.state.images.len(), 1);
        let img = engine.state.images.get("logo").unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 4);
    }

    #[test]
    fn upload_raw_byte_count_mismatch_rejected() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        // Announce 4x4 but send only 16 bytes (actually for 2x2).
        let mut w = Writer::new();
        w.str("bad");
        w.u8(0x01);
        w.u32(4);
        w.u32(4);
        w.bytes(&[0u8; 16]);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &w.buf);
        engine.process_pty_chunk(&build_envelope(&frames));

        assert!(engine.state.images.is_empty());
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
        w.bytes(&[0u8; 4]);
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &w.buf);
        engine.process_pty_chunk(&build_envelope(&frames));
        assert!(engine.state.images.is_empty());
    }

    #[test]
    fn upload_duplicate_id_rejected() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let body = upload_raw_2x2("dup");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 1, &body);
        append_command(&mut frames, CMD_UPLOAD_IMAGE, 2, &body);
        engine.process_pty_chunk(&build_envelope(&frames));

        assert_eq!(engine.state.images.len(), 1);
        let response = engine.take_responses();
        let payload = unwrap_t2c_envelope(&response);
        let mut r = Reader::new(&payload);
        let _ = r.u8();
        let _ = r.u32();
        // Frame 1: Ok.
        assert_eq!(r.u8().unwrap(), RSP_OK);
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
        assert!(engine.state.images.is_empty());
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
        assert!(engine.state.elements.contains_key("widget"));
    }

    #[test]
    fn create_element_with_unknown_image_atomically_fails() {
        let mut engine = VgeEngine::new((9, 20), 1.0);
        let create = create_element_with_draw_image("widget", "missing");
        let mut frames = Vec::new();
        append_command(&mut frames, CMD_CREATE_ELEMENT, 1, &create);
        engine.process_pty_chunk(&build_envelope(&frames));
        // Atomic failure: no element added.
        assert!(engine.state.elements.is_empty());
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

        let el = engine.state.elements.get("widget").unwrap();
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
        let el = engine.state.elements.get("widget").unwrap();
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
        assert!(engine.state.elements.contains_key("widget"));
        assert!(!engine.state.images.contains_key("logo"));
    }
}

