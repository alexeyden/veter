// Portal table entries: per-portal vt100 instance, anchoring metadata,
// and a sub-engine for nested portals.

use std::collections::HashMap;

use vt100::{Callbacks, MouseProtocolEncoding, MouseProtocolMode, Parser, Screen};

use super::state::PrtEngine;
use crate::vft::VftEngine;
use crate::vge::VgeEngine;
use crate::vss::VssEngine;

/// A portal's vertical anchor.
///
/// `Live` portals re-evaluate `origin_y` against the current top of the
/// live region every frame (§5.2). `Scrollback` portals snapshot
/// `top_of_live_screen + origin_y` at command-processing time and stay
/// pinned to that absolute scrollback line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalAnchor {
    Live { origin_y: i32 },
    Scrollback { anchor_line: i64 },
}

/// Per-portal vt100 callbacks: collect bell/title/icon/clipboard/OSC-7
/// signals into a queue the engine drains after `Parser::process` returns.
///
/// Keeping these as raw byte payloads (vs. parsed PRT events) lets the
/// engine-level translator handle base64 decoding and OSC-7 selection in
/// one place, near the wire-format helpers in `prt_protocol::envelope`.
#[derive(Default)]
pub struct PortalCallbacks {
    pub events: Vec<RawCallbackEvent>,
}

#[derive(Debug)]
pub enum RawCallbackEvent {
    Bell,
    Title(Vec<u8>),
    IconName(Vec<u8>),
    /// OSC 52 set form. `selection` is the raw selector bytes (`c`, `p`, …);
    /// `data` is base64. Engine decodes before emitting the PRT event.
    ClipboardSet { selection: Vec<u8>, data: Vec<u8> },
    /// OSC 52 query form. The inner program asked the host for clipboard
    /// content; the client decides how to reply via WritePortal (§8.4).
    ClipboardQuery { selection: Vec<u8> },
    /// OSC 7 (cwd announcement). vt100 routes anything not 0/1/2/52 to
    /// `unhandled_osc`; engine matches on the leading parameter.
    Osc(Vec<Vec<u8>>),
}

impl Callbacks for PortalCallbacks {
    fn audible_bell(&mut self, _: &mut Screen) {
        self.events.push(RawCallbackEvent::Bell);
    }
    fn visual_bell(&mut self, _: &mut Screen) {
        self.events.push(RawCallbackEvent::Bell);
    }
    fn set_window_title(&mut self, _: &mut Screen, title: &[u8]) {
        self.events.push(RawCallbackEvent::Title(title.to_vec()));
    }
    fn set_window_icon_name(&mut self, _: &mut Screen, name: &[u8]) {
        self.events.push(RawCallbackEvent::IconName(name.to_vec()));
    }
    fn copy_to_clipboard(&mut self, _: &mut Screen, ty: &[u8], data: &[u8]) {
        self.events.push(RawCallbackEvent::ClipboardSet {
            selection: ty.to_vec(),
            data: data.to_vec(),
        });
    }
    fn paste_from_clipboard(&mut self, _: &mut Screen, ty: &[u8]) {
        self.events.push(RawCallbackEvent::ClipboardQuery {
            selection: ty.to_vec(),
        });
    }
    fn unhandled_osc(&mut self, _: &mut Screen, params: &[&[u8]]) {
        self.events
            .push(RawCallbackEvent::Osc(params.iter().map(|p| p.to_vec()).collect()));
    }
}

/// Snapshot of polled vt100 state for delta detection (§8.5–§8.9).
///
/// `MouseModeChange` events are coalesced by spec: comparing snapshots
/// before vs. after each `Parser::process` call naturally folds many
/// transitions into a single end-state event, satisfying the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolledStateCache {
    pub on_alt: bool,
    pub cursor_visible: bool,
    pub mouse_protocol: u8,
    pub mouse_encoding: u8,
    /// DECSET 1004 focus-event reporting. vt100 doesn't track this in
    /// v1, so the cache always carries `0`. Hook lands when vt100 gains
    /// the bit.
    pub focus_events: u8,
}

impl PolledStateCache {
    pub fn from_screen(screen: &Screen) -> Self {
        Self {
            on_alt: screen.alternate_screen(),
            cursor_visible: !screen.hide_cursor(),
            mouse_protocol: map_mouse_protocol(screen.mouse_protocol_mode()),
            mouse_encoding: map_mouse_encoding(screen.mouse_protocol_encoding()),
            focus_events: 0,
        }
    }
}

/// Map vt100's `MouseProtocolMode` to the spec's `protocol` byte (§8.9).
fn map_mouse_protocol(m: MouseProtocolMode) -> u8 {
    match m {
        MouseProtocolMode::None => 0,
        MouseProtocolMode::Press => 1,         // DECSET 9 (X10)
        MouseProtocolMode::PressRelease => 2,  // DECSET 1000 (normal)
        MouseProtocolMode::ButtonMotion => 3,  // DECSET 1002 (button)
        MouseProtocolMode::AnyMotion => 4,     // DECSET 1003 (any-event)
    }
}

fn map_mouse_encoding(e: MouseProtocolEncoding) -> u8 {
    match e {
        MouseProtocolEncoding::Default => 0,
        MouseProtocolEncoding::Utf8 => 1,
        MouseProtocolEncoding::Sgr => 2,
        // urxvt (1015) is not represented in vt100; if/when it lands,
        // the mapping returns 3 here.
    }
}

/// One portal in the host's table.
///
/// The portal owns its own `vt100::Parser<PortalCallbacks>` (sized as
/// `rows = size_h, cols = size_w`, with a scrollback ring of
/// `scrollback_lines`) and a recursively-nested `PrtEngine` that holds
/// any sub-portals created inside it.
#[allow(dead_code)] // id/creation_seq/scrollback_lines are read by Phase 6 rendering
pub struct Portal {
    pub id: String,
    pub size_w: u32,
    pub size_h: u32,
    pub origin_x: i32,
    pub anchor: PortalAnchor,
    pub is_visible: bool,
    pub draw_order: i32,
    pub creation_seq: u64,
    pub scrollback_lines: u32,
    pub vt: Parser<PortalCallbacks>,
    pub children: PrtEngine,
    /// §10 — every portal owns its own VGE engine that operates in
    /// the portal's cell coordinate space. `auto_reply_dsr` is forced
    /// off so PRT remains the sole DSR responder inside the portal.
    pub vge: VgeEngine,
    /// §10 (vft-in-portal) — every portal owns its own VFT engine
    /// scoped to that portal's transfer table. Workers act on the
    /// host's filesystem with the host's user permissions; isolation
    /// is OS-level (containers, user accounts), not VFT-level.
    pub vft: VftEngine,
    /// Every portal owns its own VSS engine so a `veterd attach`
    /// running inside *this* portal can ship its binary engine
    /// snapshot into this scope. See `doc/session-manager.md` §4.5
    /// (renderer-side application).
    pub vss: VssEngine,
    pub state_cache: PolledStateCache,
    /// DSR cursor-position queries observed on inbound bytes that
    /// haven't yet been answered. Drained after `vt.process` so the
    /// reply reflects post-process cursor state (§13.4).
    pub pending_cursor_queries: u32,
}

impl Portal {
    /// Drain every GPU image handle owned (directly or transitively
    /// via sub-portals) by this portal's VGE engines. Invoked by the
    /// PRT engine right before removing the portal from its scope, so
    /// the renderer can `canvas.delete_image()` each ID and the
    /// femtovg cache doesn't leak when a portal goes away.
    pub fn drain_for_destroy(&mut self) -> Vec<crate::vge::GpuImageId> {
        let mut deletes = Vec::new();
        // Already-scheduled deletes from in-flight DropImage commands.
        deletes.extend(self.vge.take_pending_image_deletes());
        // Live images currently uploaded against this portal's VGE.
        // `VgeState::reset` drains the image table and returns every
        // populated GPU id; the rest of the state it resets is
        // irrelevant here because the portal itself is going away.
        deletes.extend(self.vge.state.reset());
        // Recurse into the sub-portal subtree.
        deletes.extend(self.children.take_subtree_for_destroy());
        deletes
    }
}

/// Per-screen portal table (§5.4: main and alt screens have independent
/// tables).
pub struct PortalSet {
    pub portals: HashMap<String, Portal>,
    pub creation_counter: u64,
}

impl PortalSet {
    pub fn new() -> Self {
        Self {
            portals: HashMap::new(),
            creation_counter: 0,
        }
    }

    pub fn next_seq(&mut self) -> u64 {
        let n = self.creation_counter;
        self.creation_counter += 1;
        n
    }
}

impl Default for PortalSet {
    fn default() -> Self {
        Self::new()
    }
}
