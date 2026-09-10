use crate::callbacks::DynamicColor;

const BASE64: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
const CLIPBOARD_SELECTOR: &[u8] = b"cpqs01234567";

pub struct WrappedScreen<CB: crate::callbacks::Callbacks = ()> {
    pub screen: crate::screen::Screen,
    pub callbacks: CB,
}

impl WrappedScreen<()> {
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self::new_with_callbacks(rows, cols, scrollback_len, ())
    }
}

impl<CB: crate::callbacks::Callbacks> WrappedScreen<CB> {
    pub fn new_with_callbacks(
        rows: u16,
        cols: u16,
        scrollback_len: usize,
        callbacks: CB,
    ) -> Self {
        Self {
            screen: crate::screen::Screen::new(
                crate::grid::Size { rows, cols },
                scrollback_len,
            ),
            callbacks,
        }
    }
}

impl<CB: crate::callbacks::Callbacks> vte::Perform for WrappedScreen<CB> {
    fn print(&mut self, c: char) {
        if c == '\u{fffd}' || ('\u{80}'..'\u{a0}').contains(&c) {
            self.callbacks.unhandled_char(&mut self.screen, c);
        } else {
            self.screen.text(c);
        }
    }

    fn execute(&mut self, b: u8) {
        match b {
            7 => self.callbacks.audible_bell(&mut self.screen),
            8 => self.screen.bs(),
            9 => self.screen.tab(),
            10 => self.screen.lf(),
            11 => self.screen.vt(),
            12 => self.screen.ff(),
            13 => self.screen.cr(),
            // SO (0x0E) / SI (0x0F) — flip GL between G1 and G0 so DEC
            // line-drawing programs (jtop, dialog, ncurses TUIs) render
            // box characters instead of leaking raw `qx j k l m` ASCII.
            14 => self.screen.shift_out(),
            15 => self.screen.shift_in(),
            _ => self.callbacks.unhandled_control(&mut self.screen, b),
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, b: u8) {
        if let Some(i) = intermediates.first() {
            // ESC ( <c> / ESC ) <c> / ESC * <c> / ESC + <c> — SCS, designate
            // G0..G3. Followups with a second intermediate (96-char sets,
            // DOCS, etc.) are still left to the unhandled callback.
            if intermediates.len() == 1 {
                let selector = match *i {
                    b'(' => Some(0),
                    b')' => Some(1),
                    b'*' => Some(2),
                    b'+' => Some(3),
                    _ => None,
                };
                if let Some(sel) = selector {
                    self.screen.designate_charset(sel, b);
                    return;
                }
            }
            self.callbacks.unhandled_escape(
                &mut self.screen,
                Some(*i),
                intermediates.get(1).copied(),
                b,
            );
        } else {
            match b {
                b'7' => self.screen.decsc(),
                b'8' => self.screen.decrc(),
                b'=' => self.screen.deckpam(),
                b'>' => self.screen.deckpnm(),
                b'M' => self.screen.ri(),
                b'D' => self.screen.ind(),
                b'E' => self.screen.nel(),
                b'c' => self.screen.ris(),
                b'g' => self.callbacks.visual_bell(&mut self.screen),
                // ST. vte reports the terminator of a string sequence
                // (OSC, DCS, APC) here once the sequence itself is
                // dispatched; consuming it keeps a perfectly ordinary
                // `OSC … ST` from being logged as an unhandled escape.
                b'\\' => {}
                _ => {
                    self.callbacks.unhandled_escape(
                        &mut self.screen,
                        None,
                        None,
                        b,
                    );
                }
            }
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        c: char,
    ) {
        let unhandled = |screen: &mut crate::screen::Screen| {
            self.callbacks.unhandled_csi(
                screen,
                intermediates.first().copied(),
                intermediates.get(1).copied(),
                &params.iter().collect::<Vec<_>>(),
                c,
            );
        };
        match intermediates.first() {
            None => match c {
                '@' => self.screen.ich(canonicalize_params_1(params, 1)),
                'A' => self.screen.cuu(canonicalize_params_1(params, 1)),
                'B' => self.screen.cud(canonicalize_params_1(params, 1)),
                'C' => self.screen.cuf(canonicalize_params_1(params, 1)),
                'D' => self.screen.cub(canonicalize_params_1(params, 1)),
                'E' => self.screen.cnl(canonicalize_params_1(params, 1)),
                'F' => self.screen.cpl(canonicalize_params_1(params, 1)),
                'G' => self.screen.cha(canonicalize_params_1(params, 1)),
                // HVP (CSI f) addresses the cursor exactly as CUP
                // (CSI H) does — apt's fancy progress bar parks its
                // status line with `CSI <rows> ; 0 f`, and with the
                // sequence unhandled the bar landed wherever the
                // cursor already was, then scrolled up with the text.
                'H' | 'f' => {
                    self.screen.cup(canonicalize_params_2(params, 1, 1))
                }
                'I' => self.screen.cht(canonicalize_params_1(params, 1)),
                'J' => self
                    .screen
                    .ed(canonicalize_params_1(params, 0), unhandled),
                'K' => self
                    .screen
                    .el(canonicalize_params_1(params, 0), unhandled),
                'L' => self.screen.il(canonicalize_params_1(params, 1)),
                'M' => self.screen.dl(canonicalize_params_1(params, 1)),
                'P' => self.screen.dch(canonicalize_params_1(params, 1)),
                'S' => self.screen.su(canonicalize_params_1(params, 1)),
                'T' => self.screen.sd(canonicalize_params_1(params, 1)),
                'X' => self.screen.ech(canonicalize_params_1(params, 1)),
                'Z' => self.screen.cbt(canonicalize_params_1(params, 1)),
                // SM / RM — the non-private half of the mode family
                // (IRM, LNM). The `?` spellings are handled below.
                'h' => self.screen.sm(params, unhandled),
                'l' => self.screen.rm(params, unhandled),
                'b' => self.screen.rep(canonicalize_params_1(params, 1)),
                'd' => self.screen.vpa(canonicalize_params_1(params, 1)),
                'm' => self.screen.sgr(params, unhandled),
                // SCOSC / SCORC, the ANSI.SYS spelling of DECSC /
                // DECRC. Unambiguous here because this parser has no
                // left/right margins, so CSI s is never DECSLRM.
                's' => self.screen.decsc(),
                'u' => self.screen.decrc(),
                'r' => self.screen.decstbm(canonicalize_params_decstbm(
                    params,
                    self.screen.grid().size(),
                )),
                't' => {
                    let mut params_iter = params.iter();
                    let op =
                        params_iter.next().and_then(|x| x.first().copied());
                    match op {
                        Some(8) => {
                            let (screen_rows, screen_cols) =
                                self.screen.size();
                            let rows =
                                params_iter.next().map_or(screen_rows, |x| {
                                    *x.first().unwrap_or(&screen_rows)
                                });
                            let cols =
                                params_iter.next().map_or(screen_cols, |x| {
                                    *x.first().unwrap_or(&screen_cols)
                                });
                            self.callbacks
                                .resize(&mut self.screen, (rows, cols));
                        }
                        // 14 — text area in pixels, 16 — cell size in
                        // pixels, 18 — text area in cells. The first two
                        // are the renderer's to answer; 18 the screen
                        // could answer itself, but all three go out the
                        // same door so one responder owns the format.
                        Some(op @ (14 | 16 | 18)) => {
                            self.callbacks
                                .report_window_size(&mut self.screen, op);
                        }
                        // 22 / 23 — push and pop the window title.
                        // terminfo's `smcup` and `rmcup` for
                        // xterm-256color carry these, which is how a
                        // full-screen program's title survives it.
                        Some(22) => {
                            self.callbacks.push_window_title(&mut self.screen);
                        }
                        Some(23) => {
                            self.callbacks.pop_window_title(&mut self.screen);
                        }
                        _ => {
                            self.callbacks.unhandled_csi(
                                &mut self.screen,
                                None,
                                None,
                                &params.iter().collect::<Vec<_>>(),
                                c,
                            );
                        }
                    }
                }
                _ => {
                    self.callbacks.unhandled_csi(
                        &mut self.screen,
                        None,
                        None,
                        &params.iter().collect::<Vec<_>>(),
                        c,
                    );
                }
            },
            Some(b'?') => match c {
                'J' => self
                    .screen
                    .decsed(canonicalize_params_1(params, 0), unhandled),
                'K' => self
                    .screen
                    .decsel(canonicalize_params_1(params, 0), unhandled),
                'h' => self.screen.decset(params, unhandled),
                'l' => self.screen.decrst(params, unhandled),
                _ => {
                    self.callbacks.unhandled_csi(
                        &mut self.screen,
                        Some(b'?'),
                        intermediates.get(1).copied(),
                        &params.iter().collect::<Vec<_>>(),
                        c,
                    );
                }
            },
            // The kitty keyboard protocol's stack operations. All three
            // share the `u` final byte with SCORC (`CSI u`), which is
            // why they are reached only through a private prefix —
            // parameterless `CSI u` above stays DECRC.
            Some(b'>') if c == 'u' => {
                self.screen.kbd_push(canonicalize_params_1(params, 0));
            }
            Some(b'<') if c == 'u' => {
                self.screen.kbd_pop(canonicalize_params_1(params, 1));
            }
            Some(b'=') if c == 'u' => {
                let (flags, mode) = canonicalize_params_2(params, 0, 1);
                self.screen.kbd_set(flags, mode);
            }
            // DECSTR, soft reset. terminfo's `is2` / `rs2` lead with
            // it, so it arrives at the start of essentially every
            // ncurses program.
            Some(b'!') if c == 'p' => self.screen.decstr(),
            // DECSCUSR, cursor style (terminfo `Ss` / `Se`).
            Some(b' ') if c == 'q' => {
                self.screen.decscusr(canonicalize_params_1(params, 1));
            }
            Some(i) => {
                self.callbacks.unhandled_csi(
                    &mut self.screen,
                    Some(*i),
                    intermediates.get(1).copied(),
                    &params.iter().collect::<Vec<_>>(),
                    c,
                );
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bel_terminated: bool) {
        match params {
            [b"0", s] => {
                self.callbacks.set_window_icon_name(&mut self.screen, s);
                self.callbacks.set_window_title(&mut self.screen, s);
            }
            [b"1", s] => {
                self.callbacks.set_window_icon_name(&mut self.screen, s);
            }
            [b"2", s] => {
                self.callbacks.set_window_title(&mut self.screen, s);
            }
            [b"52", ty, data] => {
                match (
                    ty.iter().all(|c| CLIPBOARD_SELECTOR.contains(c)),
                    *data,
                ) {
                    (true, b"?") => {
                        self.callbacks
                            .paste_from_clipboard(&mut self.screen, ty);
                    }
                    (true, data)
                        if data.iter().all(|c| BASE64.contains(c)) =>
                    {
                        self.callbacks.copy_to_clipboard(
                            &mut self.screen,
                            ty,
                            data,
                        );
                    }
                    _ => {
                        self.callbacks
                            .unhandled_osc(&mut self.screen, params);
                    }
                }
            }
            // OSC 4 — palette entries, as `index ; spec` pairs
            // (terminfo `initc`). `?` in place of a spec is a query.
            [b"4", rest @ ..] if !rest.is_empty() => {
                let mut handled = true;
                for pair in rest.chunks(2) {
                    match (pair.first().and_then(|p| parse_u8(p)), pair.get(1))
                    {
                        (Some(idx), Some(&b"?")) => {
                            self.callbacks
                                .query_palette_color(&mut self.screen, idx);
                        }
                        (Some(idx), Some(spec)) => {
                            self.callbacks.set_palette_color(
                                &mut self.screen,
                                idx,
                                spec,
                            );
                        }
                        _ => handled = false,
                    }
                }
                if !handled {
                    self.callbacks.unhandled_osc(&mut self.screen, params);
                }
            }
            // OSC 104 — reset the whole palette (terminfo `oc`), or
            // just the named entries.
            [b"104"] => {
                self.callbacks.reset_palette_color(&mut self.screen, None);
            }
            [b"104", rest @ ..] => {
                for p in rest {
                    if let Some(idx) = parse_u8(p) {
                        self.callbacks
                            .reset_palette_color(&mut self.screen, Some(idx));
                    }
                }
            }
            // OSC 10 / 11 / 12 — the dynamic colours. xterm lets one
            // command carry several, each naming the colour after the
            // last, so `OSC 10 ; fg ; bg ST` sets both.
            [sel @ (b"10" | b"11" | b"12"), rest @ ..]
                if !rest.is_empty() =>
            {
                let base = match *sel {
                    b"10" => 0,
                    b"11" => 1,
                    _ => 2,
                };
                for (i, spec) in rest.iter().enumerate() {
                    let Some(which) = dynamic_color(base + i) else {
                        break;
                    };
                    if *spec == b"?" {
                        self.callbacks
                            .query_dynamic_color(&mut self.screen, which);
                    } else {
                        self.callbacks.set_dynamic_color(
                            &mut self.screen,
                            which,
                            spec,
                        );
                    }
                }
            }
            // OSC 110 / 111 / 112 — and their resets (terminfo `Cr`).
            [b"110"] => self
                .callbacks
                .reset_dynamic_color(&mut self.screen, DynamicColor::Foreground),
            [b"111"] => self
                .callbacks
                .reset_dynamic_color(&mut self.screen, DynamicColor::Background),
            [b"112"] => self
                .callbacks
                .reset_dynamic_color(&mut self.screen, DynamicColor::Cursor),
            _ => {
                self.callbacks.unhandled_osc(&mut self.screen, params);
            }
        }
    }
}

/// The `n`th dynamic colour counting from OSC 10, for the chained
/// `OSC 10 ; fg ; bg` form.
fn dynamic_color(n: usize) -> Option<DynamicColor> {
    match n {
        0 => Some(DynamicColor::Foreground),
        1 => Some(DynamicColor::Background),
        2 => Some(DynamicColor::Cursor),
        // 13 and up (pointer foreground, highlight, …) exist but
        // nothing here can act on them.
        _ => None,
    }
}

/// Parse an OSC parameter as a palette index.
fn parse_u8(p: &[u8]) -> Option<u8> {
    std::str::from_utf8(p).ok()?.parse().ok()
}

fn canonicalize_params_1(params: &vte::Params, default: u16) -> u16 {
    let first = params.iter().next().map_or(0, |x| *x.first().unwrap_or(&0));
    if first == 0 {
        default
    } else {
        first
    }
}

fn canonicalize_params_2(
    params: &vte::Params,
    default1: u16,
    default2: u16,
) -> (u16, u16) {
    let mut iter = params.iter();
    let first = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let first = if first == 0 { default1 } else { first };

    let second = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let second = if second == 0 { default2 } else { second };

    (first, second)
}

fn canonicalize_params_decstbm(
    params: &vte::Params,
    size: crate::grid::Size,
) -> (u16, u16) {
    let mut iter = params.iter();
    let top = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let top = if top == 0 { 1 } else { top };

    let bottom = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let bottom = if bottom == 0 { size.rows } else { bottom };

    (top, bottom)
}
