use unicode_width::UnicodeWidthChar as _;

// chosen to make the size of the cell struct 32 bytes
const CONTENT_BYTES: usize = 22;

const IS_WIDE: u8 = 0b1000_0000;
const IS_WIDE_CONTINUATION: u8 = 0b0100_0000;
const LEN_BITS: u8 = 0b0001_1111;

/// Represents a single terminal cell.
#[derive(Clone, Debug, Eq)]
pub struct Cell {
    contents: [u8; CONTENT_BYTES],
    len: u8,
    attrs: crate::attrs::Attrs,
}
const _: () = assert!(std::mem::size_of::<Cell>() == 32);

impl PartialEq<Self> for Cell {
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        if self.attrs != other.attrs {
            return false;
        }
        let len = self.len();
        self.contents[..len] == other.contents[..len]
    }
}

impl Cell {
    pub(crate) fn new() -> Self {
        Self {
            contents: Default::default(),
            len: 0,
            attrs: crate::attrs::Attrs::default(),
        }
    }

    fn len(&self) -> usize {
        usize::from(self.len & LEN_BITS)
    }

    pub(crate) fn serialize_binary(&self, w: &mut crate::snapshot::Writer) {
        // `self.len` carries both the wide-flag bits (top 2) and the
        // UTF-8 content length (bottom 5). Encoding the byte verbatim
        // preserves the flags; we then emit only the live content
        // bytes — the rest of `contents` is padding.
        w.u8(self.len);
        let content_len = (self.len & LEN_BITS) as usize;
        w.buf.extend_from_slice(&self.contents[..content_len]);
        crate::snapshot::encode_attrs(w, &self.attrs);
    }

    pub(crate) fn deserialize_binary(
        r: &mut crate::snapshot::Reader,
    ) -> Result<Self, crate::snapshot::SnapshotError> {
        let len = r.u8()?;
        let content_len = (len & LEN_BITS) as usize;
        if content_len > CONTENT_BYTES {
            return Err(crate::snapshot::SnapshotError::bad_payload(
                "cell content length exceeds CONTENT_BYTES",
            ));
        }
        let content_bytes = r.take(content_len)?;
        let mut contents = [0u8; CONTENT_BYTES];
        contents[..content_len].copy_from_slice(content_bytes);
        let attrs = crate::snapshot::decode_attrs(r)?;
        Ok(Self {
            contents,
            len,
            attrs,
        })
    }

    pub(crate) fn set(&mut self, c: char, a: crate::attrs::Attrs) {
        self.len = 0;
        self.append_char(0, c);
        // strings in this context should always be an arbitrary character
        // followed by zero or more zero-width characters, so we should only
        // have to look at the first character
        self.set_wide(c.width().unwrap_or(1) > 1);
        self.attrs = a;
    }

    pub(crate) fn append(&mut self, c: char) {
        let len = self.len();
        if len >= CONTENT_BYTES - 4 {
            return;
        }
        if len == 0 {
            self.contents[0] = b' ';
            self.len += 1;
        }

        // we already checked that we have space for another codepoint
        self.append_char(self.len(), c);
    }

    // Writes bytes representing c at start
    // Requires caller to verify start <= CODEPOINTS_IN_CELL * 4
    fn append_char(&mut self, start: usize, c: char) {
        c.encode_utf8(&mut self.contents[start..]);
        self.len += u8::try_from(c.len_utf8()).unwrap();
    }

    pub(crate) fn clear(&mut self, attrs: crate::attrs::Attrs) {
        self.len = 0;
        self.attrs = attrs;
    }

    /// Returns the text contents of the cell.
    ///
    /// Can include multiple unicode characters if combining characters are
    /// used, but will contain at most one character with a non-zero character
    /// width.
    // Since contents has been constructed by appending chars encoded as UTF-8 it will be valid UTF-8
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn contents(&self) -> &str {
        std::str::from_utf8(&self.contents[..self.len()]).unwrap()
    }

    /// Returns whether the cell contains any text data.
    #[must_use]
    pub fn has_contents(&self) -> bool {
        self.len() > 0
    }

    /// Returns whether the text data in the cell represents a wide character.
    #[must_use]
    pub fn is_wide(&self) -> bool {
        self.len & IS_WIDE != 0
    }

    /// Returns whether the cell contains the second half of a wide character
    /// (in other words, whether the previous cell in the row contains a wide
    /// character)
    #[must_use]
    pub fn is_wide_continuation(&self) -> bool {
        self.len & IS_WIDE_CONTINUATION != 0
    }

    fn set_wide(&mut self, wide: bool) {
        if wide {
            self.len |= IS_WIDE;
        } else {
            self.len &= !IS_WIDE;
        }
    }

    pub(crate) fn set_wide_continuation(&mut self, wide: bool) {
        if wide {
            self.len |= IS_WIDE_CONTINUATION;
        } else {
            self.len &= !IS_WIDE_CONTINUATION;
        }
    }

    pub(crate) fn attrs(&self) -> &crate::attrs::Attrs {
        &self.attrs
    }

    /// Returns the foreground color of the cell.
    #[must_use]
    pub fn fgcolor(&self) -> crate::Color {
        self.attrs.fgcolor
    }

    /// Returns the background color of the cell.
    #[must_use]
    pub fn bgcolor(&self) -> crate::Color {
        self.attrs.bgcolor
    }

    /// Returns whether the cell should be rendered with the bold text
    /// attribute.
    #[must_use]
    pub fn bold(&self) -> bool {
        self.attrs.bold()
    }

    /// Returns whether the cell should be rendered with the dim text
    /// attribute.
    #[must_use]
    pub fn dim(&self) -> bool {
        self.attrs.dim()
    }

    /// Returns whether the cell should be rendered with the italic text
    /// attribute.
    #[must_use]
    pub fn italic(&self) -> bool {
        self.attrs.italic()
    }

    /// Returns whether the cell should be rendered with the underlined text
    /// attribute.
    #[must_use]
    pub fn underline(&self) -> bool {
        self.attrs.underline()
    }

    /// Returns whether the cell should be rendered with the inverse text
    /// attribute.
    #[must_use]
    pub fn inverse(&self) -> bool {
        self.attrs.inverse()
    }
}
