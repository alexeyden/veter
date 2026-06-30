// Binary primitives shared by all VSS frame bodies (§1.4 of the
// extension specs — identical to PRT/VGE/VFT).

use super::frame::{
    ERR_BAD_PAYLOAD, ESC, ESC_MARK_TILDE, ESC_MARK_XON, ESC_MARK_XOFF, TILDE, XOFF, XON,
};

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
        w.u64(0x0123_4567_89AB_CDEF);
        w.i64(-1);
        w.str("hello");
        w.bytes(&[1, 2, 3]);

        let mut r = Reader::new(&w.buf);
        assert_eq!(r.u8().unwrap(), 0xAB);
        assert_eq!(r.u16().unwrap(), 0xBEEF);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u64().unwrap(), 0x0123_4567_89AB_CDEF);
        assert_eq!(r.i64().unwrap(), -1);
        assert_eq!(r.string().unwrap(), "hello");
        assert_eq!(r.bytes().unwrap(), &[1, 2, 3]);
        assert!(r.at_end());
    }

    #[test]
    fn truncated_buffer() {
        let mut r = Reader::new(&[0x01]);
        assert!(r.u32().is_err());
        let mut r = Reader::new(&[0x01, 0x02, 0x03, 0x04]);
        assert!(r.u64().is_err());
    }

    #[test]
    fn empty_string_decodes() {
        let mut r = Reader::new(&[0x00]);
        assert_eq!(r.string().unwrap(), "");
        assert!(r.at_end());
    }

    #[test]
    fn invalid_utf8_string_errors() {
        let mut r = Reader::new(&[0x02, 0xFF, 0xFE]);
        assert!(r.string().is_err());
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
