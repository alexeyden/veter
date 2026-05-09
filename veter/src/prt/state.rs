// PRT engine state: portal table per-screen, command dispatch, and
// response framing.
//
// Phase 2 implemented the portal lifecycle (Probe/Create/Delete/
// UpdateSize/UpdateOrigin/UpdateVisibility/UpdateDrawOrder/ClearAll/
// SetFocus/SetCursorStyle). Phase 3 wires the WritePortal byte
// pipeline: route bytes through the portal's PRT ApcStream (sub-portal
// dispatch + side-channel terminal events) → portal vt100 with a
// callback collector → polled state-delta detection → DSR auto-replies.
// Per-portal events (Bell, TitleChange, ClipboardOp, BufferModeChange,
// CursorVisibilityChange, MouseModeChange, RawReply, WorkingDirChange)
// are appended to the response envelope after the WritePortal Ok frame.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use prt_protocol::apc::{ApcStream, TerminalEvent};
use prt_protocol::codec::Reader;
use prt_protocol::command::{
    self, AnchorMode, Command, CreatePortalBody, CursorStyle, FocusTarget, UpdateOriginBody,
    WritePortalBody,
};
use prt_protocol::envelope::{
    append_frame, bell_body, buffer_mode_change_body, clipboard_op_body,
    cursor_visibility_change_body, err_body, icon_name_change_body, mouse_mode_change_body,
    portal_evicted_body, raw_reply_body, resize_notify_body, title_change_body,
    working_dir_change_body, wrap_t2c_envelope, ProbeBody,
};
use prt_protocol::frame::*;

use super::portal::{
    Portal, PortalAnchor, PortalCallbacks, PortalSet, PolledStateCache, RawCallbackEvent,
};

/// Host-side caps published in the probe response (§2.1, §12).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_portals: u32,
    pub max_portal_cells_w: u32,
    pub max_portal_cells_h: u32,
    pub max_scrollback_lines: u32,
    pub max_write_bytes: u32,
    /// §2.1 features bitmask (see `FEAT_*` in `prt_protocol::frame`).
    pub features: u8,
    /// §5.5 nesting cap. We treat this as the maximum allowed engine
    /// depth, where the top-level engine has depth 0. With the spec's
    /// recommended default of 8, sub-engines may exist at depths
    /// 1..=8; CreatePortal at depth 8 fails. `max_nesting_depth = 0`
    /// means no portals at all.
    pub max_nesting_depth: u8,
    /// §10 trailing capability byte. `Some(bits)` advertises the
    /// VGE-integration features; `None` omits the byte entirely (clients
    /// reading a shorter probe response treat missing bits as zero).
    pub vge_features: Option<u8>,
}

impl Default for Limits {
    fn default() -> Self {
        // Recommended caps from §12. Feature bits advertise every event
        // category Phase 3 wires up; alt-screen-in-portal is honoured
        // unconditionally.
        Self {
            max_portals: 64,
            max_portal_cells_w: 1024,
            max_portal_cells_h: 512,
            max_scrollback_lines: 100_000,
            max_write_bytes: 1 << 20,
            features: FEAT_ALT_SCREEN_IN_PORTAL
                | FEAT_EMIT_TITLE_EVENTS
                | FEAT_EMIT_ICON_EVENTS
                | FEAT_EMIT_CWD_EVENTS
                | FEAT_EMIT_CLIPBOARD_EVENTS
                | FEAT_EMIT_BELL_EVENTS
                | FEAT_EMIT_MOUSE_MODE_EVENTS,
            max_nesting_depth: 8,
            vge_features: Some(FEAT_VGE_IN_PORTAL),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusKind {
    Host,
    Portal(String),
}

pub struct PrtState {
    main: PortalSet,
    alt: Option<PortalSet>,
    on_alt: bool,
    pub focus: FocusKind,
    pub cursor_style: CursorStyle,
}

impl PrtState {
    pub fn new() -> Self {
        Self {
            main: PortalSet::new(),
            alt: None,
            on_alt: false,
            focus: FocusKind::Host,
            cursor_style: CursorStyle::Hollow,
        }
    }

    pub fn on_alt(&self) -> bool {
        self.on_alt
    }

    pub fn current(&self) -> &PortalSet {
        if self.on_alt {
            self.alt.as_ref().expect("on_alt without alt set")
        } else {
            &self.main
        }
    }

    pub fn current_mut(&mut self) -> &mut PortalSet {
        if self.on_alt {
            self.alt.as_mut().expect("on_alt without alt set")
        } else {
            &mut self.main
        }
    }

    /// §5.4 — switch to alt with a fresh empty portal set. The main
    /// set is preserved (suspended), not evicted: returning to main
    /// restores it. No PortalEvicted events fire here.
    pub fn enter_alt_screen(&mut self) {
        if !self.on_alt {
            self.alt = Some(PortalSet::new());
            self.on_alt = true;
        }
    }

    /// §5.4 — drop the alt set and restore main. Returns the dropped
    /// alt set so the engine can emit `PortalEvicted reason=2` for
    /// each portal that was on it (§8.7).
    pub fn leave_alt_screen(&mut self) -> Option<PortalSet> {
        if !self.on_alt {
            return None;
        }
        self.on_alt = false;
        self.alt.take()
    }

    /// Walk the focus chain (§9.1, §13.5) starting from this scope and
    /// return the path of portal IDs from outermost to the focused
    /// leaf. An empty path means this scope's host (e.g. the host
    /// vt100 at the top level) is the focused leaf.
    ///
    /// At a portal whose own engine focus is `Portal(X)`, descent
    /// continues into that portal's children. Recursion stops when a
    /// scope's focus is `Host`, or when a referenced portal id no
    /// longer exists (dangling focus is treated as the chain ending
    /// at the last valid step).
    pub fn focus_chain(&self) -> Vec<&str> {
        let mut chain: Vec<&str> = Vec::new();
        let mut cur = self;
        loop {
            match &cur.focus {
                FocusKind::Host => return chain,
                FocusKind::Portal(id) => {
                    chain.push(id.as_str());
                    match cur.current().portals.get(id.as_str()) {
                        Some(p) => cur = &p.children.state,
                        None => return chain,
                    }
                }
            }
        }
    }
}

impl Default for PrtState {
    fn default() -> Self {
        Self::new()
    }
}

/// Output of `PrtEngine::process_pty_chunk_full` — the passthrough
/// bytes destined for the next layer (parent portal's vt100 or, at the
/// top level, the host's vt100), plus any side-channel terminal events
/// the apc stream observed in this chunk.
///
/// Phase 3 only acts on `CursorPositionQuery`; the rest are observed
/// for Phase 4 (RIS/DECSTR scope-clear, EraseDisplay/EraseScrollback
/// portal cleanup).
pub struct ChunkOutput {
    pub passthrough: Vec<u8>,
    pub terminal_events: Vec<TerminalEvent>,
}

pub struct PrtEngine {
    apc: ApcStream,
    pub state: PrtState,
    pub limits: Limits,
    /// Nesting depth of this engine in the portal tree. Top-level host
    /// = 0; a sub-engine inside a depth-d portal is at depth d+1.
    depth: u32,
    pending_response_bytes: Vec<u8>,
    /// Event frames produced during the *current* command's processing
    /// (e.g. WritePortal callbacks, polled state deltas, DSR replies)
    /// and during free-floating phases (e.g. `after_vt100_process`
    /// scrollback eviction). `dispatch_frame` flushes these into the
    /// response envelope after each command's response, satisfying
    /// §1.2; `flush_pending_events` wraps any leftovers into a
    /// standalone event envelope.
    pending_events: Vec<(u8, Vec<u8>)>,
    /// Tracks `top_of_live_screen` (absolute scrollback line index of
    /// the parent vt100's first live-screen row). Updated by
    /// `after_vt100_process`, used by anchor-line resolution and
    /// scrollback eviction.
    line_tracker: LineTracker,
    /// Cell metrics inherited by every per-portal VGE engine spun up
    /// when a portal is created in this scope. Children of a portal
    /// inherit the same metrics — the host grid measures cells
    /// uniformly, and §5.1 says portal cells match host cells.
    cell_px: (u16, u16),
    scale_factor: f32,
    /// GPU image handles that this engine's portal-removal sites
    /// (DeletePortal, ClearAll, scope_reset, the cull/evict family,
    /// alt-screen swap-out) extracted from portals as they were
    /// destroyed, plus anything the per-portal VGE engines themselves
    /// queued. Drained per frame by the renderer via
    /// `take_all_pending_image_deletes` so femtovg's GPU cache stays
    /// in sync with the CPU side.
    pending_image_deletes: Vec<femtovg::ImageId>,
    /// Decoded OSC 52 set payloads observed in any portal's vt100 in
    /// this engine's subtree. Buffered alongside the regular
    /// `EVT_CLIPBOARD_OP` emission so the host (`App`) can apply them
    /// to the system clipboard. Drained by `take_pending_clipboard_writes`.
    pending_clipboard_writes: Vec<String>,
    /// Wakeup closure cloned into every per-portal VFT engine spawned
    /// in this scope (and inherited by sub-engines). When a per-portal
    /// VFT worker emits an event during an idle phase, it pings this
    /// to tick the host's main loop so `drive_and_flush_vft` runs and
    /// the event surfaces as a RawReply.
    vft_wakeup: crate::vft::Wakeup,
}

impl PrtEngine {
    /// Construct the host's top-level PRT engine with default cell
    /// metrics (suitable for unit tests; `main.rs` should use
    /// `with_metrics_and_wakeup` so per-portal VFT engines can wake
    /// the event loop).
    pub fn new() -> Self {
        Self::with_all(
            Limits::default(),
            0,
            (8, 16),
            1.0,
            std::sync::Arc::new(|| {}),
        )
    }

    /// Production constructor for the host's top-level engine. The
    /// supplied wakeup is shared with every per-portal VFT engine
    /// spawned in this scope (and inherited by sub-engines), so a
    /// download chunk emitted by a worker inside a deeply nested
    /// portal still ticks the same main loop that drives the host.
    pub fn with_metrics_and_wakeup(
        cell_px: (u16, u16),
        scale_factor: f32,
        vft_wakeup: crate::vft::Wakeup,
    ) -> Self {
        Self::with_all(Limits::default(), 0, cell_px, scale_factor, vft_wakeup)
    }

    /// Construct with explicit limits. Used by tests that need to pin
    /// caps; metrics default to the test-friendly values.
    #[allow(dead_code)] // tests-only
    pub fn with_limits(limits: Limits, depth: u32) -> Self {
        Self::with_all(limits, depth, (8, 16), 1.0, std::sync::Arc::new(|| {}))
    }

    fn with_all(
        limits: Limits,
        depth: u32,
        cell_px: (u16, u16),
        scale_factor: f32,
        vft_wakeup: crate::vft::Wakeup,
    ) -> Self {
        Self {
            apc: ApcStream::new(),
            state: PrtState::new(),
            limits,
            depth,
            pending_response_bytes: Vec::new(),
            pending_events: Vec::new(),
            line_tracker: LineTracker::new(),
            cell_px,
            scale_factor,
            pending_image_deletes: Vec::new(),
            pending_clipboard_writes: Vec::new(),
            vft_wakeup,
        }
    }

    /// Drain every pending image-delete in this engine's subtree:
    /// (a) IDs accumulated from portal removals at this scope,
    /// (b) per-portal VGE engines' pending DropImage IDs,
    /// (c) recursively, the same from each portal's sub-engine.
    ///
    /// Caller (renderer) is responsible for `canvas.delete_image(id)`
    /// on each ID.
    pub fn take_all_pending_image_deletes(&mut self) -> Vec<femtovg::ImageId> {
        let mut deletes: Vec<femtovg::ImageId> =
            std::mem::take(&mut self.pending_image_deletes);
        Self::collect_pending_from_set(&mut self.state.main, &mut deletes);
        if let Some(alt) = &mut self.state.alt {
            Self::collect_pending_from_set(alt, &mut deletes);
        }
        deletes
    }

    fn collect_pending_from_set(
        set: &mut PortalSet,
        out: &mut Vec<femtovg::ImageId>,
    ) {
        for portal in set.portals.values_mut() {
            out.extend(portal.vge.take_pending_image_deletes());
            out.extend(portal.children.take_all_pending_image_deletes());
        }
    }

    /// Drain every GPU image handle reachable from this engine — its
    /// own scope-pending deletes plus every portal's subtree. Invoked
    /// by `Portal::drain_for_destroy` when a parent portal is being
    /// torn down so all the per-portal VGE engines below it surrender
    /// their handles in one pass.
    pub fn take_subtree_for_destroy(&mut self) -> Vec<femtovg::ImageId> {
        let mut deletes: Vec<femtovg::ImageId> =
            std::mem::take(&mut self.pending_image_deletes);
        for portal in self.state.main.portals.values_mut() {
            deletes.extend(portal.drain_for_destroy());
        }
        if let Some(alt) = &mut self.state.alt {
            for portal in alt.portals.values_mut() {
                deletes.extend(portal.drain_for_destroy());
            }
        }
        deletes
    }

    /// Top-of-live-screen exposed for anchor-line math (rendering).
    /// `0` until the engine's first `after_vt100_process` call.
    #[allow(dead_code)] // Phase 6 (renderer reads this)
    pub fn top_of_live_screen(&self) -> i64 {
        self.line_tracker.top_of_live_screen
    }

    #[allow(dead_code)] // Phase 6 / introspection
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Drain queued response bytes (one or more APC envelopes) ready to
    /// write to the PTY master.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response_bytes)
    }

    /// Drain decoded OSC 52 set payloads observed in any portal under
    /// this engine since the last call. The host (`App`) feeds these to
    /// the system clipboard; the corresponding `EVT_CLIPBOARD_OP` PRT
    /// frames are still emitted to the engine's client.
    pub fn take_pending_clipboard_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_clipboard_writes)
    }

    /// Ingest raw PTY bytes. Returns the passthrough byte slice that
    /// should be forwarded to the next layer (VGE, then vt100).
    ///
    /// Convenience wrapper around `process_pty_chunk_full` for callers
    /// that don't need the terminal-event surface — kept for API
    /// symmetry with the VGE engine; `main.rs` uses the `_full` variant
    /// because it needs to feed the events back into
    /// `handle_terminal_events`.
    #[allow(dead_code)]
    pub fn process_pty_chunk(&mut self, input: &[u8]) -> Vec<u8> {
        self.process_pty_chunk_full(input).passthrough
    }

    /// Variant of `process_pty_chunk` that also surfaces the terminal
    /// events observed in this chunk. The caller decides what to do
    /// with them — the engine itself does not interpret them, since
    /// most are scoped to a vt100 the engine doesn't own.
    pub fn process_pty_chunk_full(&mut self, input: &[u8]) -> ChunkOutput {
        let out = self.apc.feed(input);
        for payload in out.payloads {
            self.handle_envelope_payload(&payload);
        }
        ChunkOutput {
            passthrough: out.passthrough,
            terminal_events: out.events,
        }
    }

    /// Queue an event frame to be flushed into the current response
    /// envelope at the end of the current command's dispatch (§1.2).
    fn emit_event(&mut self, frame_type: u8, body: Vec<u8>) {
        self.pending_events.push((frame_type, body));
    }

    /// Wrap any leftover events into a standalone t2c envelope and
    /// append to `pending_response_bytes`. Called after free-floating
    /// event sources (terminal-event reactions, alt-screen swap,
    /// scrollback eviction) — events generated mid-command are
    /// flushed by `dispatch_frame` directly into the response envelope.
    pub fn flush_pending_events(&mut self) {
        if self.pending_events.is_empty() {
            return;
        }
        let mut frames = Vec::new();
        for (frame_type, body) in self.pending_events.drain(..) {
            append_frame(&mut frames, frame_type, 0, &body);
        }
        self.queue_envelope(frames);
    }

    /// Drive every per-portal VFT engine in this scope's active
    /// portal set, recursively, and emit any responses they produced
    /// as RawReply events for their containing portal (§10
    /// vft-in-portal). Suspended sets (e.g. main while on alt) are
    /// not driven in v1; their workers buffer events and drain on
    /// the next screen swap. Calls `flush_pending_events` at the end
    /// so the resulting envelopes ride out via the next
    /// `take_responses`.
    pub fn drive_and_flush_vft(&mut self) {
        let portal_ids: Vec<String> = self
            .state
            .current()
            .portals
            .keys()
            .cloned()
            .collect();
        for id in portal_ids {
            let bundle = {
                let Some(portal) = self.state.current_mut().portals.get_mut(&id) else {
                    continue;
                };
                portal.vft.drive();
                portal.children.drive_and_flush_vft();
                let mut b = portal.vft.take_responses();
                b.extend_from_slice(&portal.children.take_responses());
                b
            };
            if !bundle.is_empty() {
                self.emit_event(EVT_RAW_REPLY, raw_reply_body(&id, &bundle));
            }
        }
        self.flush_pending_events();
    }

    /// React to a slice of `TerminalEvent`s observed by an apc stream
    /// scoped to this engine: HardReset/SoftReset wipe this engine's
    /// active portal set, EraseDisplay/EraseScrollback cull it, and
    /// CursorPositionQuery is left to the caller (the parent vt100
    /// owner answers it; the engine itself has no vt100 to query).
    pub fn handle_terminal_events(&mut self, events: &[TerminalEvent]) {
        for ev in events {
            match ev {
                TerminalEvent::HardReset | TerminalEvent::SoftReset => self.scope_reset(),
                TerminalEvent::EraseDisplay => self.cull_for_erase_display(),
                TerminalEvent::EraseScrollback => self.cull_for_erase_scrollback(),
                TerminalEvent::CursorPositionQuery => {}
            }
        }
    }

    /// Update line tracking and react to alt-screen transitions on the
    /// vt100 this engine is anchored to. At the host level this is the
    /// host's vt100; inside a portal this is the parent portal's
    /// vt100. Drops portals that have fallen off the bottom of
    /// scrollback (PortalEvicted reason=0) and emits PortalEvicted
    /// reason=2 for any alt-set portals dropped on alt-screen exit.
    pub fn after_vt100_process<CB: vt100::Callbacks>(
        &mut self,
        parser: &mut vt100::Parser<CB>,
    ) {
        // §5.4 — detect alt-screen transitions by polling vt100.
        let now_alt = parser.screen().alternate_screen();
        if now_alt && !self.state.on_alt() {
            self.state.enter_alt_screen();
            // No eviction events on entry: main set is suspended, not
            // dropped.
        } else if !now_alt && self.state.on_alt() {
            if let Some(mut dropped) = self.state.leave_alt_screen() {
                for (id, mut portal) in dropped.portals.drain() {
                    self.pending_image_deletes
                        .extend(portal.drain_for_destroy());
                    self.emit_event(
                        EVT_PORTAL_EVICTED,
                        portal_evicted_body(&id, EVICT_ALT_SWAP),
                    );
                }
            }
            // Line tracker meaningfully tracks the main screen only;
            // re-prime against the freshly-restored main vt100 state.
            self.line_tracker.clear();
        }

        // Anchor math is meaningful only on the main screen — the alt
        // screen has no scrollback (§5.3 per VGE precedent).
        if !self.state.on_alt() {
            self.line_tracker.update(parser);
            self.evict_off_scrollback();
        }
    }

    /// §5.7 RIS / DECSTR scope reset: wipe every portal on the active
    /// set, emit PortalEvicted reason=1 for each, and reset focus.
    fn scope_reset(&mut self) {
        if self.state.current().portals.is_empty()
            && matches!(self.state.focus, FocusKind::Host)
        {
            return;
        }
        let drained: Vec<(String, Portal)> =
            self.state.current_mut().portals.drain().collect();
        let ids: Vec<String> = drained.iter().map(|(id, _)| id.clone()).collect();
        for (_, mut portal) in drained {
            self.pending_image_deletes
                .extend(portal.drain_for_destroy());
        }
        for id in &ids {
            self.emit_event(
                EVT_PORTAL_EVICTED,
                portal_evicted_body(id, EVICT_ERASE),
            );
        }
        if matches!(self.state.focus, FocusKind::Portal(_)) {
            self.state.focus = FocusKind::Host;
        }
        // Line tracking is re-derived on the next `after_vt100_process`.
        self.line_tracker.clear();
    }

    /// §5.8 ESC[2J: drop every portal whose effective anchor lies in
    /// the live region. For Live portals that's always; for Scrollback
    /// portals it means anchor_line >= top_of_live_screen.
    fn cull_for_erase_display(&mut self) {
        let top = self.line_tracker.top_of_live_screen;
        let to_drop: Vec<String> = self
            .state
            .current()
            .portals
            .iter()
            .filter_map(|(id, p)| match p.anchor {
                PortalAnchor::Live { .. } => Some(id.clone()),
                PortalAnchor::Scrollback { anchor_line } if anchor_line >= top => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in to_drop {
            if let Some(mut portal) = self.state.current_mut().portals.remove(&id) {
                self.pending_image_deletes
                    .extend(portal.drain_for_destroy());
            }
            self.emit_event(
                EVT_PORTAL_EVICTED,
                portal_evicted_body(&id, EVICT_ERASE),
            );
        }
    }

    /// §5.8 ESC[3J: drop every Scrollback portal whose anchor_line is
    /// in the scrollback region (anchor_line < top_of_live_screen).
    /// Live portals are unaffected.
    fn cull_for_erase_scrollback(&mut self) {
        let top = self.line_tracker.top_of_live_screen;
        let to_drop: Vec<String> = self
            .state
            .current()
            .portals
            .iter()
            .filter_map(|(id, p)| match p.anchor {
                PortalAnchor::Scrollback { anchor_line } if anchor_line < top => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in to_drop {
            if let Some(mut portal) = self.state.current_mut().portals.remove(&id) {
                self.pending_image_deletes
                    .extend(portal.drain_for_destroy());
            }
            self.emit_event(
                EVT_PORTAL_EVICTED,
                portal_evicted_body(&id, EVICT_ERASE),
            );
        }
    }

    /// §5.2 — drop Scrollback portals whose anchor_line has fallen off
    /// the bottom of the parser's scrollback ring. Emit PortalEvicted
    /// reason=0 for each.
    fn evict_off_scrollback(&mut self) {
        if self.line_tracker.history_cap == 0 {
            return;
        }
        let oldest_visible = self.line_tracker.top_of_live_screen
            - self.line_tracker.history_cap as i64;
        let to_evict: Vec<String> = self
            .state
            .current()
            .portals
            .iter()
            .filter_map(|(id, p)| match p.anchor {
                PortalAnchor::Scrollback { anchor_line } if anchor_line < oldest_visible => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in to_evict {
            if let Some(mut portal) = self.state.current_mut().portals.remove(&id) {
                self.pending_image_deletes
                    .extend(portal.drain_for_destroy());
            }
            self.emit_event(
                EVT_PORTAL_EVICTED,
                portal_evicted_body(&id, EVICT_SCROLLBACK),
            );
        }
    }

    fn queue_envelope(&mut self, frames_buf: Vec<u8>) {
        let env = wrap_t2c_envelope(&frames_buf);
        self.pending_response_bytes.extend_from_slice(&env);
    }

    fn handle_envelope_payload(&mut self, payload: &[u8]) {
        let mut frames_buf: Vec<u8> = Vec::new();

        let mut r = Reader::new(payload);
        let version = match r.u8() {
            Ok(v) => v,
            Err(_) => return, // can't even respond — corrupt envelope
        };
        if version > PROTOCOL_VERSION {
            // Future version we can't safely parse. Surface unsupported
            // with request_id 0 (we haven't read it yet).
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
                append_frame(out_frames, RSP_ERR, request_id, &err_body(code, ""));
            }
            Ok(cmd) => match self.apply_command(cmd) {
                Ok(rsp_body) => {
                    let rsp_type = if frame_type == CMD_PROBE { RSP_PROBE } else { RSP_OK };
                    append_frame(out_frames, rsp_type, request_id, &rsp_body);
                }
                Err((code, msg)) => {
                    append_frame(out_frames, RSP_ERR, request_id, &err_body(code, msg));
                }
            },
        }
        // §1.2: events generated during a command appear after that
        // command's response, in the same envelope.
        for (event_type, event_body) in self.pending_events.drain(..) {
            append_frame(out_frames, event_type, 0, &event_body);
        }
    }

    fn apply_command(&mut self, cmd: Command) -> Result<Vec<u8>, (u16, &'static str)> {
        match cmd {
            Command::Probe => self.cmd_probe(),
            Command::CreatePortal(b) => self.cmd_create_portal(b),
            Command::DeletePortal { id } => self.cmd_delete_portal(&id),
            Command::UpdateSize { id, new_w, new_h } => {
                self.cmd_update_size(&id, new_w, new_h)
            }
            Command::UpdateOrigin(b) => self.cmd_update_origin(b),
            Command::UpdateVisibility { id, is_visible } => {
                self.cmd_update_visibility(&id, is_visible)
            }
            Command::UpdateDrawOrder { id, draw_order } => {
                self.cmd_update_draw_order(&id, draw_order)
            }
            Command::ClearAll => self.cmd_clear_all(),
            Command::WritePortal(b) => self.cmd_write_portal(b),
            Command::SetFocus { target } => self.cmd_set_focus(target),
            Command::SetCursorStyle { unfocused } => self.cmd_set_cursor_style(unfocused),
            Command::SetPortalScrollback { id, lines } => {
                self.cmd_set_portal_scrollback(&id, lines)
            }
        }
    }

    fn cmd_probe(&mut self) -> Result<Vec<u8>, (u16, &'static str)> {
        let pb = ProbeBody {
            protocol_version: u16::from(PROTOCOL_VERSION),
            max_portals: self.limits.max_portals,
            max_portal_cells_w: self.limits.max_portal_cells_w,
            max_portal_cells_h: self.limits.max_portal_cells_h,
            max_scrollback_lines: self.limits.max_scrollback_lines,
            max_write_bytes: self.limits.max_write_bytes,
            features: self.limits.features,
            max_nesting_depth: self.limits.max_nesting_depth,
            vge_features: self.limits.vge_features,
        };
        Ok(pb.encode())
    }

    fn cmd_create_portal(
        &mut self,
        b: CreatePortalBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        // Validate atomically — no mutation until every check passes.
        if b.size_w == 0 || b.size_h == 0 {
            return Err((ERR_SIZE_OUT_OF_RANGE, "size must be >= 1"));
        }
        if b.size_w > self.limits.max_portal_cells_w
            || b.size_h > self.limits.max_portal_cells_h
        {
            return Err((ERR_SIZE_OUT_OF_RANGE, "size exceeds cap"));
        }
        // §5.5 nesting check happens at the engine that would *contain*
        // the new sub-engine: that's `self`. The new sub-engine has
        // depth `self.depth + 1`, which must not exceed
        // `max_nesting_depth`.
        if self.depth >= u32::from(self.limits.max_nesting_depth) {
            return Err((ERR_MAX_NESTING_DEPTH, "max_nesting_depth exceeded"));
        }
        {
            let set = self.state.current();
            if set.portals.contains_key(&b.id) {
                return Err((ERR_DUPLICATE_ID, "id in use"));
            }
            if (set.portals.len() as u32) >= self.limits.max_portals {
                return Err((ERR_TOO_MANY_PORTALS, "portal budget exhausted"));
            }
        }

        // Mutate.
        let scrollback =
            (b.scrollback_lines.min(self.limits.max_scrollback_lines)) as usize;
        let rows = b.size_h as u16;
        let cols = b.size_w as u16;
        let vt = vt100::Parser::new_with_callbacks(
            rows,
            cols,
            scrollback,
            PortalCallbacks::default(),
        );
        let initial_cache = PolledStateCache::from_screen(vt.screen());
        let anchor = match b.anchor_mode {
            AnchorMode::Live => PortalAnchor::Live {
                origin_y: b.origin_y,
            },
            AnchorMode::Scrollback => PortalAnchor::Scrollback {
                anchor_line: self.line_tracker.top_of_live_screen + i64::from(b.origin_y),
            },
        };
        let child_engine = PrtEngine::with_all(
            self.limits,
            self.depth + 1,
            self.cell_px,
            self.scale_factor,
            self.vft_wakeup.clone(),
        );
        let mut portal_vge = crate::vge::VgeEngine::new(self.cell_px, self.scale_factor);
        // §10 + §13.4 — leave PRT as the sole DSR responder inside the
        // portal so an inner `\x1b[6n` produces exactly one reply.
        portal_vge.set_auto_reply_dsr(false);

        // §10 (vft-in-portal) — every portal owns its own VFT engine.
        // Workers share the host's wakeup so async events from any
        // depth tick the host's main loop.
        let portal_vft = crate::vft::VftEngine::with_wakeup(self.vft_wakeup.clone());

        let set = self.state.current_mut();
        let creation_seq = set.next_seq();
        let portal = Portal {
            id: b.id.clone(),
            size_w: b.size_w,
            size_h: b.size_h,
            origin_x: b.origin_x,
            anchor,
            is_visible: b.is_visible,
            draw_order: b.draw_order,
            creation_seq,
            scrollback_lines: scrollback as u32,
            vt,
            children: child_engine,
            vge: portal_vge,
            vft: portal_vft,
            state_cache: initial_cache,
            pending_cursor_queries: 0,
        };
        set.portals.insert(b.id, portal);
        Ok(Vec::new())
    }

    fn cmd_delete_portal(&mut self, id: &str) -> Result<Vec<u8>, (u16, &'static str)> {
        let mut portal = match self.state.current_mut().portals.remove(id) {
            Some(p) => p,
            None => return Err((ERR_UNKNOWN_PORTAL, "id not found")),
        };
        let deletes = portal.drain_for_destroy();
        self.pending_image_deletes.extend(deletes);
        Ok(Vec::new())
    }

    fn cmd_update_size(
        &mut self,
        id: &str,
        new_w: u32,
        new_h: u32,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        if new_w == 0 || new_h == 0 {
            return Err((ERR_SIZE_OUT_OF_RANGE, "size must be >= 1"));
        }
        if new_w > self.limits.max_portal_cells_w || new_h > self.limits.max_portal_cells_h
        {
            return Err((ERR_SIZE_OUT_OF_RANGE, "size exceeds cap"));
        }
        let portal = self
            .state
            .current_mut()
            .portals
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_PORTAL, "id not found"))?;
        let changed = portal.size_w != new_w || portal.size_h != new_h;
        portal.vt.screen_mut().set_size(new_h as u16, new_w as u16);
        portal.size_w = new_w;
        portal.size_h = new_h;
        // §5.6 / §6.3 — emit ResizeNotify only on a material change so
        // idempotent UpdateSize calls don't generate spurious events.
        if changed {
            self.emit_event(EVT_RESIZE_NOTIFY, resize_notify_body(id, new_h, new_w));
        }
        Ok(Vec::new())
    }

    fn cmd_update_origin(
        &mut self,
        b: UpdateOriginBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let top = self.line_tracker.top_of_live_screen;
        let portal = self
            .state
            .current_mut()
            .portals
            .get_mut(&b.id)
            .ok_or((ERR_UNKNOWN_PORTAL, "id not found"))?;
        let current = match portal.anchor {
            PortalAnchor::Live { .. } => AnchorMode::Live,
            PortalAnchor::Scrollback { .. } => AnchorMode::Scrollback,
        };
        // §6.4 — mode swap is not allowed; client must echo the portal's
        // current mode.
        if b.anchor_mode != current {
            return Err((ERR_BAD_PAYLOAD, "anchor_mode mismatch"));
        }
        portal.origin_x = b.new_origin_x;
        portal.anchor = match b.anchor_mode {
            AnchorMode::Live => PortalAnchor::Live {
                origin_y: b.new_origin_y,
            },
            AnchorMode::Scrollback => PortalAnchor::Scrollback {
                anchor_line: top + i64::from(b.new_origin_y),
            },
        };
        Ok(Vec::new())
    }

    fn cmd_update_visibility(
        &mut self,
        id: &str,
        is_visible: bool,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let portal = self
            .state
            .current_mut()
            .portals
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_PORTAL, "id not found"))?;
        portal.is_visible = is_visible;
        Ok(Vec::new())
    }

    fn cmd_update_draw_order(
        &mut self,
        id: &str,
        draw_order: i32,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let portal = self
            .state
            .current_mut()
            .portals
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_PORTAL, "id not found"))?;
        portal.draw_order = draw_order;
        Ok(Vec::new())
    }

    fn cmd_clear_all(&mut self) -> Result<Vec<u8>, (u16, &'static str)> {
        let drained: Vec<(String, Portal)> =
            self.state.current_mut().portals.drain().collect();
        for (_, mut portal) in drained {
            self.pending_image_deletes
                .extend(portal.drain_for_destroy());
        }
        Ok(Vec::new())
    }

    fn cmd_write_portal(
        &mut self,
        b: WritePortalBody,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        // Atomicity (§7.1): validate before consuming any bytes. The
        // failure paths must leave the inner vt100 untouched.
        if (b.data.len() as u64) > u64::from(self.limits.max_write_bytes) {
            return Err((ERR_WRITE_TOO_LARGE, "data exceeds max_write_bytes"));
        }
        if !self.state.current().portals.contains_key(&b.id) {
            return Err((ERR_UNKNOWN_PORTAL, "id not found"));
        }

        // ---- pipeline ------------------------------------------------
        // We collect everything into locals inside a borrow scope, then
        // emit events on `self` after the borrow ends — `emit_event`
        // takes `&mut self` and would otherwise alias.
        let (raw_events, old_cache, new_cache, reverse_bytes) = {
            let portal = self
                .state
                .current_mut()
                .portals
                .get_mut(&b.id)
                .expect("contains_key checked above");

            // 1. Route through the portal's PRT ApcStream. Sub-portal
            //    envelopes embedded in the byte stream are dispatched
            //    against `portal.children`'s scope. Terminal events
            //    (CursorPositionQuery, RIS/DECSTR/2J/3J inside portal)
            //    are surfaced for us to act on against THIS portal's
            //    vt100 and its sub-portal scope.
            let chunk = portal.children.process_pty_chunk_full(&b.data);
            for ev in &chunk.terminal_events {
                if matches!(ev, TerminalEvent::CursorPositionQuery) {
                    portal.pending_cursor_queries =
                        portal.pending_cursor_queries.saturating_add(1);
                }
            }

            // §5.7 / §5.8 — RIS / DECSTR / 2J / 3J observed inside this
            // portal's byte stream are scoped to this portal: they
            // wipe / cull `portal.children`'s sub-portal table. The
            // bytes themselves still flow to portal.vt below, which
            // also resets / erases its own grid.
            portal.children.handle_terminal_events(&chunk.terminal_events);

            // §10 (vft-in-portal) — RIS / DECSTR inside the portal
            // also abort every transfer in this portal's VFT engine.
            // EraseDisplay / EraseScrollback do not affect VFT (file
            // transfer carries no on-screen state).
            for ev in &chunk.terminal_events {
                if matches!(ev, TerminalEvent::HardReset | TerminalEvent::SoftReset) {
                    portal
                        .vft
                        .abort_all(vft_protocol::frame::ABORT_HOST_RESET, "");
                }
            }

            // 2. §10 — per-portal VGE. Extract any ESC_VGE envelopes
            //    from the bytes and apply them against the portal's
            //    private VGE state. The engine internally handles its
            //    own RIS/DECSTR/2J/3J reactions on its own apc, so
            //    inside-portal scoping for VGE elements works the same
            //    way it does for sub-portals.
            let vge_passthrough = portal.vge.process_pty_chunk(&chunk.passthrough);

            // 2b. §10 (vft-in-portal) — extract any ESC_VFT envelopes
            //     from the post-VGE passthrough. VFT has no on-screen
            //     state so it does not observe terminal events on its
            //     own apc; the abort_all call above covers RIS/DECSTR.
            let vft_passthrough = portal.vft.process_pty_chunk(&vge_passthrough);

            // Drain any worker events that arrived synchronously
            // during this command (e.g. a Finalised reply for an
            // EndUpload that fsync'd very fast on local SSD).
            portal.vft.drive();

            // 3. Feed the remaining bytes into the portal's vt100. The
            //    `PortalCallbacks` instance accumulates Bell / Title /
            //    Icon / Clipboard / OSC events.
            portal.vt.process(&vft_passthrough);
            let raw_events = std::mem::take(&mut portal.vt.callbacks_mut().events);

            // 4. Update the children engine's line tracker against the
            //    parent portal's vt100 (sub-portal anchoring is
            //    relative to that vt100), and run any pending alt-
            //    screen / scrollback eviction logic. Then do the same
            //    for the per-portal VGE engine, whose elements anchor
            //    to the same vt100.
            portal.children.after_vt100_process(&mut portal.vt);
            portal.children.flush_pending_events();
            portal.vge.after_vt100_process(&mut portal.vt);

            // 5. Snapshot polled state and detect deltas.
            let old_cache = portal.state_cache;
            let new_cache = PolledStateCache::from_screen(portal.vt.screen());
            portal.state_cache = new_cache;

            // 6. Reverse-channel byte stream for this portal:
            //    a. Sub-engine outbound (responses + events from
            //       sub-portal commands embedded in the inbound bytes,
            //       plus any portal-eviction events the children
            //       engine just queued).
            //    b. Per-portal VGE outbound (responses to ESC_VGE
            //       commands the inner program issued — its own probe,
            //       upload-image acks, etc.).
            //    c. DSR cursor-position auto-replies (PRT is the sole
            //       responder; the per-portal VGE has its DSR auto-
            //       reply turned off so this isn't doubled).
            //    All surface to the parent client as a single RawReply.
            let mut reverse = portal.children.take_responses();
            reverse.extend_from_slice(&portal.vge.take_responses());
            reverse.extend_from_slice(&portal.vft.take_responses());
            if portal.pending_cursor_queries > 0 {
                let (row, col) = portal.vt.screen().cursor_position();
                let reply = format!("\x1b[{};{}R", u32::from(row) + 1, u32::from(col) + 1);
                for _ in 0..portal.pending_cursor_queries {
                    reverse.extend_from_slice(reply.as_bytes());
                }
                portal.pending_cursor_queries = 0;
            }

            (raw_events, old_cache, new_cache, reverse)
        };

        // ---- emit events ---------------------------------------------
        if !reverse_bytes.is_empty() {
            self.emit_event(EVT_RAW_REPLY, raw_reply_body(&b.id, &reverse_bytes));
        }
        for ev in raw_events {
            self.translate_callback_event(&b.id, ev);
        }
        if old_cache.on_alt != new_cache.on_alt {
            self.emit_event(
                EVT_BUFFER_MODE_CHANGE,
                buffer_mode_change_body(&b.id, new_cache.on_alt),
            );
        }
        if old_cache.cursor_visible != new_cache.cursor_visible {
            self.emit_event(
                EVT_CURSOR_VISIBILITY_CHANGE,
                cursor_visibility_change_body(&b.id, new_cache.cursor_visible),
            );
        }
        if old_cache.mouse_protocol != new_cache.mouse_protocol
            || old_cache.mouse_encoding != new_cache.mouse_encoding
            || old_cache.focus_events != new_cache.focus_events
        {
            self.emit_event(
                EVT_MOUSE_MODE_CHANGE,
                mouse_mode_change_body(
                    &b.id,
                    new_cache.mouse_protocol,
                    new_cache.mouse_encoding,
                    new_cache.focus_events,
                ),
            );
        }

        Ok(Vec::new())
    }

    /// Translate one `RawCallbackEvent` into the matching PRT event.
    /// Base64 is decoded for OSC 52 set form per §8.4; OSC 7 is
    /// recognised by leading-parameter match.
    fn translate_callback_event(&mut self, id: &str, ev: RawCallbackEvent) {
        match ev {
            RawCallbackEvent::Bell => {
                self.emit_event(EVT_BELL, bell_body(id));
            }
            RawCallbackEvent::Title(t) => {
                let title = String::from_utf8_lossy(&t).into_owned();
                self.emit_event(EVT_TITLE_CHANGE, title_change_body(id, &title));
            }
            RawCallbackEvent::IconName(n) => {
                let name = String::from_utf8_lossy(&n).into_owned();
                self.emit_event(EVT_ICON_NAME_CHANGE, icon_name_change_body(id, &name));
            }
            RawCallbackEvent::ClipboardSet { selection, data } => {
                // §8.4 — selection is a single ASCII byte. xterm allows
                // multi-char selectors (e.g. "cp"); take the first.
                let sel = selection.first().copied().unwrap_or(b'c');
                let decoded = b64_decode(&data).unwrap_or_default();
                // Surface the text to the host (App) so it can update
                // the system clipboard. We still emit the PRT event
                // unchanged — clients (e.g. nested vmux) may also care.
                if let Ok(text) = std::str::from_utf8(&decoded) {
                    self.pending_clipboard_writes.push(text.to_owned());
                }
                self.emit_event(
                    EVT_CLIPBOARD_OP,
                    clipboard_op_body(id, sel, CLIPBOARD_SET, &decoded),
                );
            }
            RawCallbackEvent::ClipboardQuery { selection } => {
                let sel = selection.first().copied().unwrap_or(b'c');
                self.emit_event(
                    EVT_CLIPBOARD_OP,
                    clipboard_op_body(id, sel, CLIPBOARD_QUERY, b""),
                );
            }
            RawCallbackEvent::Osc(params) => {
                // OSC 7 ; <uri>  →  WorkingDirChange.
                if params.len() == 2 && params[0] == b"7" {
                    let uri = String::from_utf8_lossy(&params[1]).into_owned();
                    self.emit_event(
                        EVT_WORKING_DIR_CHANGE,
                        working_dir_change_body(id, &uri),
                    );
                }
                // Other unhandled OSC sequences: silently drop. Future
                // events (e.g. OSC 8 hyperlinks) plug in here.
            }
        }
    }

    fn cmd_set_focus(
        &mut self,
        target: FocusTarget,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        if let FocusTarget::Portal(id) = &target
            && !self.state.current().portals.contains_key(id)
        {
            return Err((ERR_UNKNOWN_PORTAL, "id not found"));
        }
        self.state.focus = match target {
            FocusTarget::Host => FocusKind::Host,
            FocusTarget::Portal(id) => FocusKind::Portal(id),
        };
        Ok(Vec::new())
    }

    fn cmd_set_cursor_style(
        &mut self,
        unfocused: CursorStyle,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        self.state.cursor_style = unfocused;
        Ok(Vec::new())
    }

    /// Drive a portal's vt100 scrollback offset. `lines = 0` returns to
    /// the live region; larger values move the visible region back into
    /// the portal's scrollback ring. The vt100 layer clamps requests
    /// larger than the current history depth, so over-large `lines` is
    /// silently capped.
    ///
    /// The Ok body echoes:
    /// ```
    /// u32 applied_lines    ; offset actually in effect after clamping
    /// u32 history_depth    ; rows currently held in the portal's
    ///                      ; scrollback ring (sized by inner program
    ///                      ; output history, not the configured cap)
    /// ```
    /// so a client showing a scrollbar can size the thumb without an
    /// extra round-trip.
    fn cmd_set_portal_scrollback(
        &mut self,
        id: &str,
        lines: u32,
    ) -> Result<Vec<u8>, (u16, &'static str)> {
        let portal = self
            .state
            .current_mut()
            .portals
            .get_mut(id)
            .ok_or((ERR_UNKNOWN_PORTAL, "id not found"))?;
        portal.vt.screen_mut().set_scrollback(lines as usize);
        let applied = portal.vt.screen().scrollback() as u32;
        // Probe the actual history depth: ask vt100 to scroll past the
        // end, read back the clamped value, then restore. Identical
        // technique to `vge::state::measure_history` so we don't need
        // to add a new vt100 accessor.
        portal.vt.screen_mut().set_scrollback(usize::MAX);
        let history_depth = portal.vt.screen().scrollback() as u32;
        portal.vt.screen_mut().set_scrollback(applied as usize);

        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&applied.to_le_bytes());
        body.extend_from_slice(&history_depth.to_le_bytes());
        Ok(body)
    }
}

impl Default for PrtEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---- LineTracker ------------------------------------------------------

/// Tracks `top_of_live_screen` (absolute scrollback line index of
/// vt100's first live-screen row) by probing the parser before/after
/// `parser.process(...)` calls.
///
/// Same algorithm as VGE's tracker (see `veter/src/vge/state.rs`):
/// pre-saturation growth advances the line count by added history rows;
/// at-cap growth detects eviction by hashing vt100's topmost history
/// row and comparing across probes.
struct LineTracker {
    top_of_live_screen: i64,
    prev_history_size: usize,
    history_cap: usize,
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

    fn update<CB: vt100::Callbacks>(&mut self, parser: &mut vt100::Parser<CB>) {
        let (history_size, top_hash) = probe_history(parser);

        if !self.initialized {
            self.prev_history_size = history_size;
            self.history_cap = history_size;
            self.prev_top_hash = top_hash;
            self.initialized = true;
            return;
        }

        if history_size > self.history_cap {
            self.history_cap = history_size;
        }

        if history_size > self.prev_history_size {
            // Pre-saturation: every new history line corresponds to one
            // live-screen scroll, with no eviction.
            let added = history_size - self.prev_history_size;
            self.top_of_live_screen += added as i64;
        } else if history_size == self.prev_history_size
            && self.history_cap > 0
            && history_size == self.history_cap
            && top_hash != self.prev_top_hash
        {
            // At cap, history size doesn't grow but the topmost row
            // changed — at least one eviction. Counting 1 is a known
            // limitation under heavy paste.
            self.top_of_live_screen += 1;
        }

        self.prev_history_size = history_size;
        self.prev_top_hash = top_hash;
    }

    /// Reset to initial state (used by RIS/DECSTR — line tracking is
    /// re-derived after the parser also resets).
    fn clear(&mut self) {
        *self = Self::new();
    }
}

fn probe_history<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
) -> (usize, u64) {
    let saved = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(usize::MAX);
    let history_size = parser.screen().scrollback();
    let mut hasher = DefaultHasher::new();
    if history_size > 0 {
        let cols = parser.screen().size().1;
        for col in 0..cols {
            if let Some(cell) = parser.screen().cell(0, col) {
                let s = cell.contents();
                s.hash(&mut hasher);
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

/// Standard base64 decoder for OSC 52 set form (§8.4).
///
/// Tolerates `=` padding and whitespace; rejects on any other
/// non-alphabet byte. Returns `None` for malformed input — the engine
/// then emits a ClipboardOp with an empty `data` field rather than
/// fabricating bytes.
fn b64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in input {
        let v: u32 = match b {
            b'A'..=b'Z' => u32::from(b - b'A'),
            b'a'..=b'z' => u32::from(b - b'a') + 26,
            b'0'..=b'9' => u32::from(b - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

// ---- tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use prt_protocol::apc::ApcStream;
    use prt_protocol::codec::Reader;
    use prt_protocol::encode;
    use prt_protocol::envelope::wrap_c2t_envelope;

    /// Shape of a parsed response frame: (frame_type, request_id, body).
    struct ParsedFrame {
        frame_type: u8,
        request_id: u32,
        body: Vec<u8>,
    }

    /// Build a c2t envelope carrying one command frame, feed it to the
    /// engine, and parse out the (single) response frame from the t2c
    /// envelope the engine emits.
    fn dispatch_one(
        engine: &mut PrtEngine,
        frame_type: u8,
        request_id: u32,
        body: &[u8],
    ) -> ParsedFrame {
        let mut frames = Vec::new();
        append_frame(&mut frames, frame_type, request_id, body);
        let env = wrap_c2t_envelope(&frames);
        let _passthrough = engine.process_pty_chunk(&env);
        let resp_bytes = engine.take_responses();
        decode_single_response(&resp_bytes)
    }

    fn decode_single_response(resp_bytes: &[u8]) -> ParsedFrame {
        // Use a T2C-marker stream to recover the response payload.
        let mut s = ApcStream::with_marker(*MARKER_T2C);
        let out = s.feed(resp_bytes);
        assert!(out.passthrough.is_empty(), "spurious passthrough bytes");
        assert_eq!(out.payloads.len(), 1, "expected exactly one envelope");
        let payload = &out.payloads[0];

        let mut r = Reader::new(payload);
        let version = r.u8().unwrap();
        assert_eq!(version, PROTOCOL_VERSION);
        let _payload_len = r.u32().unwrap();
        let frame_type = r.u8().unwrap();
        let request_id = r.u32().unwrap();
        let body_len = r.u32().unwrap();
        let body = r.take(body_len as usize).unwrap().to_vec();
        ParsedFrame {
            frame_type,
            request_id,
            body,
        }
    }

    fn err_code(body: &[u8]) -> u16 {
        let mut r = Reader::new(body);
        r.u16().unwrap()
    }

    fn make_create_body(id: &str, w: u32, h: u32) -> Vec<u8> {
        encode::create_portal_body(&CreatePortalBody {
            id: id.to_string(),
            size_w: w,
            size_h: h,
            origin_x: 0,
            origin_y: 0,
            anchor_mode: AnchorMode::Live,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 0,
        })
    }

    #[test]
    fn probe_returns_caps_with_request_id_echoed() {
        let mut engine = PrtEngine::new();
        let parsed = dispatch_one(&mut engine, CMD_PROBE, 0xCAFE_BABE, &[]);
        assert_eq!(parsed.frame_type, RSP_PROBE);
        assert_eq!(parsed.request_id, 0xCAFE_BABE);
        let mut r = Reader::new(&parsed.body);
        assert_eq!(r.u16().unwrap(), 1); // protocol_version
        assert_eq!(r.u32().unwrap(), 64); // max_portals
        assert_eq!(r.u32().unwrap(), 1024); // max_portal_cells_w
        assert_eq!(r.u32().unwrap(), 512); // max_portal_cells_h
        assert_eq!(r.u32().unwrap(), 100_000); // max_scrollback_lines
        assert_eq!(r.u32().unwrap(), 1 << 20); // max_write_bytes
        // features: alt-screen + bell + title + icon + cwd + clipboard + mouse-mode
        let features = r.u8().unwrap();
        assert!(features & FEAT_ALT_SCREEN_IN_PORTAL != 0);
        assert!(features & FEAT_EMIT_BELL_EVENTS != 0);
        assert!(features & FEAT_EMIT_TITLE_EVENTS != 0);
        assert!(features & FEAT_EMIT_ICON_EVENTS != 0);
        assert!(features & FEAT_EMIT_CWD_EVENTS != 0);
        assert!(features & FEAT_EMIT_CLIPBOARD_EVENTS != 0);
        assert!(features & FEAT_EMIT_MOUSE_MODE_EVENTS != 0);
        assert_eq!(r.u8().unwrap(), 8); // max_nesting_depth
        // §10 trailing byte advertises FEAT_VGE_IN_PORTAL.
        assert_eq!(r.u8().unwrap(), FEAT_VGE_IN_PORTAL);
        assert!(r.at_end());
    }

    #[test]
    fn create_portal_inserts_in_main_set() {
        let mut engine = PrtEngine::new();
        let body = make_create_body("p", 80, 24);
        let parsed = dispatch_one(&mut engine, CMD_CREATE_PORTAL, 1, &body);
        assert_eq!(parsed.frame_type, RSP_OK);
        assert_eq!(parsed.request_id, 1);
        assert!(engine.state.current().portals.contains_key("p"));
        let portal = &engine.state.current().portals["p"];
        assert_eq!(portal.size_w, 80);
        assert_eq!(portal.size_h, 24);
        assert_eq!(portal.creation_seq, 0);
        assert!(matches!(portal.anchor, PortalAnchor::Live { origin_y: 0 }));
    }

    #[test]
    fn duplicate_id_errors_atomically() {
        let mut engine = PrtEngine::new();
        let body = make_create_body("p", 10, 10);
        assert_eq!(
            dispatch_one(&mut engine, CMD_CREATE_PORTAL, 1, &body).frame_type,
            RSP_OK
        );

        let parsed = dispatch_one(&mut engine, CMD_CREATE_PORTAL, 2, &body);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_DUPLICATE_ID);
        // Atomic: still exactly one portal.
        assert_eq!(engine.state.current().portals.len(), 1);
    }

    #[test]
    fn create_portal_size_zero_is_size_out_of_range() {
        let mut engine = PrtEngine::new();
        let body = make_create_body("p", 0, 10);
        let parsed = dispatch_one(&mut engine, CMD_CREATE_PORTAL, 1, &body);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_SIZE_OUT_OF_RANGE);
        assert!(engine.state.current().portals.is_empty());
    }

    #[test]
    fn create_portal_size_above_cap_errors() {
        let limits = Limits {
            max_portal_cells_w: 80,
            max_portal_cells_h: 24,
            ..Limits::default()
        };
        let mut engine = PrtEngine::with_limits(limits, 0);
        let body = make_create_body("p", 81, 10);
        let parsed = dispatch_one(&mut engine, CMD_CREATE_PORTAL, 1, &body);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_SIZE_OUT_OF_RANGE);
    }

    #[test]
    fn create_portal_too_many_portals() {
        let limits = Limits {
            max_portals: 2,
            ..Limits::default()
        };
        let mut engine = PrtEngine::with_limits(limits, 0);
        for (i, id) in ["a", "b"].iter().enumerate() {
            let parsed = dispatch_one(
                &mut engine,
                CMD_CREATE_PORTAL,
                i as u32,
                &make_create_body(id, 10, 10),
            );
            assert_eq!(parsed.frame_type, RSP_OK);
        }
        let parsed = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            99,
            &make_create_body("c", 10, 10),
        );
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_TOO_MANY_PORTALS);
        assert_eq!(engine.state.current().portals.len(), 2);
    }

    #[test]
    fn nesting_cap_zero_rejects_top_level_create() {
        // max_nesting_depth=0 means no portals at all — the top-level
        // engine at depth 0 already exceeds the cap.
        let limits = Limits {
            max_nesting_depth: 0,
            ..Limits::default()
        };
        let mut engine = PrtEngine::with_limits(limits, 0);
        let parsed = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_MAX_NESTING_DEPTH);
    }

    #[test]
    fn nesting_cap_one_allows_only_top_level() {
        // depth-0 engine can create top-level portals; the sub-engine
        // they own is at depth 1, which equals the cap and so cannot
        // create further sub-portals.
        let limits = Limits {
            max_nesting_depth: 1,
            ..Limits::default()
        };
        let mut top = PrtEngine::with_limits(limits, 0);
        assert_eq!(
            dispatch_one(&mut top, CMD_CREATE_PORTAL, 1, &make_create_body("p", 10, 10))
                .frame_type,
            RSP_OK
        );
        // Construct a sub-engine the way cmd_create_portal does. We
        // can't drive bytes through it via WritePortal yet (Phase 3),
        // but we can dispatch directly to verify the depth check.
        let mut sub = PrtEngine::with_limits(limits, 1);
        let parsed = dispatch_one(&mut sub, CMD_CREATE_PORTAL, 1, &make_create_body("q", 10, 10));
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_MAX_NESTING_DEPTH);
    }

    #[test]
    fn delete_portal_removes_or_errors() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );

        let body = encode::delete_portal_body("p");
        let parsed = dispatch_one(&mut engine, CMD_DELETE_PORTAL, 2, &body);
        assert_eq!(parsed.frame_type, RSP_OK);
        assert!(engine.state.current().portals.is_empty());

        // Second delete: now unknown.
        let parsed = dispatch_one(&mut engine, CMD_DELETE_PORTAL, 3, &body);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_UNKNOWN_PORTAL);
    }

    #[test]
    fn update_size_resizes_inner_vt100() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 80, 24),
        );

        let body = encode::update_size_body("p", 100, 30);
        let parsed = dispatch_one(&mut engine, CMD_UPDATE_SIZE, 2, &body);
        assert_eq!(parsed.frame_type, RSP_OK);
        let portal = &engine.state.current().portals["p"];
        assert_eq!(portal.size_w, 100);
        assert_eq!(portal.size_h, 30);
        assert_eq!(portal.vt.screen().size(), (30, 100));
    }

    #[test]
    fn update_origin_mode_mismatch_is_bad_payload() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );

        // Portal was created in Live mode; now claim Scrollback — reject.
        let body = encode::update_origin_body(&UpdateOriginBody {
            id: "p".into(),
            new_origin_x: 5,
            new_origin_y: 6,
            anchor_mode: AnchorMode::Scrollback,
        });
        let parsed = dispatch_one(&mut engine, CMD_UPDATE_ORIGIN, 2, &body);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn update_origin_live_overwrites_origin() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );
        let body = encode::update_origin_body(&UpdateOriginBody {
            id: "p".into(),
            new_origin_x: 5,
            new_origin_y: 9,
            anchor_mode: AnchorMode::Live,
        });
        let parsed = dispatch_one(&mut engine, CMD_UPDATE_ORIGIN, 2, &body);
        assert_eq!(parsed.frame_type, RSP_OK);
        let portal = &engine.state.current().portals["p"];
        assert_eq!(portal.origin_x, 5);
        assert!(matches!(portal.anchor, PortalAnchor::Live { origin_y: 9 }));
    }

    #[test]
    fn update_visibility_and_draw_order() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );

        let body = encode::update_visibility_body("p", false);
        assert_eq!(
            dispatch_one(&mut engine, CMD_UPDATE_VISIBILITY, 2, &body).frame_type,
            RSP_OK
        );
        assert!(!engine.state.current().portals["p"].is_visible);

        let body = encode::update_draw_order_body("p", -42);
        assert_eq!(
            dispatch_one(&mut engine, CMD_UPDATE_DRAW_ORDER, 3, &body).frame_type,
            RSP_OK
        );
        assert_eq!(engine.state.current().portals["p"].draw_order, -42);
    }

    #[test]
    fn clear_all_wipes_current_set() {
        let mut engine = PrtEngine::new();
        for id in ["a", "b", "c"] {
            let _ = dispatch_one(
                &mut engine,
                CMD_CREATE_PORTAL,
                0,
                &make_create_body(id, 10, 10),
            );
        }
        assert_eq!(engine.state.current().portals.len(), 3);
        let parsed = dispatch_one(&mut engine, CMD_CLEAR_ALL, 9, &[]);
        assert_eq!(parsed.frame_type, RSP_OK);
        assert!(engine.state.current().portals.is_empty());
    }

    #[test]
    fn set_focus_unknown_portal_errors() {
        let mut engine = PrtEngine::new();
        let body = encode::set_focus_body(&FocusTarget::Portal("nope".into()));
        let parsed = dispatch_one(&mut engine, CMD_SET_FOCUS, 1, &body);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_UNKNOWN_PORTAL);
        assert!(matches!(engine.state.focus, FocusKind::Host));
    }

    #[test]
    fn set_focus_host_then_portal() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );

        let body = encode::set_focus_body(&FocusTarget::Portal("p".into()));
        assert_eq!(
            dispatch_one(&mut engine, CMD_SET_FOCUS, 2, &body).frame_type,
            RSP_OK
        );
        assert_eq!(engine.state.focus, FocusKind::Portal("p".into()));

        let body = encode::set_focus_body(&FocusTarget::Host);
        assert_eq!(
            dispatch_one(&mut engine, CMD_SET_FOCUS, 3, &body).frame_type,
            RSP_OK
        );
        assert_eq!(engine.state.focus, FocusKind::Host);
    }

    #[test]
    fn set_cursor_style_persists() {
        let mut engine = PrtEngine::new();
        let body = encode::set_cursor_style_body(CursorStyle::Dim);
        assert_eq!(
            dispatch_one(&mut engine, CMD_SET_CURSOR_STYLE, 1, &body).frame_type,
            RSP_OK
        );
        assert_eq!(engine.state.cursor_style, CursorStyle::Dim);
    }

    #[test]
    fn write_portal_unknown_id_errors_atomically() {
        let mut engine = PrtEngine::new();
        let body = encode::write_portal_body(&WritePortalBody {
            id: "nope".into(),
            data: b"hello".to_vec(),
        });
        let parsed = dispatch_one(&mut engine, CMD_WRITE_PORTAL, 1, &body);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_UNKNOWN_PORTAL);
    }

    #[test]
    fn write_portal_too_large_errors() {
        let limits = Limits {
            max_write_bytes: 4,
            ..Limits::default()
        };
        let mut engine = PrtEngine::with_limits(limits, 0);
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: vec![0u8; 5],
        });
        let parsed = dispatch_one(&mut engine, CMD_WRITE_PORTAL, 2, &body);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_WRITE_TOO_LARGE);
    }

    #[test]
    fn unknown_command_errors() {
        let mut engine = PrtEngine::new();
        // Unallocated reserved frame type in the command range.
        let parsed = dispatch_one(&mut engine, 0x7E, 1, &[]);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(err_code(&parsed.body), ERR_UNKNOWN_COMMAND);
    }

    #[test]
    fn future_protocol_version_errors_with_unsupported_version() {
        // Build an envelope by hand with version 2.
        let mut frames = Vec::new();
        append_frame(&mut frames, CMD_PROBE, 1, &[]);
        let mut unstuffed = Vec::new();
        unstuffed.push(2u8); // future version
        unstuffed.extend_from_slice(&(frames.len() as u32).to_le_bytes());
        unstuffed.extend_from_slice(&frames);
        let mut env = vec![ESC, APC_OPEN];
        env.extend_from_slice(MARKER_C2T);
        prt_protocol::codec::stuff(&unstuffed, &mut env);
        env.push(ESC);
        env.push(ST_CLOSE);

        let mut engine = PrtEngine::new();
        let _ = engine.process_pty_chunk(&env);
        let resp = engine.take_responses();
        let parsed = decode_single_response(&resp);
        assert_eq!(parsed.frame_type, RSP_ERR);
        assert_eq!(parsed.request_id, 0); // we couldn't read it
        assert_eq!(err_code(&parsed.body), ERR_UNSUPPORTED_VERSION);
    }

    #[test]
    fn batched_commands_in_one_envelope_produce_batched_responses() {
        let mut engine = PrtEngine::new();
        let create = make_create_body("p", 10, 10);
        let vis = encode::update_visibility_body("p", false);
        let order = encode::update_draw_order_body("p", 5);

        let mut frames = Vec::new();
        append_frame(&mut frames, CMD_CREATE_PORTAL, 100, &create);
        append_frame(&mut frames, CMD_UPDATE_VISIBILITY, 200, &vis);
        append_frame(&mut frames, CMD_UPDATE_DRAW_ORDER, 300, &order);
        let env = wrap_c2t_envelope(&frames);

        let _ = engine.process_pty_chunk(&env);
        let resp = engine.take_responses();

        let mut s = ApcStream::with_marker(*MARKER_T2C);
        let out = s.feed(&resp);
        assert_eq!(out.payloads.len(), 1, "one envelope back");
        let payload = &out.payloads[0];

        let mut r = Reader::new(payload);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _payload_len = r.u32().unwrap();
        let mut request_ids = Vec::new();
        while !r.at_end() {
            let _ft = r.u8().unwrap();
            let rid = r.u32().unwrap();
            let body_len = r.u32().unwrap() as usize;
            r.take(body_len).unwrap();
            request_ids.push(rid);
        }
        assert_eq!(request_ids, vec![100, 200, 300]);
    }

    // ---- Phase 3: WritePortal pipeline ------------------------------

    /// Decode every frame across every t2c envelope in `resp_bytes`,
    /// preserving order. Used by tests that expect a response frame
    /// followed by zero-or-more event frames.
    fn decode_all_response_frames(resp_bytes: &[u8]) -> Vec<ParsedFrame> {
        let mut s = ApcStream::with_marker(*MARKER_T2C);
        let out = s.feed(resp_bytes);
        let mut all = Vec::new();
        for payload in &out.payloads {
            let mut r = Reader::new(payload);
            assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
            let _payload_len = r.u32().unwrap();
            while !r.at_end() {
                let frame_type = r.u8().unwrap();
                let request_id = r.u32().unwrap();
                let body_len = r.u32().unwrap() as usize;
                let body = r.take(body_len).unwrap().to_vec();
                all.push(ParsedFrame {
                    frame_type,
                    request_id,
                    body,
                });
            }
        }
        all
    }

    fn dispatch_full(
        engine: &mut PrtEngine,
        frame_type: u8,
        request_id: u32,
        body: &[u8],
    ) -> Vec<ParsedFrame> {
        let mut frames = Vec::new();
        append_frame(&mut frames, frame_type, request_id, body);
        let env = wrap_c2t_envelope(&frames);
        let _ = engine.process_pty_chunk(&env);
        let resp = engine.take_responses();
        decode_all_response_frames(&resp)
    }

    /// Convenience: create a portal, then write `data` to it, return
    /// every frame produced in response to the WritePortal.
    fn create_and_write(
        engine: &mut PrtEngine,
        id: &str,
        size_w: u32,
        size_h: u32,
        data: &[u8],
    ) -> Vec<ParsedFrame> {
        let create = encode::create_portal_body(&CreatePortalBody {
            id: id.to_string(),
            size_w,
            size_h,
            origin_x: 0,
            origin_y: 0,
            anchor_mode: AnchorMode::Live,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 0,
        });
        assert_eq!(
            dispatch_one(engine, CMD_CREATE_PORTAL, 1, &create).frame_type,
            RSP_OK
        );
        let body = encode::write_portal_body(&WritePortalBody {
            id: id.to_string(),
            data: data.to_vec(),
        });
        dispatch_full(engine, CMD_WRITE_PORTAL, 2, &body)
    }

    fn first_event(frames: &[ParsedFrame], code: u8) -> Option<&ParsedFrame> {
        frames.iter().find(|f| f.frame_type == code)
    }

    fn event_count(frames: &[ParsedFrame], code: u8) -> usize {
        frames.iter().filter(|f| f.frame_type == code).count()
    }

    #[test]
    fn write_portal_feeds_bytes_to_inner_vt100() {
        let mut engine = PrtEngine::new();
        let frames = create_and_write(&mut engine, "p", 20, 4, b"hello");

        // Response then no events for plain text.
        assert_eq!(frames[0].frame_type, RSP_OK);
        assert_eq!(frames.len(), 1, "no events for plain text");

        let portal = &engine.state.current().portals["p"];
        let cell = portal.vt.screen().cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "h");
        let cell = portal.vt.screen().cell(0, 4).unwrap();
        assert_eq!(cell.contents(), "o");
    }

    #[test]
    fn bell_byte_emits_bell_event() {
        let mut engine = PrtEngine::new();
        let frames = create_and_write(&mut engine, "p", 10, 2, b"\x07");
        assert_eq!(event_count(&frames, EVT_BELL), 1);
        let bell = first_event(&frames, EVT_BELL).unwrap();
        let mut r = Reader::new(&bell.body);
        assert_eq!(r.string().unwrap(), "p");
    }

    #[test]
    fn osc_0_fires_both_title_and_icon_name() {
        let mut engine = PrtEngine::new();
        let frames = create_and_write(&mut engine, "p", 10, 2, b"\x1b]0;hello world\x07");
        assert_eq!(event_count(&frames, EVT_TITLE_CHANGE), 1);
        assert_eq!(event_count(&frames, EVT_ICON_NAME_CHANGE), 1);
        let title = first_event(&frames, EVT_TITLE_CHANGE).unwrap();
        let mut r = Reader::new(&title.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.string().unwrap(), "hello world");
    }

    #[test]
    fn osc_1_fires_only_icon_name() {
        let mut engine = PrtEngine::new();
        let frames = create_and_write(&mut engine, "p", 10, 2, b"\x1b]1;icon\x07");
        assert_eq!(event_count(&frames, EVT_TITLE_CHANGE), 0);
        assert_eq!(event_count(&frames, EVT_ICON_NAME_CHANGE), 1);
    }

    #[test]
    fn osc_2_fires_only_title() {
        let mut engine = PrtEngine::new();
        let frames = create_and_write(&mut engine, "p", 10, 2, b"\x1b]2;windowtitle\x07");
        assert_eq!(event_count(&frames, EVT_TITLE_CHANGE), 1);
        assert_eq!(event_count(&frames, EVT_ICON_NAME_CHANGE), 0);
        let t = first_event(&frames, EVT_TITLE_CHANGE).unwrap();
        let mut r = Reader::new(&t.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.string().unwrap(), "windowtitle");
    }

    #[test]
    fn osc_7_fires_working_dir_change() {
        let mut engine = PrtEngine::new();
        let frames = create_and_write(
            &mut engine,
            "p",
            10,
            2,
            b"\x1b]7;file://host/home/user\x07",
        );
        assert_eq!(event_count(&frames, EVT_WORKING_DIR_CHANGE), 1);
        let cwd = first_event(&frames, EVT_WORKING_DIR_CHANGE).unwrap();
        let mut r = Reader::new(&cwd.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.string().unwrap(), "file://host/home/user");
    }

    #[test]
    fn osc_52_set_decodes_base64_to_clipboard_op() {
        // `aGVsbG8=` is base64 for "hello".
        let mut engine = PrtEngine::new();
        let frames = create_and_write(
            &mut engine,
            "p",
            10,
            2,
            b"\x1b]52;c;aGVsbG8=\x07",
        );
        assert_eq!(event_count(&frames, EVT_CLIPBOARD_OP), 1);
        let ev = first_event(&frames, EVT_CLIPBOARD_OP).unwrap();
        let mut r = Reader::new(&ev.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u8().unwrap(), b'c');
        assert_eq!(r.u8().unwrap(), CLIPBOARD_SET);
        assert_eq!(r.bytes().unwrap(), b"hello");
    }

    #[test]
    fn osc_52_query_emits_clipboard_op_with_empty_data() {
        let mut engine = PrtEngine::new();
        let frames = create_and_write(&mut engine, "p", 10, 2, b"\x1b]52;c;?\x07");
        let ev = first_event(&frames, EVT_CLIPBOARD_OP).unwrap();
        let mut r = Reader::new(&ev.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u8().unwrap(), b'c');
        assert_eq!(r.u8().unwrap(), CLIPBOARD_QUERY);
        assert_eq!(r.bytes().unwrap(), b"");
    }

    #[test]
    fn dectcem_toggle_emits_cursor_visibility_change() {
        let mut engine = PrtEngine::new();
        // Default is visible; hide → visible=0.
        let frames = create_and_write(&mut engine, "p", 10, 2, b"\x1b[?25l");
        let ev = first_event(&frames, EVT_CURSOR_VISIBILITY_CHANGE).unwrap();
        let mut r = Reader::new(&ev.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u8().unwrap(), 0);

        // Show again → visible=1.
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: b"\x1b[?25h".to_vec(),
        });
        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 5, &body);
        let ev = first_event(&frames, EVT_CURSOR_VISIBILITY_CHANGE).unwrap();
        let mut r = Reader::new(&ev.body);
        let _ = r.string().unwrap();
        assert_eq!(r.u8().unwrap(), 1);
    }

    #[test]
    fn alt_screen_toggle_emits_buffer_mode_change() {
        let mut engine = PrtEngine::new();
        let frames = create_and_write(&mut engine, "p", 10, 4, b"\x1b[?1049h");
        let ev = first_event(&frames, EVT_BUFFER_MODE_CHANGE).unwrap();
        let mut r = Reader::new(&ev.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u8().unwrap(), 1);
    }

    #[test]
    fn mouse_mode_change_emits_event() {
        let mut engine = PrtEngine::new();
        // DECSET 1000 (PressRelease) + 1006 (SGR encoding) in one stream.
        let frames =
            create_and_write(&mut engine, "p", 10, 2, b"\x1b[?1000h\x1b[?1006h");
        // Coalesced: one event reflecting end state.
        assert_eq!(event_count(&frames, EVT_MOUSE_MODE_CHANGE), 1);
        let ev = first_event(&frames, EVT_MOUSE_MODE_CHANGE).unwrap();
        let mut r = Reader::new(&ev.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u8().unwrap(), 2); // protocol = normal/PressRelease
        assert_eq!(r.u8().unwrap(), 2); // encoding = SGR
        assert_eq!(r.u8().unwrap(), 0); // focus_events: vt100 doesn't track
    }

    #[test]
    fn dsr_cursor_query_yields_raw_reply() {
        let mut engine = PrtEngine::new();
        // "abc" advances cursor to col 3 (0-indexed); DSR returns
        // 1-indexed `\x1b[1;4R`.
        let frames = create_and_write(&mut engine, "p", 10, 2, b"abc\x1b[6n");
        let ev = first_event(&frames, EVT_RAW_REPLY).unwrap();
        let mut r = Reader::new(&ev.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.bytes().unwrap(), b"\x1b[1;4R");
    }

    #[test]
    fn nested_write_portal_response_surfaces_as_raw_reply() {
        let mut engine = PrtEngine::new();
        let create = encode::create_portal_body(&CreatePortalBody {
            id: "outer".into(),
            size_w: 80,
            size_h: 24,
            origin_x: 0,
            origin_y: 0,
            anchor_mode: AnchorMode::Live,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 0,
        });
        assert_eq!(
            dispatch_one(&mut engine, CMD_CREATE_PORTAL, 1, &create).frame_type,
            RSP_OK
        );

        // Build an inner CreatePortal envelope; pack it into the
        // WritePortal data so that the engine routes it through
        // outer.children.
        let inner_body = encode::create_portal_body(&CreatePortalBody {
            id: "inner".into(),
            size_w: 40,
            size_h: 12,
            origin_x: 0,
            origin_y: 0,
            anchor_mode: AnchorMode::Live,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 0,
        });
        let mut inner_frames = Vec::new();
        append_frame(&mut inner_frames, CMD_CREATE_PORTAL, 42, &inner_body);
        let inner_envelope = wrap_c2t_envelope(&inner_frames);

        let body = encode::write_portal_body(&WritePortalBody {
            id: "outer".into(),
            data: inner_envelope,
        });
        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 2, &body);

        // Outer Ok + RawReply containing the inner Ok response envelope.
        assert_eq!(frames[0].frame_type, RSP_OK);
        let raw = first_event(&frames, EVT_RAW_REPLY).unwrap();
        let mut r = Reader::new(&raw.body);
        assert_eq!(r.string().unwrap(), "outer");
        let inner_t2c = r.bytes().unwrap();

        // Decode the embedded inner-engine response envelope.
        let mut s = ApcStream::with_marker(*MARKER_T2C);
        let out = s.feed(inner_t2c);
        assert_eq!(out.payloads.len(), 1);
        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _len = r.u32().unwrap();
        let inner_ft = r.u8().unwrap();
        let inner_rid = r.u32().unwrap();
        assert_eq!(inner_ft, RSP_OK);
        assert_eq!(inner_rid, 42);

        // Sub-portal must live in outer.children.
        let outer = &engine.state.current().portals["outer"];
        assert!(outer.children.state.current().portals.contains_key("inner"));
    }

    #[test]
    fn idempotent_state_does_not_emit_spurious_events() {
        let mut engine = PrtEngine::new();
        // First write hides the cursor → 1 event.
        let frames = create_and_write(&mut engine, "p", 10, 2, b"\x1b[?25l");
        assert_eq!(event_count(&frames, EVT_CURSOR_VISIBILITY_CHANGE), 1);

        // Second write: hide again. Same end state → no event.
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: b"\x1b[?25l".to_vec(),
        });
        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 5, &body);
        assert_eq!(event_count(&frames, EVT_CURSOR_VISIBILITY_CHANGE), 0);
    }

    #[test]
    fn write_portal_failure_consumes_no_bytes() {
        // unknown_portal — atomically rejects, no callback events fire.
        let mut engine = PrtEngine::new();
        let body = encode::write_portal_body(&WritePortalBody {
            id: "ghost".into(),
            data: b"\x1b]2;hi\x07".to_vec(),
        });
        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 1, &body);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_type, RSP_ERR);
        assert_eq!(err_code(&frames[0].body), ERR_UNKNOWN_PORTAL);
    }

    // ---- Phase 4: line tracking, eviction, host-screen lifecycle ----

    fn make_create_body_full(
        id: &str,
        w: u32,
        h: u32,
        anchor: AnchorMode,
        origin_y: i32,
    ) -> Vec<u8> {
        encode::create_portal_body(&CreatePortalBody {
            id: id.to_string(),
            size_w: w,
            size_h: h,
            origin_x: 0,
            origin_y,
            anchor_mode: anchor,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 0,
        })
    }

    #[test]
    fn update_size_emits_resize_notify() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 80, 24),
        );
        let body = encode::update_size_body("p", 100, 30);
        let frames = dispatch_full(&mut engine, CMD_UPDATE_SIZE, 2, &body);

        assert_eq!(frames[0].frame_type, RSP_OK);
        assert_eq!(event_count(&frames, EVT_RESIZE_NOTIFY), 1);
        let ev = first_event(&frames, EVT_RESIZE_NOTIFY).unwrap();
        let mut r = Reader::new(&ev.body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u32().unwrap(), 30); // rows
        assert_eq!(r.u32().unwrap(), 100); // cols
    }

    #[test]
    fn update_size_no_op_emits_no_event() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 80, 24),
        );
        let body = encode::update_size_body("p", 80, 24);
        let frames = dispatch_full(&mut engine, CMD_UPDATE_SIZE, 2, &body);
        assert_eq!(event_count(&frames, EVT_RESIZE_NOTIFY), 0);
    }

    #[test]
    fn host_ris_clears_portals_and_emits_eviction() {
        let mut engine = PrtEngine::new();
        for id in ["a", "b", "c"] {
            let _ = dispatch_one(
                &mut engine,
                CMD_CREATE_PORTAL,
                0,
                &make_create_body(id, 10, 10),
            );
        }
        // Feed RIS through the top-level engine. process_pty_chunk
        // does NOT auto-react in v1 — the parent loop calls
        // handle_terminal_events on the events itself.
        let chunk = engine.process_pty_chunk_full(b"\x1bc");
        assert!(chunk.terminal_events.contains(&TerminalEvent::HardReset));
        engine.handle_terminal_events(&chunk.terminal_events);
        engine.flush_pending_events();

        assert!(engine.state.current().portals.is_empty());
        let resp = engine.take_responses();
        let frames = decode_all_response_frames(&resp);
        assert_eq!(event_count(&frames, EVT_PORTAL_EVICTED), 3);
        for f in &frames {
            if f.frame_type == EVT_PORTAL_EVICTED {
                let mut r = Reader::new(&f.body);
                let _ = r.string().unwrap();
                assert_eq!(r.u8().unwrap(), EVICT_ERASE);
            }
        }
    }

    #[test]
    fn host_decstr_also_resets_scope() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            0,
            &make_create_body("p", 10, 10),
        );
        let chunk = engine.process_pty_chunk_full(b"\x1b[!p");
        assert!(chunk.terminal_events.contains(&TerminalEvent::SoftReset));
        engine.handle_terminal_events(&chunk.terminal_events);
        assert!(engine.state.current().portals.is_empty());
    }

    #[test]
    fn host_2j_drops_live_and_live_region_scrollback_portals() {
        let mut engine = PrtEngine::new();
        // Live portal — always in the live region by definition.
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            0,
            &make_create_body_full("live", 10, 10, AnchorMode::Live, 5),
        );
        // Scrollback portal anchored at line 0 (== top_of_live_screen
        // == 0 in this fresh engine), so it lies in the live region.
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            0,
            &make_create_body_full("sb_live", 10, 10, AnchorMode::Scrollback, 0),
        );
        let chunk = engine.process_pty_chunk_full(b"\x1b[2J");
        engine.handle_terminal_events(&chunk.terminal_events);
        engine.flush_pending_events();

        assert!(engine.state.current().portals.is_empty());
        let resp = engine.take_responses();
        let frames = decode_all_response_frames(&resp);
        assert_eq!(event_count(&frames, EVT_PORTAL_EVICTED), 2);
    }

    #[test]
    fn host_3j_drops_scrollback_region_portals_only() {
        let mut engine = PrtEngine::new();
        // Move top_of_live_screen forward so we have a "scrollback
        // region" to anchor a portal in. The line tracker's first
        // call only primes — we need a baseline-then-delta sequence.
        let mut parser = vt100::Parser::new(24, 80, 1000);
        engine.after_vt100_process(&mut parser);
        parser.process(&b"\n".repeat(30));
        engine.after_vt100_process(&mut parser);
        let top = engine.top_of_live_screen();
        assert!(top > 0, "top_of_live_screen should have advanced");

        // Scrollback portal at line 0 is below top → in scrollback.
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            0,
            &make_create_body_full("old", 10, 10, AnchorMode::Scrollback, -(top as i32)),
        );
        // Live portal is unaffected by 3J.
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            0,
            &make_create_body_full("live", 10, 10, AnchorMode::Live, 0),
        );

        let chunk = engine.process_pty_chunk_full(b"\x1b[3J");
        engine.handle_terminal_events(&chunk.terminal_events);

        assert!(!engine.state.current().portals.contains_key("old"));
        assert!(engine.state.current().portals.contains_key("live"));
    }

    #[test]
    fn host_alt_swap_evicts_alt_set_on_return() {
        let mut engine = PrtEngine::new();
        let mut parser = vt100::Parser::new(24, 80, 1000);

        // Start on main; create one main portal.
        engine.after_vt100_process(&mut parser);
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            0,
            &make_create_body("main_p", 10, 10),
        );

        // Enter alt screen.
        parser.process(b"\x1b[?1049h");
        engine.after_vt100_process(&mut parser);
        engine.flush_pending_events();
        assert!(engine.state.on_alt());
        // Main portal is suspended, not evicted: no events on entry.
        let resp = engine.take_responses();
        assert!(resp.is_empty(), "no events on alt entry");

        // Create a portal on the alt screen.
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            0,
            &make_create_body("alt_p", 10, 10),
        );

        // Leave alt; alt portal should be evicted reason=2.
        parser.process(b"\x1b[?1049l");
        engine.after_vt100_process(&mut parser);
        engine.flush_pending_events();
        assert!(!engine.state.on_alt());
        // Main portal restored.
        assert!(engine.state.current().portals.contains_key("main_p"));

        let resp = engine.take_responses();
        let frames = decode_all_response_frames(&resp);
        let evicted: Vec<_> = frames
            .iter()
            .filter(|f| f.frame_type == EVT_PORTAL_EVICTED)
            .collect();
        assert_eq!(evicted.len(), 1);
        let mut r = Reader::new(&evicted[0].body);
        assert_eq!(r.string().unwrap(), "alt_p");
        assert_eq!(r.u8().unwrap(), EVICT_ALT_SWAP);
    }

    #[test]
    fn scrollback_portal_evicted_when_anchor_falls_off_history() {
        // Tiny scrollback ring so we can run past it.
        let mut engine = PrtEngine::new();
        let mut parser = vt100::Parser::new(2, 10, 5);

        // Prime the line tracker.
        engine.after_vt100_process(&mut parser);
        // Create a Scrollback portal at line 0 (current top == 0).
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            0,
            &make_create_body_full("pin", 5, 5, AnchorMode::Scrollback, 0),
        );

        // Step the line tracker once per scroll so the at-cap eviction
        // counter advances correctly (the tracker only registers ≥1
        // eviction per update call, by design — see comment on
        // `LineTracker::update`). Each row needs unique content so the
        // saturation detector's row-hash sees a change.
        for i in 0..20u8 {
            parser.process(format!("r{i}\r\n").as_bytes());
            engine.after_vt100_process(&mut parser);
        }
        engine.flush_pending_events();

        assert!(!engine.state.current().portals.contains_key("pin"));
        let resp = engine.take_responses();
        let frames = decode_all_response_frames(&resp);
        let ev = first_event(&frames, EVT_PORTAL_EVICTED).unwrap();
        let mut r = Reader::new(&ev.body);
        assert_eq!(r.string().unwrap(), "pin");
        assert_eq!(r.u8().unwrap(), EVICT_SCROLLBACK);
    }

    #[test]
    fn inside_portal_ris_wipes_sub_portals_and_emits_in_raw_reply() {
        // Outer portal + nested CreatePortal inside it.
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("outer", 80, 24),
        );

        let inner_body = make_create_body("inner", 40, 12);
        let mut inner_frames = Vec::new();
        append_frame(&mut inner_frames, CMD_CREATE_PORTAL, 7, &inner_body);
        let inner_envelope = wrap_c2t_envelope(&inner_frames);
        let body = encode::write_portal_body(&WritePortalBody {
            id: "outer".into(),
            data: inner_envelope,
        });
        let _ = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 2, &body);

        let outer = &engine.state.current().portals["outer"];
        assert!(outer.children.state.current().portals.contains_key("inner"));

        // Now feed RIS into the outer portal — sub-portals should die.
        let body = encode::write_portal_body(&WritePortalBody {
            id: "outer".into(),
            data: b"\x1bc".to_vec(),
        });
        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 3, &body);

        // Outer's children scope is wiped.
        let outer = &engine.state.current().portals["outer"];
        assert!(outer.children.state.current().portals.is_empty());

        // Outer Ok + RawReply containing the inner-engine's t2c
        // envelope with the PortalEvicted event for "inner".
        assert_eq!(frames[0].frame_type, RSP_OK);
        let raw = first_event(&frames, EVT_RAW_REPLY).unwrap();
        let mut r = Reader::new(&raw.body);
        assert_eq!(r.string().unwrap(), "outer");
        let inner_t2c = r.bytes().unwrap();

        let mut s = ApcStream::with_marker(*MARKER_T2C);
        let out = s.feed(inner_t2c);
        assert!(!out.payloads.is_empty());
        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _ = r.u32().unwrap();
        let inner_ft = r.u8().unwrap();
        assert_eq!(inner_ft, EVT_PORTAL_EVICTED);
        let _ = r.u32().unwrap(); // request_id (0 for events)
        let body_len = r.u32().unwrap() as usize;
        let inner_body = r.take(body_len).unwrap();
        let mut br = Reader::new(inner_body);
        assert_eq!(br.string().unwrap(), "inner");
        assert_eq!(br.u8().unwrap(), EVICT_ERASE);
    }

    #[test]
    fn line_tracker_advances_with_history() {
        let mut engine = PrtEngine::new();
        let mut parser = vt100::Parser::new(2, 10, 100);
        engine.after_vt100_process(&mut parser);
        assert_eq!(engine.top_of_live_screen(), 0);
        // 5 newlines on a 2-row screen push 4 lines into history (the
        // last LF advances the cursor without yet scrolling the next).
        // The line tracker advances by exactly that many.
        parser.process(&b"\n".repeat(5));
        engine.after_vt100_process(&mut parser);
        let top = engine.top_of_live_screen();
        assert!(top > 0 && top <= 5, "expected 1..=5 advance, got {top}");
    }

    // ---- Phase 7: focus chain resolution -----------------------------

    #[test]
    fn focus_chain_empty_when_host_focused() {
        let engine = PrtEngine::new();
        assert!(engine.state.focus_chain().is_empty());
    }

    #[test]
    fn focus_chain_with_top_level_portal_focus() {
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );
        let body = encode::set_focus_body(&FocusTarget::Portal("p".into()));
        assert_eq!(
            dispatch_one(&mut engine, CMD_SET_FOCUS, 2, &body).frame_type,
            RSP_OK
        );
        let chain = engine.state.focus_chain();
        assert_eq!(chain, vec!["p"]);
    }

    #[test]
    fn focus_chain_descends_through_nested_focus() {
        // Build host → outer portal "A", nested portal "X" inside A.
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("A", 80, 24),
        );

        // Create "X" inside A via nested WritePortal.
        let inner_create = make_create_body("X", 40, 12);
        let mut inner_frames = Vec::new();
        append_frame(&mut inner_frames, CMD_CREATE_PORTAL, 7, &inner_create);
        let inner_envelope = wrap_c2t_envelope(&inner_frames);
        let body = encode::write_portal_body(&WritePortalBody {
            id: "A".into(),
            data: inner_envelope,
        });
        assert_eq!(
            dispatch_full(&mut engine, CMD_WRITE_PORTAL, 2, &body)[0].frame_type,
            RSP_OK
        );

        // Set host focus to A. From here `focus_chain` should descend
        // into A's children (whose own focus is still Host) and stop.
        let body = encode::set_focus_body(&FocusTarget::Portal("A".into()));
        assert_eq!(
            dispatch_one(&mut engine, CMD_SET_FOCUS, 3, &body).frame_type,
            RSP_OK
        );
        assert_eq!(engine.state.focus_chain(), vec!["A"]);

        // Now route X focus through A's scope as well: A.children focus
        // = Portal("X"). We can't easily SetFocus through the wire
        // without driving M3-side bytes, so poke the inner state
        // directly — focus_chain just walks scopes.
        engine
            .state
            .current_mut()
            .portals
            .get_mut("A")
            .unwrap()
            .children
            .state
            .focus = FocusKind::Portal("X".into());
        assert_eq!(engine.state.focus_chain(), vec!["A", "X"]);
    }

    #[test]
    fn focus_chain_stops_at_dangling_portal_id() {
        let mut engine = PrtEngine::new();
        // Set focus to a portal that doesn't exist by hand-crafting
        // PrtState. (SetFocus would reject this with err_unknown_portal,
        // but the chain walker must still terminate gracefully.)
        engine.state.focus = FocusKind::Portal("ghost".into());
        assert_eq!(engine.state.focus_chain(), vec!["ghost"]);
    }

    // ---- Phase 10 (vge_in_portal): per-portal VGE plumbing -----------

    #[test]
    fn per_portal_vge_probe_surfaces_as_raw_reply() {
        // A program inside a portal sends an ESC_VGE Probe envelope.
        // The host's per-portal VGE engine processes it and the
        // ProbeResponse comes back as part of the portal's RawReply.
        use vge_protocol::frame::{
            CMD_PROBE as VGE_CMD_PROBE, MARKER_T2C as VGE_MARKER_T2C,
            RSP_PROBE as VGE_RSP_PROBE,
        };

        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 80, 24),
        );

        // Build an ESC_VGE c2t envelope carrying a Probe frame, embed
        // it as the WritePortal payload.
        let mut vge_frames = Vec::new();
        vge_protocol::envelope::append_frame(&mut vge_frames, VGE_CMD_PROBE, 99, &[]);
        let vge_envelope = vge_protocol::envelope::wrap_c2t_envelope(&vge_frames);
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: vge_envelope,
        });

        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 2, &body);

        // PRT WritePortal Ok + a RawReply event carrying the VGE
        // ProbeResponse envelope.
        assert_eq!(frames[0].frame_type, RSP_OK);
        let raw = first_event(&frames, EVT_RAW_REPLY)
            .expect("expected RawReply containing the VGE response");
        let mut r = Reader::new(&raw.body);
        assert_eq!(r.string().unwrap(), "p");
        let inner_t2c = r.bytes().unwrap();

        // Decode the inner VGE response envelope (lowercase `vge`
        // marker for host-to-client).
        let mut s = vge_protocol::apc::ApcStream::with_marker(*VGE_MARKER_T2C);
        let out = s.feed(inner_t2c);
        assert_eq!(out.payloads.len(), 1, "expected one VGE response envelope");
        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        let _version = r.u8().unwrap();
        let _payload_len = r.u32().unwrap();
        let frame_type = r.u8().unwrap();
        let request_id = r.u32().unwrap();
        assert_eq!(frame_type, VGE_RSP_PROBE);
        assert_eq!(request_id, 99);
    }

    #[test]
    fn per_portal_vge_does_not_double_reply_dsr() {
        // Inner program sends `\x1b[6n` which both the PRT apc and the
        // VGE apc observe. PRT must be the sole responder — VGE has
        // its auto_reply_dsr disabled at portal-creation time.
        // Expect exactly ONE `\x1b[<r>;<c>R` reply in the RawReply.
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 2),
        );
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: b"abc\x1b[6n".to_vec(),
        });
        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 2, &body);

        let raw = first_event(&frames, EVT_RAW_REPLY).unwrap();
        let mut r = Reader::new(&raw.body);
        assert_eq!(r.string().unwrap(), "p");
        let data = r.bytes().unwrap();
        // Exactly one DSR reply, not two.
        assert_eq!(data, b"\x1b[1;4R");
    }

    // ---- §10 (vft-in-portal): per-portal VFT plumbing ----------------

    #[test]
    fn per_portal_vft_probe_surfaces_as_raw_reply() {
        // A program inside a portal sends an ESC_VFT Probe envelope.
        // The host's per-portal VFT engine processes it and the
        // ProbeResponse comes back as part of the portal's RawReply.
        use vft_protocol::frame::{
            CMD_PROBE as VFT_CMD_PROBE, MARKER_H2C as VFT_MARKER_H2C,
            RSP_PROBE as VFT_RSP_PROBE,
        };

        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 80, 24),
        );

        let mut vft_frames = Vec::new();
        vft_protocol::envelope::append_frame(&mut vft_frames, VFT_CMD_PROBE, 77, &[]);
        let vft_envelope = vft_protocol::envelope::wrap_c2h_envelope(&vft_frames);
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: vft_envelope,
        });

        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 2, &body);

        assert_eq!(frames[0].frame_type, RSP_OK);
        let raw = first_event(&frames, EVT_RAW_REPLY)
            .expect("expected RawReply containing the VFT response");
        let mut r = Reader::new(&raw.body);
        assert_eq!(r.string().unwrap(), "p");
        let inner_h2c = r.bytes().unwrap();

        let mut s = vft_protocol::apc::ApcStream::with_marker(*VFT_MARKER_H2C);
        let out = s.feed(inner_h2c);
        assert_eq!(
            out.payloads.len(),
            1,
            "expected one VFT response envelope"
        );
        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        let _version = r.u8().unwrap();
        let _payload_len = r.u32().unwrap();
        let frame_type = r.u8().unwrap();
        let request_id = r.u32().unwrap();
        assert_eq!(frame_type, VFT_RSP_PROBE);
        assert_eq!(request_id, 77);
    }

    #[test]
    fn inside_portal_ris_aborts_per_portal_vft_transfers() {
        // Bring up a per-portal VFT upload, then send `ESC c` inside
        // the portal. The portal's VFT engine should abort the
        // transfer and emit a TransferAborted event in the portal's
        // RawReply stream.
        use vft_protocol::command::{BeginUploadBody, Command as VftCommand};
        use vft_protocol::encode::build_envelope as vft_build_envelope;
        use vft_protocol::frame::{ABORT_HOST_RESET, EVT_TRANSFER_ABORTED, MARKER_H2C as VFT_MARKER_H2C};

        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 80, 24),
        );

        // Use a temp path so the portal's VFT engine can actually
        // open the upload destination. We never fill it; the test
        // immediately resets.
        let tmp_path = std::env::temp_dir()
            .join(format!("vft-prt-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp_path);
        let begin = VftCommand::BeginUpload(BeginUploadBody {
            transfer_id: "t".into(),
            host_path: tmp_path.to_string_lossy().into_owned(),
            basename: "".into(),
            total_bytes: 100,
            flags: 0,
            mode: 0,
            mtime: 0,
        });
        let begin_env = vft_build_envelope(&[(begin, 1)]);
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: begin_env,
        });
        let _ = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 2, &body);

        // Now send `\x1b c` (RIS) inside the portal. Per spec §10
        // this aborts every transfer in the portal's VFT engine.
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: b"\x1bc".to_vec(),
        });
        let frames = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 3, &body);

        let raw = first_event(&frames, EVT_RAW_REPLY)
            .expect("expected RawReply with TransferAborted");
        let mut r = Reader::new(&raw.body);
        assert_eq!(r.string().unwrap(), "p");
        let inner_h2c = r.bytes().unwrap();
        let mut s = vft_protocol::apc::ApcStream::with_marker(*VFT_MARKER_H2C);
        let out = s.feed(inner_h2c);
        assert!(!out.payloads.is_empty(), "expected at least one VFT envelope");

        // Find the TransferAborted frame and verify its reason byte.
        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        let _v = r.u8().unwrap();
        let _len = r.u32().unwrap();
        let mut saw_abort = false;
        while !r.at_end() {
            let frame_type = r.u8().unwrap();
            let _rid = r.u32().unwrap();
            let body_len = r.u32().unwrap() as usize;
            let body = r.take(body_len).unwrap();
            if frame_type == EVT_TRANSFER_ABORTED {
                let mut br = Reader::new(body);
                assert_eq!(br.string().unwrap(), "t");
                assert_eq!(br.u8().unwrap(), ABORT_HOST_RESET);
                saw_abort = true;
            }
        }
        assert!(saw_abort, "expected TransferAborted in the response");
        let _ = std::fs::remove_file(&tmp_path);
    }

    #[test]
    fn take_all_pending_image_deletes_empty_when_no_images() {
        let mut engine = PrtEngine::new();
        assert!(engine.take_all_pending_image_deletes().is_empty());

        // Even with portals (and sub-portals) that hold no images.
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("p", 10, 10),
        );
        let inner_create = make_create_body("inner", 5, 5);
        let mut inner_frames = Vec::new();
        append_frame(&mut inner_frames, CMD_CREATE_PORTAL, 7, &inner_create);
        let inner_envelope = wrap_c2t_envelope(&inner_frames);
        let body = encode::write_portal_body(&WritePortalBody {
            id: "p".into(),
            data: inner_envelope,
        });
        let _ = dispatch_full(&mut engine, CMD_WRITE_PORTAL, 2, &body);

        assert!(engine.take_all_pending_image_deletes().is_empty());
    }

    #[test]
    fn deleting_portal_drains_subtree_for_destroy() {
        // Build host → outer "A" → sub-portal "X". DeletePortal("A")
        // must walk the subtree, drain GPU IDs, and route them through
        // PrtEngine::pending_image_deletes for the renderer to free.
        // Without real GPU IDs (femtovg::ImageId is opaque), we can't
        // assert non-empty contents, but we CAN assert the machinery
        // runs without panicking and yields a Vec on the next drain.
        let mut engine = PrtEngine::new();
        let _ = dispatch_one(
            &mut engine,
            CMD_CREATE_PORTAL,
            1,
            &make_create_body("A", 80, 24),
        );
        let inner_body = make_create_body("X", 40, 12);
        let mut inner_frames = Vec::new();
        append_frame(&mut inner_frames, CMD_CREATE_PORTAL, 7, &inner_body);
        let env = wrap_c2t_envelope(&inner_frames);
        let _ = dispatch_full(
            &mut engine,
            CMD_WRITE_PORTAL,
            2,
            &encode::write_portal_body(&WritePortalBody {
                id: "A".into(),
                data: env,
            }),
        );

        let body = encode::delete_portal_body("A");
        assert_eq!(
            dispatch_one(&mut engine, CMD_DELETE_PORTAL, 3, &body).frame_type,
            RSP_OK
        );
        assert!(engine.state.current().portals.is_empty());
        // Drain runs over the now-empty live tree; engine-level queue
        // was populated synchronously during DeletePortal but contains
        // only IDs from images that were actually allocated on the
        // GPU (none, in this test). Method must still return cleanly.
        let _ = engine.take_all_pending_image_deletes();
    }

    #[test]
    fn b64_decode_basic() {
        assert_eq!(b64_decode(b"aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64_decode(b"").unwrap(), b"");
        // Whitespace is tolerated.
        assert_eq!(b64_decode(b"aGVs\nbG8=").unwrap(), b"hello");
        // Garbage is not.
        assert!(b64_decode(b"!!!!").is_none());
    }
}
