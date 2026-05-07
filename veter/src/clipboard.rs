//! Host-side clipboard wiring.
//!
//! Two pieces:
//! 1. [`ClipboardManager`] — lazy `arboard` wrapper. `arboard::Clipboard::new`
//!    can fail (no display, headless tests), so we hold an `Option` and
//!    treat a missing backend as "clipboard ops are no-ops".
//! 2. [`HostCallbacks`] — `vt100::Callbacks` impl installed on the host
//!    parser. Catches OSC 52 set requests from host-direct children
//!    (e.g. anything spawned without going through vmux/PRT), decodes
//!    base64, and buffers the text for the App to apply. OSC 52 query
//!    is left as the default no-op (refuse policy — see plan stage 1).

pub struct ClipboardManager {
    inner: Option<arboard::Clipboard>,
}

impl ClipboardManager {
    pub fn new() -> Self {
        Self {
            inner: arboard::Clipboard::new().ok(),
        }
    }

    pub fn set_text(&mut self, text: &str) {
        if let Some(cb) = &mut self.inner {
            let _ = cb.set_text(text.to_string());
        }
    }

    pub fn get_text(&mut self) -> Option<String> {
        self.inner.as_mut().and_then(|cb| cb.get_text().ok())
    }

    /// Linux PRIMARY selection (auto-populated on text selection,
    /// pasted by middle-click). On Wayland this rides on the
    /// `wayland-data-control` protocol; on X11 it's a separate
    /// selection atom.
    pub fn set_primary(&mut self, text: &str) {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        if let Some(cb) = &mut self.inner {
            let _ = cb
                .set()
                .clipboard(LinuxClipboardKind::Primary)
                .text(text.to_string());
        }
    }

    pub fn get_primary(&mut self) -> Option<String> {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        self.inner.as_mut().and_then(|cb| {
            cb.get()
                .clipboard(LinuxClipboardKind::Primary)
                .text()
                .ok()
        })
    }
}

/// vt100 callbacks installed on the host parser. Buffers OSC 52 set
/// payloads (decoded from base64) for the App to drain each tick.
#[derive(Default)]
pub struct HostCallbacks {
    pub pending_set: Vec<String>,
}

impl vt100::Callbacks for HostCallbacks {
    fn copy_to_clipboard(
        &mut self,
        _: &mut vt100::Screen,
        _ty: &[u8],
        data: &[u8],
    ) {
        // §8.4 — `data` is base64. Bad input ⇒ drop silently rather than
        // poisoning the clipboard with garbage.
        let Some(decoded) = b64_decode(data) else {
            return;
        };
        let Ok(text) = String::from_utf8(decoded) else {
            return;
        };
        self.pending_set.push(text);
    }

    // paste_from_clipboard intentionally left as the default no-op:
    // OSC 52 query is refused. Programs that issue it just don't get
    // a reply.
}

/// Build the byte sequence to write to the PTY when pasting `text`.
/// Normalizes line endings (CR/CRLF → LF) and strips any embedded
/// `ESC [ 201 ~` end marker so a malicious clipboard cannot escape
/// bracketed-paste mode and inject commands. When `bracketed` is true
/// the output is wrapped in the standard `ESC [ 200 ~ … ESC [ 201 ~`
/// envelope.
pub fn build_paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let sanitized = normalized.replace("\x1b[201~", "");
    if bracketed {
        let mut out = Vec::with_capacity(sanitized.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.into_bytes()
    }
}

/// Standard base64 decoder for OSC 52 set form (§8.4). Tolerates `=`
/// padding and whitespace; rejects on any other non-alphabet byte.
/// Returns `None` for malformed input.
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
