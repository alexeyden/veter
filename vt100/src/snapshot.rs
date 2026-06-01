//! Binary state snapshot for `Screen` (VSS extension's `VtFragment`
//! payload). Captures every internal field — alt-screen grid, DECSC
//! saved cursor, scroll region, origin mode, charset, all input modes
//! — so a fresh `Screen` can be reconstructed verbatim on the
//! renderer side without re-parsing any escape sequences.
//!
//! Used from `tools/vsd` to compose a snapshot and from
//! `veter-host`'s `VssEngine` to apply one. See `doc/session-manager.md`
//! §4 for the protocol-level role.
//!
//! Internal-only codec primitives live here; no external crate
//! dependency is taken on. The wire conventions match the
//! `vss-protocol` primitive codec (little-endian, LEB128 `varu`) so a
//! reader/writer in either crate would yield identical bytes.

/// Bumped on any breaking change to the binary layout below. Strict
/// match — see [`Screen::restore_from_binary_snapshot`].
pub(crate) const SNAPSHOT_KIND_VERSION: u16 = 1;

/// Error returned when a `Screen` binary snapshot cannot be decoded:
/// wrong kind version, truncated payload, or otherwise malformed.
///
/// The wrapped `&'static str` is a short human-readable reason
/// suitable for logging; treat it as opaque from outside the crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotError(pub &'static str);

impl SnapshotError {
    pub(crate) fn kind_version_mismatch(_got: u16, _want: u16) -> Self {
        Self("vt100 snapshot kind version mismatch")
    }
    pub(crate) fn bad_payload(reason: &'static str) -> Self {
        Self(reason)
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for SnapshotError {}

pub(crate) struct Writer<'a> {
    pub buf: &'a mut Vec<u8>,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(buf: &'a mut Vec<u8>) -> Self {
        Self { buf }
    }
    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[allow(dead_code)]
    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[allow(dead_code)]
    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn varu(&mut self, mut v: u64) {
        loop {
            let mut b = (v & 0x7F) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
                self.buf.push(b);
            } else {
                self.buf.push(b);
                return;
            }
        }
    }
    #[allow(dead_code)]
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.varu(b.len() as u64);
        self.buf.extend_from_slice(b);
    }
    pub(crate) fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }
}

pub(crate) struct Reader<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        if self.pos + n > self.buf.len() {
            return Err(SnapshotError::bad_payload("truncated"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16, SnapshotError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    #[allow(dead_code)]
    pub(crate) fn u32(&mut self) -> Result<u32, SnapshotError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    #[allow(dead_code)]
    pub(crate) fn u64(&mut self) -> Result<u64, SnapshotError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    pub(crate) fn varu(&mut self) -> Result<u64, SnapshotError> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            let val = u64::from(b & 0x7F);
            if shift >= 64 {
                return Err(SnapshotError::bad_payload("varu overflow"));
            }
            result |= val
                .checked_shl(shift)
                .ok_or(SnapshotError::bad_payload("varu shift"))?;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }
    #[allow(dead_code)]
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], SnapshotError> {
        let len = self.varu()? as usize;
        self.take(len)
    }
    pub(crate) fn bool(&mut self) -> Result<bool, SnapshotError> {
        Ok(self.u8()? != 0)
    }
}

// ---- leaf-type codecs (Color, MouseProtocolMode/Encoding) -----------

const COLOR_DEFAULT: u8 = 0;
const COLOR_IDX: u8 = 1;
const COLOR_RGB: u8 = 2;

pub(crate) fn encode_color(w: &mut Writer, c: crate::attrs::Color) {
    match c {
        crate::attrs::Color::Default => w.u8(COLOR_DEFAULT),
        crate::attrs::Color::Idx(i) => {
            w.u8(COLOR_IDX);
            w.u8(i);
        }
        crate::attrs::Color::Rgb(r, g, b) => {
            w.u8(COLOR_RGB);
            w.u8(r);
            w.u8(g);
            w.u8(b);
        }
    }
}

pub(crate) fn decode_color(r: &mut Reader) -> Result<crate::attrs::Color, SnapshotError> {
    let tag = r.u8()?;
    match tag {
        COLOR_DEFAULT => Ok(crate::attrs::Color::Default),
        COLOR_IDX => Ok(crate::attrs::Color::Idx(r.u8()?)),
        COLOR_RGB => Ok(crate::attrs::Color::Rgb(r.u8()?, r.u8()?, r.u8()?)),
        _ => Err(SnapshotError::bad_payload("unknown color tag")),
    }
}

pub(crate) fn encode_mouse_mode(w: &mut Writer, m: crate::screen::MouseProtocolMode) {
    let tag = match m {
        crate::screen::MouseProtocolMode::None => 0u8,
        crate::screen::MouseProtocolMode::Press => 1,
        crate::screen::MouseProtocolMode::PressRelease => 2,
        crate::screen::MouseProtocolMode::ButtonMotion => 3,
        crate::screen::MouseProtocolMode::AnyMotion => 4,
    };
    w.u8(tag);
}

pub(crate) fn decode_mouse_mode(r: &mut Reader) -> Result<crate::screen::MouseProtocolMode, SnapshotError> {
    Ok(match r.u8()? {
        0 => crate::screen::MouseProtocolMode::None,
        1 => crate::screen::MouseProtocolMode::Press,
        2 => crate::screen::MouseProtocolMode::PressRelease,
        3 => crate::screen::MouseProtocolMode::ButtonMotion,
        4 => crate::screen::MouseProtocolMode::AnyMotion,
        _ => return Err(SnapshotError::bad_payload("unknown mouse mode tag")),
    })
}

pub(crate) fn encode_mouse_encoding(w: &mut Writer, e: crate::screen::MouseProtocolEncoding) {
    let tag = match e {
        crate::screen::MouseProtocolEncoding::Default => 0u8,
        crate::screen::MouseProtocolEncoding::Utf8 => 1,
        crate::screen::MouseProtocolEncoding::Sgr => 2,
    };
    w.u8(tag);
}

pub(crate) fn decode_mouse_encoding(
    r: &mut Reader,
) -> Result<crate::screen::MouseProtocolEncoding, SnapshotError> {
    Ok(match r.u8()? {
        0 => crate::screen::MouseProtocolEncoding::Default,
        1 => crate::screen::MouseProtocolEncoding::Utf8,
        2 => crate::screen::MouseProtocolEncoding::Sgr,
        _ => return Err(SnapshotError::bad_payload("unknown mouse encoding tag")),
    })
}

// ---- Charset (sets[4] + gl) -----------------------------------------

const CHARSET_ASCII: u8 = 0;
const CHARSET_DEC_SPECIAL: u8 = 1;

pub(crate) fn encode_charset_state(w: &mut Writer, cs: &crate::charset::CharsetState) {
    for s in &cs.sets {
        w.u8(match s {
            crate::charset::Charset::Ascii => CHARSET_ASCII,
            crate::charset::Charset::DecSpecialGraphics => CHARSET_DEC_SPECIAL,
        });
    }
    w.u8(cs.gl);
}

pub(crate) fn decode_charset_state(
    r: &mut Reader,
) -> Result<crate::charset::CharsetState, SnapshotError> {
    let mut sets = [crate::charset::Charset::Ascii; 4];
    for slot in &mut sets {
        *slot = match r.u8()? {
            CHARSET_ASCII => crate::charset::Charset::Ascii,
            CHARSET_DEC_SPECIAL => crate::charset::Charset::DecSpecialGraphics,
            _ => return Err(SnapshotError::bad_payload("unknown charset tag")),
        };
    }
    let gl = r.u8()?;
    if gl >= 4 {
        return Err(SnapshotError::bad_payload("charset gl out of range"));
    }
    Ok(crate::charset::CharsetState { sets, gl })
}

// ---- Attrs (fgcolor + bgcolor + mode bitfield) -----------------------

pub(crate) fn encode_attrs(w: &mut Writer, a: &crate::attrs::Attrs) {
    encode_color(w, a.fgcolor);
    encode_color(w, a.bgcolor);
    w.u8(a.mode);
}

pub(crate) fn decode_attrs(r: &mut Reader) -> Result<crate::attrs::Attrs, SnapshotError> {
    Ok(crate::attrs::Attrs {
        fgcolor: decode_color(r)?,
        bgcolor: decode_color(r)?,
        mode: r.u8()?,
    })
}
