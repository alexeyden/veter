// APC envelope wrapping (§1.1–1.2) for both directions, plus host-side
// body builders for ProbeResponse, Err, and every event in §8.

use crate::codec::{Reader, stuff, Writer};
use crate::frame::*;

/// One colour a host publishes with the text it picked to sit on it
/// (§10): an accent slot, or a named swatch from the theme. `name` is
/// empty for an accent the theme gave as a literal colour rather than
/// by swatch name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Swatch {
    pub name: String,
    /// Straight RGBA8.
    pub fill: [u8; 4],
    /// Straight RGBA8 text over `fill`.
    pub text: [u8; 4],
}

impl Swatch {
    fn encode(&self, w: &mut Writer) {
        w.str(&self.name);
        for b in self.fill.into_iter().chain(self.text) {
            w.u8(b);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, crate::codec::DecodeError> {
        let name = r.string()?.to_string();
        let fill = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
        let text = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
        Ok(Self { name, fill, text })
    }
}

/// The host's `host.*` palette as §10 reports it — the tail of a
/// ProbeResponse and the body of `HostThemeChanged` (§8.13), so the two
/// cannot drift apart.
///
/// Every part after the accent is positional and all-or-nothing: a
/// body that stops before or part-way through one decodes with it (and
/// everything after it) absent, never half-filled.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct HostTheme {
    /// What `host.accent` resolves to for the engine that reports it.
    pub accent: [u8; 4],
    /// The eight non-accent `host.*` colours (VGE §7.3), in the order
    /// `veter_host::vge::HostThemeColors::IDS` lists them.
    pub colors: Option<[[u8; 4]; 8]>,
    /// The accent rotation, *starting with this engine's own* — entry 0
    /// is `accent`, entry 1 what a portal one level in resolves, and so
    /// on, wrapping. What a party that hosts portals of its own (a
    /// session daemon) needs to rotate onward from here. Only after
    /// `colors`.
    pub accents: Vec<Swatch>,
    /// The theme's named swatches (`host.swatch.<name>`). Only after
    /// `accents`.
    pub swatches: Vec<Swatch>,
    /// The style engine the host asks its clients to draw with
    /// (`flat`, `aero`), which the colours above are picked for. Empty
    /// when the host names none. Only after `swatches`.
    pub style: String,
    /// The user's settings for the style engines, `<engine>.<key>` →
    /// value, every engine's whichever one `style` names — so a client
    /// that overrides the engine still draws it the user's way. What
    /// the keys mean is the engines' business; a client ignores one it
    /// does not know. Only after `style`.
    pub style_options: Vec<(String, String)>,
}

impl HostTheme {
    pub fn encode(&self, w: &mut Writer) {
        for b in self.accent {
            w.u8(b);
        }
        let Some(colors) = self.colors else { return };
        for quad in colors {
            for b in quad {
                w.u8(b);
            }
        }
        for list in [&self.accents, &self.swatches] {
            w.u8(list.len().min(u8::MAX as usize) as u8);
            for s in list.iter().take(u8::MAX as usize) {
                s.encode(w);
            }
        }
        w.str(&self.style);
        w.u8(self.style_options.len().min(u8::MAX as usize) as u8);
        for (key, value) in self.style_options.iter().take(u8::MAX as usize) {
            w.str(key);
            w.str(value);
        }
    }

    /// Inverse of [`Self::encode`]. Errors only when the accent itself
    /// is cut short.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, crate::codec::DecodeError> {
        let accent = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
        let mut theme = Self {
            accent,
            colors: None,
            accents: Vec::new(),
            swatches: Vec::new(),
            style: String::new(),
            style_options: Vec::new(),
        };
        let mut quads = [[0u8; 4]; 8];
        for slot in &mut quads {
            match (r.u8(), r.u8(), r.u8(), r.u8()) {
                (Ok(a), Ok(b), Ok(c), Ok(d)) => *slot = [a, b, c, d],
                _ => return Ok(theme),
            }
        }
        theme.colors = Some(quads);
        let mut list = || -> Option<Vec<Swatch>> {
            let n = r.u8().ok()?;
            (0..n).map(|_| Swatch::decode(r).ok()).collect()
        };
        let Some(accents) = list() else { return Ok(theme) };
        theme.accents = accents;
        let Some(swatches) = list() else { return Ok(theme) };
        theme.swatches = swatches;
        let Ok(style) = r.string() else {
            return Ok(theme);
        };
        theme.style = style.to_string();
        let options = (|| -> Option<Vec<(String, String)>> {
            let n = r.u8().ok()?;
            (0..n)
                .map(|_| Some((r.string().ok()?.to_string(), r.string().ok()?.to_string())))
                .collect()
        })();
        theme.style_options = options.unwrap_or_default();
        Ok(theme)
    }
}

/// Build the body for a ProbeResponse (§2.1).
///
/// `vge_features`, if `Some`, is appended as a trailing u8 byte carrying
/// the §10 VGE-integration capability bits. The body length is the
/// source of truth, so omitting it (`None`) is equivalent to advertising
/// "VGE-in-portal not supported" — clients reading a shorter body MUST
/// treat missing trailing fields as zero. `theme` follows it on the
/// same terms.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ProbeBody {
    pub protocol_version: u16,
    pub max_portals: u32,
    pub max_portal_cells_w: u32,
    pub max_portal_cells_h: u32,
    pub max_scrollback_lines: u32,
    pub max_write_bytes: u32,
    pub features: u8,
    pub max_nesting_depth: u8,
    pub vge_features: Option<u8>,
    /// §10 — when the host themes `host.*` styles
    /// (`FEAT_VGE_HOST_THEMED_STYLES`), the palette those ids resolve
    /// to for the probing engine's nesting depth, so a client can derive
    /// its own shades from the colours it references by `StyleRef`.
    /// Encoded only after `vge_features`.
    pub theme: Option<HostTheme>,
}

impl ProbeBody {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(30);
        w.u16(self.protocol_version);
        w.u32(self.max_portals);
        w.u32(self.max_portal_cells_w);
        w.u32(self.max_portal_cells_h);
        w.u32(self.max_scrollback_lines);
        w.u32(self.max_write_bytes);
        w.u8(self.features);
        w.u8(self.max_nesting_depth);
        // The theme follows `vge_features`, so it can only be present
        // when `vge_features` is too (no positional gap).
        if let Some(extra) = self.vge_features {
            w.u8(extra);
            if let Some(theme) = &self.theme {
                theme.encode(&mut w);
            }
        }
        w.buf
    }

    /// Inverse of [`Self::encode`]. `vge_features` and `theme` are
    /// positional tail fields, so a body that stops early decodes with
    /// them `None` rather than failing — which is exactly how a host
    /// that predates them replies. A theme is read only when the
    /// `host_themed_styles` bit says there is one.
    pub fn decode(body: &[u8]) -> Result<Self, crate::codec::DecodeError> {
        let mut r = Reader::new(body);
        let protocol_version = r.u16()?;
        let max_portals = r.u32()?;
        let max_portal_cells_w = r.u32()?;
        let max_portal_cells_h = r.u32()?;
        let max_scrollback_lines = r.u32()?;
        let max_write_bytes = r.u32()?;
        let features = r.u8()?;
        let max_nesting_depth = r.u8()?;
        let vge_features = r.u8().ok();
        let theme = vge_features
            .filter(|f| f & FEAT_VGE_HOST_THEMED_STYLES != 0)
            .and_then(|_| HostTheme::decode(&mut r).ok());
        Ok(Self {
            protocol_version,
            max_portals,
            max_portal_cells_w,
            max_portal_cells_h,
            max_scrollback_lines,
            max_write_bytes,
            features,
            max_nesting_depth,
            vge_features,
            theme,
        })
    }

    /// The ProbeResponse in a decoded host→client payload (§1.2), if
    /// it carries one — what a client's probe loop reads each envelope
    /// with.
    pub fn from_payload(payload: &[u8]) -> Option<Self> {
        let mut r = Reader::new(payload);
        let _version = r.u8().ok()?;
        let _payload_len = r.u32().ok()?;
        while !r.at_end() {
            let frame_type = r.u8().ok()?;
            let _request_id = r.u32().ok()?;
            let len = r.u32().ok()? as usize;
            let body = r.take(len).ok()?;
            if frame_type == RSP_PROBE {
                return Self::decode(body).ok();
            }
        }
        None
    }
}

/// Build the body for an Err response (§4.1).
pub fn err_body(error_code: u16, message: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + 1 + message.len());
    w.u16(error_code);
    w.str(message);
    w.buf
}

// ---- §8 event bodies ---------------------------------------------------

pub fn raw_reply_body(id: &str, data: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len() + 5 + data.len());
    w.str(id);
    w.bytes(data);
    w.buf
}

/// Body for an `EVT_HOST_THEME_CHANGED` event (§8.13): the portal
/// id — empty for the host itself — and the same palette tail the
/// probe carries.
pub fn host_theme_changed_body(id: &str, theme: &HostTheme) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len() + 4 + 32);
    w.str(id);
    theme.encode(&mut w);
    w.buf
}

/// Inverse of [`host_theme_changed_body`], for clients. Returns the
/// portal id (empty for the host) and the palette.
pub fn parse_host_theme_changed(
    body: &[u8],
) -> Result<(String, HostTheme), crate::codec::DecodeError> {
    let mut r = Reader::new(body);
    let id = r.string()?.to_string();
    Ok((id, HostTheme::decode(&mut r)?))
}

pub fn bell_body(id: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len());
    w.str(id);
    w.buf
}

/// Body for an `EVT_PORTAL_ACTIVITY` event (§8). Carries only the
/// portal id; the activity heuristic lives host-side.
pub fn portal_activity_body(id: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len());
    w.str(id);
    w.buf
}

pub fn title_change_body(id: &str, title: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len() + title.len());
    w.str(id);
    w.str(title);
    w.buf
}

pub fn icon_name_change_body(id: &str, name: &str) -> Vec<u8> {
    title_change_body(id, name)
}

pub fn working_dir_change_body(id: &str, uri: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len() + uri.len());
    w.str(id);
    w.str(uri);
    w.buf
}

/// `selection` is the ASCII byte ('c', 'p', 's', …); `op` is
/// `CLIPBOARD_SET` (0) or `CLIPBOARD_QUERY` (1). For queries the spec
/// says `data` is empty; we still encode it consistently as a `bytes`
/// field so the wire layout stays uniform.
pub fn clipboard_op_body(id: &str, selection: u8, op: u8, data: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(3 + id.len() + 5 + data.len());
    w.str(id);
    w.u8(selection);
    w.u8(op);
    w.bytes(data);
    w.buf
}

pub fn cursor_visibility_change_body(id: &str, visible: bool) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len());
    w.str(id);
    w.u8(u8::from(visible));
    w.buf
}

pub fn buffer_mode_change_body(id: &str, on_alt: bool) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len());
    w.str(id);
    w.u8(u8::from(on_alt));
    w.buf
}

pub fn portal_evicted_body(id: &str, reason: u8) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + id.len());
    w.str(id);
    w.u8(reason);
    w.buf
}

pub fn resize_notify_body(id: &str, rows: u32, cols: u32) -> Vec<u8> {
    let mut w = Writer::with_capacity(9 + id.len());
    w.str(id);
    w.u32(rows);
    w.u32(cols);
    w.buf
}

pub fn mouse_mode_change_body(
    id: &str,
    protocol: u8,
    encoding: u8,
    focus_events: u8,
) -> Vec<u8> {
    let mut w = Writer::with_capacity(4 + id.len());
    w.str(id);
    w.u8(protocol);
    w.u8(encoding);
    w.u8(focus_events);
    w.buf
}

pub fn portal_scroll_delta_body(id: &str, delta: i32) -> Vec<u8> {
    let mut w = Writer::with_capacity(5 + id.len());
    w.str(id);
    w.i32(delta);
    w.buf
}

/// `EVT_PORTAL_SCROLL_SET` (§8.12): the id, then the target in
/// `SetPortalScrollback`'s own encoding.
pub fn portal_scroll_set_body(id: &str, to: crate::command::ScrollTarget) -> Vec<u8> {
    let mut w = Writer::with_capacity(13 + id.len());
    w.str(id);
    to.write(&mut w);
    w.buf
}

/// Inverse of [`portal_scroll_set_body`], for clients.
pub fn parse_portal_scroll_set(
    body: &[u8],
) -> Result<(String, crate::command::ScrollTarget), crate::codec::DecodeError> {
    let mut r = Reader::new(body);
    let id = r.string()?.to_string();
    let to = crate::command::ScrollTarget::read(&mut r)?;
    Ok((id, to))
}

// ---- frame + envelope wrapping ----------------------------------------

/// Append a single frame to an unstuffed payload buffer.
/// Frame layout (§1.2): u8 frame_type, u32 request_id, u32 body_length,
/// body[body_length].
pub fn append_frame(buf: &mut Vec<u8>, frame_type: u8, request_id: u32, body: &[u8]) {
    buf.push(frame_type);
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

fn wrap(frames_buf: &[u8], marker: &[u8; 3]) -> Vec<u8> {
    // §1.2 unstuffed payload = u8 protocol_version, u32 payload_length,
    // frames. payload_length is "length of the rest" — i.e. just the
    // frames region.
    let mut unstuffed = Vec::with_capacity(5 + frames_buf.len());
    unstuffed.push(PROTOCOL_VERSION);
    unstuffed.extend_from_slice(&(frames_buf.len() as u32).to_le_bytes());
    unstuffed.extend_from_slice(frames_buf);

    let mut env = Vec::with_capacity(7 + unstuffed.len());
    env.push(ESC);
    env.push(APC_OPEN);
    env.extend_from_slice(marker);
    stuff(&unstuffed, &mut env);
    env.push(ESC);
    env.push(ST_CLOSE);
    env
}

/// Wrap a frame buffer as a host→client envelope (lowercase `prt`
/// marker). This is what the host emits as responses and events.
pub fn wrap_t2c_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_T2C)
}

/// Wrap a frame buffer as a client→host envelope (uppercase `PRT`
/// marker). Used by clients (and the test CLI) to feed commands.
pub fn wrap_c2t_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_C2T)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apc::ApcStream;
    use crate::codec::Reader;

    fn swatch(name: &str, fill: u8, text: u8) -> Swatch {
        Swatch {
            name: name.into(),
            fill: [fill, 0, 0, 0xFF],
            text: [text, 0, 0, 0xFF],
        }
    }

    fn full_theme() -> HostTheme {
        HostTheme {
            accent: [0x12, 0x34, 0x56, 0xFF],
            colors: Some(std::array::from_fn(|i| [i as u8, 2, 3, 4])),
            accents: vec![swatch("blue", 1, 2), swatch("", 3, 4)],
            swatches: vec![swatch("orange", 5, 6)],
            style: "aero".into(),
            style_options: vec![("aero.bubbles".into(), "false".into())],
        }
    }

    #[test]
    fn probe_body_encoded_size() {
        // Required prefix: 2 + 4*5 + 1 + 1 = 24 bytes.
        let pb = ProbeBody {
            protocol_version: 0,
            max_portals: 64,
            max_portal_cells_w: 1024,
            max_portal_cells_h: 512,
            max_scrollback_lines: 100_000,
            max_write_bytes: 1 << 20,
            features: 0xFF,
            max_nesting_depth: 8,
            vge_features: None,
            theme: None,
        };
        assert_eq!(pb.encode().len(), 24);

        // With trailing VGE byte: 25.
        let pb2 = ProbeBody {
            vge_features: Some(FEAT_VGE_IN_PORTAL),
            ..pb
        };
        assert_eq!(pb2.encode().len(), 25);

        // With VGE byte + accent RGBA: 25 + 4 = 29.
        let accent_only = HostTheme {
            colors: None,
            accents: Vec::new(),
            swatches: Vec::new(),
            style: String::new(),
            style_options: Vec::new(),
            ..full_theme()
        };
        let pb3 = ProbeBody {
            vge_features: Some(FEAT_VGE_IN_PORTAL | FEAT_VGE_HOST_THEMED_STYLES),
            theme: Some(accent_only.clone()),
            ..pb2
        };
        assert_eq!(pb3.encode().len(), 29);

        // + the eight theme colours, two empty swatch lists, an empty
        // style name and no options: 29 + 32 + 2 + 1 + 1 = 65.
        let pb5 = ProbeBody {
            theme: Some(HostTheme {
                colors: full_theme().colors,
                ..accent_only.clone()
            }),
            ..pb3
        };
        assert_eq!(pb5.encode().len(), 65);

        // A theme without vge_features cannot be encoded (no gap): it
        // is silently dropped, length stays at the 24-byte prefix.
        let pb4 = ProbeBody {
            vge_features: None,
            theme: Some(accent_only),
            ..pb5
        };
        assert_eq!(pb4.encode().len(), 24);
    }

    /// The tail fields are positional, so what a decoder must survive
    /// is a body that stops anywhere: at the prefix (a host that
    /// predates VGE integration), after the VGE byte, after the accent,
    /// mid-way through the theme block, or mid-way through a swatch.
    #[test]
    fn probe_body_round_trips_and_tolerates_truncation() {
        let full = ProbeBody {
            protocol_version: 1,
            max_portals: 64,
            max_portal_cells_w: 1024,
            max_portal_cells_h: 512,
            max_scrollback_lines: 100_000,
            max_write_bytes: 1 << 20,
            features: 0xFF,
            max_nesting_depth: 8,
            vge_features: Some(FEAT_VGE_IN_PORTAL | FEAT_VGE_HOST_THEMED_STYLES),
            theme: Some(full_theme()),
        };
        let bytes = full.encode();
        let back = ProbeBody::decode(&bytes).unwrap();
        assert_eq!(back.protocol_version, full.protocol_version);
        assert_eq!(back.max_write_bytes, full.max_write_bytes);
        assert_eq!(back.vge_features, full.vge_features);
        assert_eq!(back.theme, full.theme);
        let theme = |n: usize| ProbeBody::decode(&bytes[..n]).unwrap().theme;

        // Truncated to just before the theme block: accent survives,
        // colours are absent rather than an error.
        let t = theme(29).unwrap();
        assert_eq!(t.accent, full_theme().accent);
        assert_eq!(t.colors, None);

        // Half a theme block is no theme block — all eight or none.
        assert_eq!(theme(45).unwrap().colors, None);

        // Cut inside the accent list: the colours stand, the lists go.
        let t = theme(66).unwrap();
        assert_eq!(t.colors, full_theme().colors);
        assert!(t.accents.is_empty() && t.swatches.is_empty());

        // Cut inside the swatch list: the accents stand. (The style
        // name, `aero`, takes five bytes, and the one option after it
        // twenty: a count, `aero.bubbles` and `false`.)
        const OPTIONS: usize = 1 + 13 + 6;
        let t = theme(bytes.len() - OPTIONS - 5 - 3).unwrap();
        assert_eq!(t.accents, full_theme().accents);
        assert!(t.swatches.is_empty());

        // Cut inside the style name: the swatches stand, the host names
        // no style.
        let t = theme(bytes.len() - OPTIONS - 2).unwrap();
        assert_eq!(t.swatches, full_theme().swatches);
        assert_eq!(t.style, "");

        // Cut inside the options: the style stands, with no options —
        // never half of them.
        let t = theme(bytes.len() - 2).unwrap();
        assert_eq!(t.style, "aero");
        assert!(t.style_options.is_empty());

        // A pre-VGE host's 24-byte body.
        let back = ProbeBody::decode(&bytes[..24]).unwrap();
        assert_eq!(back.vge_features, None);
        assert_eq!(back.theme, None);

        // A longer body than this client knows about is tolerated by
        // skipping the excess (§2.1).
        let mut longer = bytes.clone();
        longer.extend_from_slice(&[0xAA; 6]);
        assert_eq!(ProbeBody::decode(&longer).unwrap().theme, full.theme);

        // And it is found inside a whole host→client payload.
        let mut frames = Vec::new();
        append_frame(&mut frames, RSP_OK, 1, &[]);
        append_frame(&mut frames, RSP_PROBE, 2, &bytes);
        let mut payload = vec![PROTOCOL_VERSION];
        payload.extend_from_slice(&(frames.len() as u32).to_le_bytes());
        payload.extend_from_slice(&frames);
        assert_eq!(ProbeBody::from_payload(&payload).unwrap().theme, full.theme);
    }

    /// The event carries the probe's own tail, so it survives the same
    /// truncations — a host that publishes accents only ends the body
    /// after the accent.
    #[test]
    fn host_theme_changed_body_round_trips() {
        let body = host_theme_changed_body("", &full_theme());
        let (id, got) = parse_host_theme_changed(&body).unwrap();
        assert_eq!(id, "", "the host itself names no portal");
        assert_eq!(got, full_theme());

        // Accents only.
        let only = HostTheme {
            colors: None,
            accents: Vec::new(),
            swatches: Vec::new(),
            style: String::new(),
            style_options: Vec::new(),
            ..full_theme()
        };
        let body = host_theme_changed_body("", &only);
        assert_eq!(parse_host_theme_changed(&body).unwrap().1, only);

        // A body too short even for the accent is an error, not a
        // silently zeroed colour.
        assert!(parse_host_theme_changed(&[0]).is_err());
    }

    #[test]
    fn t2c_envelope_passes_through_apc_stream() {
        // ApcStream defaults to MARKER_C2T; a T2C envelope must come back
        // as plain passthrough.
        let mut frames = Vec::new();
        append_frame(&mut frames, RSP_OK, 42, &[]);
        let env = wrap_t2c_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.payloads().next().is_none());
        assert_eq!(out.passthrough, env);
    }

    #[test]
    fn c2t_envelope_round_trips_with_stuffing() {
        let mut frames = Vec::new();
        // Body deliberately contains ESC bytes to exercise stuffing.
        append_frame(&mut frames, RSP_OK, 0xDEAD_BEEF, &[0x1B, 0x00, 0x1B]);
        let env = wrap_c2t_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads().count(), 1);

        let mut r = Reader::new(out.payloads().next().unwrap());
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let payload_len = r.u32().unwrap();
        assert_eq!(payload_len as usize, frames.len());
        assert_eq!(r.u8().unwrap(), RSP_OK);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u32().unwrap(), 3);
    }

    #[test]
    fn raw_reply_body_round_trip() {
        let body = raw_reply_body("left", b"\x1b[6;42R");
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "left");
        assert_eq!(r.bytes().unwrap(), b"\x1b[6;42R");
        assert!(r.at_end());
    }

    #[test]
    fn mouse_mode_change_body_round_trip() {
        let body = mouse_mode_change_body("p", 2, 2, 1);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u8().unwrap(), 2);
        assert_eq!(r.u8().unwrap(), 2);
        assert_eq!(r.u8().unwrap(), 1);
        assert!(r.at_end());
    }

    #[test]
    fn portal_scroll_delta_body_round_trip() {
        let body = portal_scroll_delta_body("pane-1", -3);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "pane-1");
        assert_eq!(r.i32().unwrap(), -3);
        assert!(r.at_end());
    }

    #[test]
    fn portal_scroll_set_body_round_trip() {
        use crate::command::ScrollTarget;
        for to in [ScrollTarget::Live, ScrollTarget::Delta(-3), ScrollTarget::Line(1234)] {
            let body = portal_scroll_set_body("pane-2", to);
            assert_eq!(parse_portal_scroll_set(&body).unwrap(), ("pane-2".to_string(), to));
        }
    }

    #[test]
    fn portal_evicted_body_round_trip() {
        let body = portal_evicted_body("test-#123", EVICT_SCROLLBACK);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "test-#123");
        assert_eq!(r.u8().unwrap(), EVICT_SCROLLBACK);
        assert!(r.at_end());
    }

    #[test]
    fn resize_notify_body_round_trip() {
        let body = resize_notify_body("p", 24, 80);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "p");
        assert_eq!(r.u32().unwrap(), 24);
        assert_eq!(r.u32().unwrap(), 80);
        assert!(r.at_end());
    }

    #[test]
    fn portal_activity_body_round_trip() {
        let body = portal_activity_body("p3");
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "p3");
        assert!(r.at_end());
    }

    #[test]
    fn err_body_round_trip() {
        let body = err_body(ERR_UNKNOWN_PORTAL, "id not found");
        let mut r = Reader::new(&body);
        assert_eq!(r.u16().unwrap(), ERR_UNKNOWN_PORTAL);
        assert_eq!(r.string().unwrap(), "id not found");
        assert!(r.at_end());
    }

    #[test]
    fn write_portal_body_in_envelope_unstuffs_correctly() {
        // Build a c2t envelope carrying CMD_WRITE_PORTAL with an ESC in
        // the inner data, run through ApcStream, decode, and verify.
        use crate::command::{parse, Command};

        let mut body = Writer::new();
        body.str("left");
        body.bytes(&[0x1B, b'[', b'H']);

        let mut frames = Vec::new();
        append_frame(&mut frames, CMD_WRITE_PORTAL, 7, &body.buf);
        let env = wrap_c2t_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert_eq!(out.payloads().count(), 1);

        let payload = out.payloads().next().unwrap();
        let mut r = Reader::new(payload);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _ = r.u32().unwrap();
        let frame_type = r.u8().unwrap();
        let request_id = r.u32().unwrap();
        let body_len = r.u32().unwrap();
        let body_bytes = r.take(body_len as usize).unwrap();
        assert_eq!(frame_type, CMD_WRITE_PORTAL);
        assert_eq!(request_id, 7);

        let cmd = parse(frame_type, body_bytes).unwrap();
        let Command::WritePortal(b) = cmd else {
            panic!();
        };
        assert_eq!(b.id, "left");
        assert_eq!(b.data, vec![0x1B, b'[', b'H']);
    }
}
