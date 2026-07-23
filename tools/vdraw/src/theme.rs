//! Runtime accent colour for vdraw's UI overlays.
//!
//! The selection outline + handles, the text caret, and the active-tool
//! highlight in the bottom chrome all key off a single accent. By
//! default that is vdraw's own built-in blue; when the terminal
//! advertises a themed `host.accent` (VGE §7.3, surfaced through the PRT
//! probe — the same mechanism vmux uses) the accent is adopted once at
//! startup so the UI tracks veter's theme and, inside nested panes, the
//! pane's depth.
//!
//! A set-once global keeps the accent out of every render signature; it
//! never changes after startup, mirroring `HOST_ACCENT_RGBA` in vmux.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vge_protocol::command::{Color, Style};

/// vdraw's own active-tool highlight fill (`#3474D6` at 0.95), the
/// fallback when the host does not theme `host.*`. The selection/caret
/// fallback is [`crate::render::ACCENT`], reused so there is a single
/// built-in accent.
const DEFAULT_ACTIVE_BG: Color = Color {
    r: 52.0 / 255.0,
    g: 116.0 / 255.0,
    b: 214.0 / 255.0,
    a: 0.95,
};

/// Host-reported accent, packed `0xRRGGBBAA`. Valid only when
/// `HOST_THEMED` is set.
static ACCENT_RGBA: AtomicU32 = AtomicU32::new(0);
static HOST_THEMED: AtomicBool = AtomicBool::new(false);

/// Adopt the host's themed accent. Called once at startup from the PRT
/// probe; a no-op path (never called) leaves the built-in accents.
pub fn set_host_accent(c: Color) {
    ACCENT_RGBA.store(pack(c), Ordering::Relaxed);
    HOST_THEMED.store(true, Ordering::Relaxed);
}

/// The host's accent as a concrete colour, or `None` when it does not
/// theme `host.*` (in which case callers use their own default).
fn host_accent() -> Option<Color> {
    if HOST_THEMED.load(Ordering::Relaxed) {
        Some(unpack(ACCENT_RGBA.load(Ordering::Relaxed)))
    } else {
        None
    }
}

/// Full-opacity accent style for the selection overlay and text caret.
pub fn selection_accent() -> Style {
    match host_accent() {
        Some(c) => Style::Flat(c),
        None => crate::render::ACCENT,
    }
}

/// Translucent accent fill for the chrome's active-tool / active-option
/// highlight.
pub fn active_tool_fill() -> Style {
    match host_accent() {
        // Keep the built-in fill's 0.95 opacity over the host's hue.
        Some(c) => Style::Flat(Color { a: 0.95, ..c }),
        None => Style::Flat(DEFAULT_ACTIVE_BG),
    }
}

fn pack(c: Color) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (q(c.r) << 24) | (q(c.g) << 16) | (q(c.b) << 8) | q(c.a)
}

fn unpack(v: u32) -> Color {
    Color {
        r: ((v >> 24) & 0xFF) as f32 / 255.0,
        g: ((v >> 16) & 0xFF) as f32 / 255.0,
        b: ((v >> 8) & 0xFF) as f32 / 255.0,
        a: (v & 0xFF) as f32 / 255.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_round_trips() {
        let c = Color {
            r: 0.2,
            g: 0.6,
            b: 0.9,
            a: 1.0,
        };
        let back = unpack(pack(c));
        for (x, y) in [(c.r, back.r), (c.g, back.g), (c.b, back.b), (c.a, back.a)] {
            assert!((x - y).abs() < 1.0 / 255.0 + 1e-6, "{x} vs {y}");
        }
    }
}
