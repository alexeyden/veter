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
//!    It also buffers OSC 0/2 window titles, which the App applies to
//!    the winit window.

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

    /// Put a raster image on the clipboard. `rgba` is straight-alpha
    /// RGBA8, `width * height * 4` bytes — the form VGE keeps uploaded
    /// images in (§8.1), so no conversion happens on the way out.
    /// arboard re-encodes it as PNG for the X11 / Wayland offer.
    ///
    /// CLIPBOARD only: PRIMARY is a text convention, and pasting an
    /// image with middle-click is not a thing anyone expects.
    pub fn set_image(&mut self, width: u32, height: u32, rgba: &[u8]) -> bool {
        let Some(cb) = &mut self.inner else {
            return false;
        };
        let data = arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: std::borrow::Cow::Borrowed(rgba),
        };
        match cb.set_image(data) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("veter: clipboard: image copy failed: {e}");
                false
            }
        }
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

/// Something the child asked the terminal for that only the App can
/// answer or apply: it needs the window, the palette or the cell
/// metrics, none of which the parser has.
///
/// Queued rather than acted on in place, because a `vt100::Callbacks`
/// impl sees only the screen — the same reason OSC 52 and the window
/// title are queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalRequest {
    /// `CSI 22 t` / `CSI 23 t` — terminfo `smcup` / `rmcup`.
    PushTitle,
    /// See [`PushTitle`](Self::PushTitle).
    PopTitle,
    /// `CSI 14 t` / `CSI 16 t` / `CSI 18 t`, by op. The sender blocks.
    SizeReport(u16),
    /// `OSC 4 ; i ; <spec>`, with the spec already parsed — a spec that
    /// doesn't parse never becomes a request.
    SetPalette(u8, (u8, u8, u8)),
    /// `OSC 4 ; i ; ?`.
    QueryPalette(u8),
    /// `OSC 104`, for one entry or (with `None`) the whole table.
    ResetPalette(Option<u8>),
    /// `OSC 10 / 11 / 12 ; <spec>`.
    SetDynamic(vt100::DynamicColor, (u8, u8, u8)),
    /// `OSC 10 / 11 / 12 ; ?` — vim and neovim both send this at
    /// startup and wait for the answer.
    QueryDynamic(vt100::DynamicColor),
    /// `OSC 110 / 111 / 112`.
    ResetDynamic(vt100::DynamicColor),
}

/// vt100 callbacks installed on the host parser. Buffers OSC 52 set
/// payloads (decoded from base64) and OSC 0/2 window titles for the App
/// to drain each tick, along with the [`TerminalRequest`]s that need
/// state the parser doesn't have.
#[derive(Default)]
pub struct HostCallbacks {
    pub pending_set: Vec<String>,
    /// Most recent OSC 0/2 title the child asked for, `None` once the
    /// App has applied it. Only the last one matters — a program that
    /// retitles per prompt would otherwise queue a burst of titles the
    /// window can never show.
    pub pending_title: Option<String>,
    /// Queued in arrival order: a palette write and the query that
    /// reads it back must not be reordered.
    pub pending_requests: Vec<TerminalRequest>,
}

impl HostCallbacks {
    /// Cap on the queue, so a program that spins on colour queries
    /// faster than the App drains cannot grow it without bound. A
    /// screenful of distinct requests is far more than anything real
    /// sends between two frames.
    const MAX_PENDING_REQUESTS: usize = 256;

    fn request(&mut self, req: TerminalRequest) {
        if self.pending_requests.len() < Self::MAX_PENDING_REQUESTS {
            self.pending_requests.push(req);
        }
    }
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

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        // OSC 0 sets icon name *and* title; we only track the title,
        // since a window with no icon-name concept has nowhere to put
        // the other half.
        self.pending_title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn push_window_title(&mut self, _: &mut vt100::Screen) {
        self.request(TerminalRequest::PushTitle);
    }

    fn pop_window_title(&mut self, _: &mut vt100::Screen) {
        self.request(TerminalRequest::PopTitle);
    }

    fn report_window_size(&mut self, _: &mut vt100::Screen, op: u16) {
        self.request(TerminalRequest::SizeReport(op));
    }

    fn set_palette_color(
        &mut self,
        _: &mut vt100::Screen,
        index: u8,
        spec: &[u8],
    ) {
        // A spec we can't read is dropped here rather than queued: the
        // App has nothing better to do with it either.
        if let Some(rgb) = veter_host::query::parse_color_spec(spec) {
            self.request(TerminalRequest::SetPalette(index, rgb));
        }
    }

    fn query_palette_color(&mut self, _: &mut vt100::Screen, index: u8) {
        self.request(TerminalRequest::QueryPalette(index));
    }

    fn reset_palette_color(
        &mut self,
        _: &mut vt100::Screen,
        index: Option<u8>,
    ) {
        self.request(TerminalRequest::ResetPalette(index));
    }

    fn set_dynamic_color(
        &mut self,
        _: &mut vt100::Screen,
        which: vt100::DynamicColor,
        spec: &[u8],
    ) {
        if let Some(rgb) = veter_host::query::parse_color_spec(spec) {
            self.request(TerminalRequest::SetDynamic(which, rgb));
        }
    }

    fn query_dynamic_color(
        &mut self,
        _: &mut vt100::Screen,
        which: vt100::DynamicColor,
    ) {
        self.request(TerminalRequest::QueryDynamic(which));
    }

    fn reset_dynamic_color(
        &mut self,
        _: &mut vt100::Screen,
        which: vt100::DynamicColor,
    ) {
        self.request(TerminalRequest::ResetDynamic(which));
    }
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
