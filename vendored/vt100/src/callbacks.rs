/// Which of the terminal's "dynamic" colours an OSC 10 / 11 / 12 names
/// — and, for the 110 / 111 / 112 resets, which one goes back to its
/// default.
///
/// The `?` query form of each is the one that matters most: vim's
/// `t_RB` and neovim's background detection both send `OSC 11 ; ? ST`
/// at startup and *wait* for the answer, exactly as they wait for DA1.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DynamicColor {
    /// OSC 10 / 110 — the default foreground.
    Foreground,
    /// OSC 11 / 111 — the default background.
    Background,
    /// OSC 12 / 112 — the cursor (terminfo `Cs` / `Cr`).
    Cursor,
}

/// This trait is used by the parser to handle extra escape sequences that
/// don't have an impact on the terminal screen directly.
pub trait Callbacks {
    /// This callback is called when the terminal requests an audible bell
    /// (typically with `^G`).
    fn audible_bell(&mut self, _: &mut crate::Screen) {}
    /// This callback is called when the terminal requests a visual bell
    /// (typically with `\eg`).
    fn visual_bell(&mut self, _: &mut crate::Screen) {}
    /// This callback is called when the terminal requests a resize
    /// (typically with `\e[8;<rows>;<cols>t`).
    fn resize(&mut self, _: &mut crate::Screen, _request: (u16, u16)) {}
    /// This callback is called when the terminal requests the window title
    /// to be set (typically with `\e]1;<icon_name>\a`)
    fn set_window_icon_name(
        &mut self,
        _: &mut crate::Screen,
        _icon_name: &[u8],
    ) {
    }
    /// This callback is called when the terminal requests the window title
    /// to be set (typically with `\e]2;<title>\a`)
    fn set_window_title(&mut self, _: &mut crate::Screen, _title: &[u8]) {}
    /// This callback is called when the terminal requests data to be copied
    /// to the system clipboard (typically with `\e]52;<ty>;<data>\a`). Note
    /// that `data` will be encoded as base64.
    fn copy_to_clipboard(
        &mut self,
        _: &mut crate::Screen,
        _ty: &[u8],
        _data: &[u8],
    ) {
    }
    /// This callback is called when the terminal requests data to be pasted
    /// from the system clipboard (typically with `\e]52;<ty>;?\a`).
    fn paste_from_clipboard(&mut self, _: &mut crate::Screen, _ty: &[u8]) {}
    /// This callback is called when the terminal receives an escape sequence
    /// which is otherwise not implemented.
    fn unhandled_char(&mut self, _: &mut crate::Screen, _c: char) {}
    /// This callback is called when the terminal receives a control
    /// character which is otherwise not implemented.
    fn unhandled_control(&mut self, _: &mut crate::Screen, _b: u8) {}
    /// This callback is called when the terminal receives an escape sequence
    /// which is otherwise not implemented.
    fn unhandled_escape(
        &mut self,
        _: &mut crate::Screen,
        _i1: Option<u8>,
        _i2: Option<u8>,
        _b: u8,
    ) {
    }
    /// This callback is called when the terminal receives a CSI sequence
    /// (`\e[`) which is otherwise not implemented.
    fn unhandled_csi(
        &mut self,
        _: &mut crate::Screen,
        _i1: Option<u8>,
        _i2: Option<u8>,
        _params: &[&[u16]],
        _c: char,
    ) {
    }
    /// This callback is called when the terminal receives a OSC sequence
    /// (`\e]`) which is otherwise not implemented.
    fn unhandled_osc(&mut self, _: &mut crate::Screen, _params: &[&[u8]]) {}

    /// `CSI 22 t` / `CSI 23 t` — push the window title onto the
    /// terminal's own stack, and pop it back off. `smcup` and `rmcup`
    /// for xterm-256color carry these, which is how a full-screen
    /// program's title change is undone when it exits.
    fn push_window_title(&mut self, _: &mut crate::Screen) {}
    /// See [`push_window_title`](Self::push_window_title).
    fn pop_window_title(&mut self, _: &mut crate::Screen) {}

    /// `CSI 14 t` (text area in pixels), `CSI 16 t` (cell size in
    /// pixels) or `CSI 18 t` (text area in cells), passed as the
    /// leading parameter. The sender blocks on the reply.
    fn report_window_size(&mut self, _: &mut crate::Screen, _op: u16) {}

    /// `OSC 4 ; <index> ; <spec>` — redefine one palette entry
    /// (terminfo `initc`). `spec` is an X colour specification, most
    /// often `rgb:RR/GG/BB` or `#RRGGBB`.
    fn set_palette_color(
        &mut self,
        _: &mut crate::Screen,
        _index: u8,
        _spec: &[u8],
    ) {
    }
    /// `OSC 4 ; <index> ; ?` — report one palette entry.
    fn query_palette_color(&mut self, _: &mut crate::Screen, _index: u8) {}
    /// `OSC 104` — reset one palette entry, or the whole palette when
    /// the index is `None` (terminfo `oc`).
    fn reset_palette_color(
        &mut self,
        _: &mut crate::Screen,
        _index: Option<u8>,
    ) {
    }

    /// `OSC 10 / 11 / 12 ; <spec>` — set a dynamic colour.
    fn set_dynamic_color(
        &mut self,
        _: &mut crate::Screen,
        _which: DynamicColor,
        _spec: &[u8],
    ) {
    }
    /// `OSC 10 / 11 / 12 ; ?` — report one. The sender blocks on it.
    fn query_dynamic_color(
        &mut self,
        _: &mut crate::Screen,
        _which: DynamicColor,
    ) {
    }
    /// `OSC 110 / 111 / 112` — put one back to its default.
    fn reset_dynamic_color(
        &mut self,
        _: &mut crate::Screen,
        _which: DynamicColor,
    ) {
    }
}

impl Callbacks for () {}
