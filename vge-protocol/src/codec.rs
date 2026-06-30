// Binary primitives shared by all VGE frame bodies (§1.4).

use super::frame::{
    ERR_BAD_PAYLOAD, ESC, ESC_MARK_TILDE, ESC_MARK_XON, ESC_MARK_XOFF, TILDE, XOFF, XON,
};

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Affine transform (§9.11), SVG / Canvas2D `matrix(a,b,c,d,e,f)`
/// convention: `x' = a·x + c·y + e; y' = b·x + d·y + f`. The linear
/// part acts on the element's rendered pixel geometry; the
/// translation `(e, f)` is in cell units.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Transform {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub fn is_finite(&self) -> bool {
        self.a.is_finite()
            && self.b.is_finite()
            && self.c.is_finite()
            && self.d.is_finite()
            && self.e.is_finite()
            && self.f.is_finite()
    }

    /// Visual rotation by `theta` radians about the element-local cell
    /// point `(cx, cy)` (§9.13 cookbook). Needs the terminal's cell
    /// pixel size because the linear part acts in pixel space while
    /// the translation is in cell units: `t = S⁻¹·(I − R)·S·c`.
    pub fn rotate_about(theta: f32, cx: f32, cy: f32, cell_w_px: f32, cell_h_px: f32) -> Self {
        let (sin, cos) = theta.sin_cos();
        Transform {
            a: cos,
            b: sin,
            c: -sin,
            d: cos,
            e: cx * (1.0 - cos) + cy * sin * (cell_h_px / cell_w_px),
            f: cy * (1.0 - cos) - cx * sin * (cell_w_px / cell_h_px),
        }
    }

    /// Axis-aligned scale about the element-local cell point
    /// `(cx, cy)`. Cell-size independent (axis-aligned scales commute
    /// with the cell scaling, §9.11).
    pub fn scale_about(sx: f32, sy: f32, cx: f32, cy: f32) -> Self {
        Transform {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: cx * (1.0 - sx),
            f: cy * (1.0 - sy),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct DecodeError(pub u16);

impl DecodeError {
    pub const fn bad_payload() -> Self {
        DecodeError(ERR_BAD_PAYLOAD)
    }
}

impl From<DecodeError> for u16 {
    fn from(e: DecodeError) -> u16 {
        e.0
    }
}

pub type DecodeResult<T> = Result<T, DecodeError>;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn take(&mut self, n: usize) -> DecodeResult<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(DecodeError::bad_payload());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> DecodeResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> DecodeResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> DecodeResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i32(&mut self) -> DecodeResult<i32> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> DecodeResult<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn i64(&mut self) -> DecodeResult<i64> {
        let b = self.take(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn bool(&mut self) -> DecodeResult<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn f32(&mut self) -> DecodeResult<f32> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// LEB128 unsigned varint (§1.4 `varu`).
    pub fn varu(&mut self) -> DecodeResult<u64> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            let val = (b & 0x7F) as u64;
            if shift >= 64 {
                return Err(DecodeError::bad_payload());
            }
            result |= val
                .checked_shl(shift)
                .ok_or(DecodeError::bad_payload())?;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    pub fn bytes(&mut self) -> DecodeResult<&'a [u8]> {
        let len = self.varu()? as usize;
        self.take(len)
    }

    pub fn string(&mut self) -> DecodeResult<&'a str> {
        let raw = self.bytes()?;
        std::str::from_utf8(raw).map_err(|_| DecodeError::bad_payload())
    }

    pub fn point(&mut self) -> DecodeResult<Point> {
        let x = self.f32()?;
        let y = self.f32()?;
        Ok(Point { x, y })
    }

    pub fn rect(&mut self) -> DecodeResult<Rect> {
        let x = self.f32()?;
        let y = self.f32()?;
        let w = self.f32()?;
        let h = self.f32()?;
        Ok(Rect { x, y, w, h })
    }

    pub fn transform(&mut self) -> DecodeResult<Transform> {
        let a = self.f32()?;
        let b = self.f32()?;
        let c = self.f32()?;
        let d = self.f32()?;
        let e = self.f32()?;
        let f = self.f32()?;
        Ok(Transform { a, b, c, d, e, f })
    }
}

pub struct Writer {
    pub buf: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    pub fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn varu(&mut self, mut v: u64) {
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

    pub fn bytes(&mut self, b: &[u8]) {
        self.varu(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    pub fn transform(&mut self, t: &Transform) {
        self.f32(t.a);
        self.f32(t.b);
        self.f32(t.c);
        self.f32(t.d);
        self.f32(t.e);
        self.f32(t.f);
    }
}

/// Byte-stuff a payload for the APC envelope body. `ESC` is doubled
/// (`ESC ESC`); the transport-hostile bytes `~`, XON and XOFF are each
/// replaced with `ESC <mark>`, so the emitted body is safe to cross an
/// interactive relay (e.g. ssh) that would otherwise interpret them.
/// Decoding (the APC parser) reverses all four cases.
pub fn stuff(input: &[u8], out: &mut Vec<u8>) {
    for &b in input {
        match b {
            ESC => {
                out.push(ESC);
                out.push(ESC);
            }
            TILDE => {
                out.push(ESC);
                out.push(ESC_MARK_TILDE);
            }
            XON => {
                out.push(ESC);
                out.push(ESC_MARK_XON);
            }
            XOFF => {
                out.push(ESC);
                out.push(ESC_MARK_XOFF);
            }
            _ => out.push(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varu_roundtrip() {
        for &v in &[0u64, 1, 0x7F, 0x80, 0x3FFF, 0x4000, 1 << 20, 1 << 35, u64::MAX] {
            let mut w = Writer::new();
            w.varu(v);
            let mut r = Reader::new(&w.buf);
            assert_eq!(r.varu().unwrap(), v);
            assert!(r.at_end());
        }
    }

    #[test]
    fn primitive_roundtrip() {
        let mut w = Writer::new();
        w.u8(0xAB);
        w.u16(0xBEEF);
        w.u32(0xDEAD_BEEF);
        w.f32(-1.5);
        w.str("hello");
        w.bytes(&[1, 2, 3]);

        let mut r = Reader::new(&w.buf);
        assert_eq!(r.u8().unwrap(), 0xAB);
        assert_eq!(r.u16().unwrap(), 0xBEEF);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.f32().unwrap(), -1.5);
        assert_eq!(r.string().unwrap(), "hello");
        assert_eq!(r.bytes().unwrap(), &[1, 2, 3]);
        assert!(r.at_end());
    }

    #[test]
    fn truncated_buffer() {
        let mut r = Reader::new(&[0x01]);
        assert!(r.u32().is_err());
    }

    #[test]
    fn stuff_doubles_esc() {
        let mut out = Vec::new();
        stuff(&[0x00, 0x1B, 0xFF, 0x1B], &mut out);
        assert_eq!(out, vec![0x00, 0x1B, 0x1B, 0xFF, 0x1B, 0x1B]);
    }

    #[test]
    fn stuff_escapes_transport_hostile_bytes() {
        let mut out = Vec::new();
        // ~, XON (0x11), XOFF (0x13) each become ESC <mark>.
        stuff(&[TILDE, XON, XOFF], &mut out);
        assert_eq!(
            out,
            vec![ESC, ESC_MARK_TILDE, ESC, ESC_MARK_XON, ESC, ESC_MARK_XOFF]
        );
    }

    #[test]
    fn stuffed_output_is_transport_clean() {
        // A stuffed body never contains a literal `~`, XON or XOFF — so
        // `\n~` (ssh's escape trigger) can't appear and flow-control bytes
        // can't pause an interactive relay. Exhaustive over every byte
        // value, plus a newline-adjacency probe.
        let all: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let mut out = Vec::new();
        stuff(&all, &mut out);
        assert!(!out.contains(&TILDE), "literal ~ leaked");
        assert!(!out.contains(&XON), "literal XON leaked");
        assert!(!out.contains(&XOFF), "literal XOFF leaked");
        for w in out.windows(2) {
            assert!(
                !((w[0] == b'\n' || w[0] == b'\r') && w[1] == TILDE),
                "newline-adjacent ~ leaked"
            );
        }
    }
}
