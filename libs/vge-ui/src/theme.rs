//! The accent palette every client's chrome keys off.
//!
//! Three sources, in priority order:
//!
//! 1. a CLI override ([`set_cli_accent`]) — lets an outer session give a
//!    nested one a distinct chrome color;
//! 2. the host's themed `host.accent` ([`set_host_accent`]), surfaced
//!    through the PRT probe (VGE §7.3), which tracks veter's theme and
//!    the pane's nesting depth;
//! 3. the app's compiled-in brand color ([`set_brand`], defaulting to
//!    vmux's `#56799f`).
//!
//! When the host themes `host.*` and there is no CLI override,
//! [`accent_style`] emits a `Style::Ref("host.accent")` so the host
//! resolves the color itself; [`accent_color`] reports the concrete
//! value that ref resolves to, so locally-derived shades (translucent
//! thumbs, darkened surfaces) match the ref'd chrome exactly.
//!
//! All state is process-global and set once at startup, keeping the
//! accent out of every render signature.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vge_protocol::command::{Color, Style};

/// Reserved host-provided accent style id (VGE spec §7.3). Resolves to
/// veter's configured accent for the client's nesting depth.
pub const HOST_ACCENT_STYLE_ID: &str = "host.accent";

/// Default brand color (`#56799f`) — a muted blue, distinctive against
/// the terminal background. Used when neither the host nor the CLI
/// supplies an accent.
pub const COLOR_BRAND: Color = Color {
    r: 0x56 as f32 / 255.0,
    g: 0x79 as f32 / 255.0,
    b: 0x9f as f32 / 255.0,
    a: 1.0,
};

/// Modal background: a dark, mostly-opaque base for legibility — a modal
/// can sit over arbitrary shell text. Tinted slightly toward
/// [`COLOR_BRAND`] so it reads as part of the same palette instead of
/// looking like a leftover from another design.
///
/// The `COLOR_*` constants are the *untethered* look — what a client
/// paints on a terminal that publishes no `host.*` theme. Read them
/// through the accessors below ([`modal_bg`] and friends), which
/// prefer the host's palette when there is one.
pub const COLOR_MODAL_BG: Color = Color {
    r: 0.09,
    g: 0.06,
    b: 0.16,
    a: 0.96,
};
pub const COLOR_MODAL_TEXT: Color = Color {
    r: 0.96,
    g: 0.96,
    b: 0.98,
    a: 1.0,
};
/// Dimmed secondary text — picker hints, inactive tab labels.
pub const COLOR_DIM_TEXT: Color = Color {
    r: 0.55,
    g: 0.58,
    b: 0.65,
    a: 1.0,
};
/// Foreground over an accent fill — selected picker rows, active tabs.
pub const COLOR_ACTIVE_TEXT: Color = Color {
    r: 0.98,
    g: 0.94,
    b: 0.85,
    a: 1.0,
};
/// Primary chrome text over a surface fill.
pub const COLOR_TITLE_TEXT: Color = Color {
    r: 0.92,
    g: 0.94,
    b: 0.98,
    a: 1.0,
};
pub const COLOR_SCROLLBAR: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 0.35,
};

/// How opaque a modal's ground is over the shell text behind it. The
/// host publishes an opaque surface — it paints its own panels on the
/// terminal background — so a client that adopts it re-applies this.
const MODAL_ALPHA: f32 = 0.96;

/// Opacity of the scrollbar thumb, which is [`host_fg`] at a fraction
/// rather than a colour of its own.
const SCROLLBAR_ALPHA: f32 = 0.35;

/// The non-accent half of the host's `host.*` palette (VGE §7.3), in
/// the order the PRT probe reports it — see
/// `veter_host::vge::HostThemeColors::IDS`.
#[derive(Debug, Clone, Copy)]
pub struct HostColors {
    pub bg: Color,
    pub fg: Color,
    pub surface: Color,
    pub surface_inset: Color,
    pub text: Color,
    pub text_dim: Color,
    pub text_on_accent: Color,
    pub warn: Color,
}

impl HostColors {
    /// Rebuild from the eight straight-RGBA8 quads the probe carries.
    pub fn from_rgba8(quads: [[u8; 4]; 8]) -> Self {
        let c = |[r, g, b, a]: [u8; 4]| Color {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: f32::from(a) / 255.0,
        };
        Self {
            bg: c(quads[0]),
            fg: c(quads[1]),
            surface: c(quads[2]),
            surface_inset: c(quads[3]),
            text: c(quads[4]),
            text_dim: c(quads[5]),
            text_on_accent: c(quads[6]),
            warn: c(quads[7]),
        }
    }
}

/// Opacity of the translucent accent used behind title text and
/// selected rows.
const THUMB_ALPHA: f32 = 0.35;

/// The app's compiled-in fallback accent, packed `0xRRGGBBAA`.
static BRAND_RGBA: AtomicU32 = AtomicU32::new(0x56_79_9f_ff);

/// Straight RGBA8 accent the host reported for this client's nesting
/// depth. Valid only when `HOST_THEMED` is set; it is the concrete value
/// `host.accent` resolves to.
static HOST_RGBA: AtomicU32 = AtomicU32::new(0);
static HOST_THEMED: AtomicBool = AtomicBool::new(false);

static CLI_RGBA: AtomicU32 = AtomicU32::new(0);
static CLI_SET: AtomicBool = AtomicBool::new(false);

/// The host's non-accent palette, packed `0xRRGGBBAA` in
/// [`HostColors`] field order. Valid only when `HOST_COLORS_SET` is
/// set; a host that themes accents but publishes no colours leaves the
/// client on the `COLOR_*` constants.
static HOST_COLORS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
static HOST_COLORS_SET: AtomicBool = AtomicBool::new(false);

/// Pack a color into `0xRRGGBBAA`.
pub fn pack(c: Color) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (q(c.r) << 24) | (q(c.g) << 16) | (q(c.b) << 8) | q(c.a)
}

/// Unpack a `0xRRGGBBAA` value into a normalized [`Color`].
pub fn unpack(rgba: u32) -> Color {
    let [r, g, b, a] = rgba.to_be_bytes();
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    }
}

/// Override the compiled-in fallback accent. Call before any render if
/// the app's own brand differs from [`COLOR_BRAND`].
pub fn set_brand(c: Color) {
    BRAND_RGBA.store(pack(c), Ordering::Relaxed);
}

/// Adopt the host's themed accent (from the PRT probe). Chrome accents
/// then reference `host.accent` so they follow veter's theme and signal
/// nesting depth.
pub fn set_host_accent(c: Color) {
    HOST_RGBA.store(pack(c), Ordering::Relaxed);
    HOST_THEMED.store(true, Ordering::Relaxed);
}

/// True once [`set_host_accent`] has been called.
pub fn host_themed() -> bool {
    HOST_THEMED.load(Ordering::Relaxed)
}

/// Adopt the rest of the host's palette (from the PRT probe's theme
/// block). Every accessor below then paints in the terminal's own
/// colours instead of this crate's built-in dark chrome — which is
/// what keeps a client's modal from looking like a hole punched in a
/// light or a warm-toned terminal.
///
/// Independent of [`set_host_accent`]: a host may publish accents and
/// nothing else, and the accent is the half a client cannot do
/// without.
pub fn set_host_theme(c: HostColors) {
    let packed = [
        c.bg,
        c.fg,
        c.surface,
        c.surface_inset,
        c.text,
        c.text_dim,
        c.text_on_accent,
        c.warn,
    ];
    for (slot, color) in HOST_COLORS.iter().zip(packed) {
        slot.store(pack(color), Ordering::Relaxed);
    }
    HOST_COLORS_SET.store(true, Ordering::Relaxed);
}

/// One slot of the host palette, or `None` when the host published
/// none. Indices follow [`HostColors`]'s field order.
fn host_color(i: usize) -> Option<Color> {
    HOST_COLORS_SET
        .load(Ordering::Relaxed)
        .then(|| unpack(HOST_COLORS[i].load(Ordering::Relaxed)))
}

/// The terminal's default background, when the host publishes it.
/// A client with a ground of its own to fill wants this; one drawing
/// over the shell should stay transparent instead.
pub fn host_bg() -> Option<Color> {
    host_color(0)
}

/// The terminal's default foreground, when the host publishes it.
pub fn host_fg() -> Option<Color> {
    host_color(1)
}

/// Ground for a modal or dialog: the host's panel surface at
/// [`MODAL_ALPHA`], else the accent-tinted [`COLOR_MODAL_BG`].
pub fn modal_bg() -> Color {
    match host_color(2) {
        Some(c) => Color {
            a: MODAL_ALPHA,
            ..c
        },
        None => tinted_modal_bg(),
    }
}

/// A recessed surface inside a modal — an input field, an unselected
/// segment. The host's, else the modal ground darkened.
pub fn inset_bg() -> Color {
    match host_color(3) {
        Some(c) => Color {
            a: MODAL_ALPHA,
            ..c
        },
        None => darken(modal_bg(), 0.35),
    }
}

/// Primary text over [`modal_bg`].
pub fn modal_text() -> Color {
    host_color(4).unwrap_or(COLOR_MODAL_TEXT)
}

/// Primary chrome text over a surface fill — pane titles, tab labels.
/// The same colour as [`modal_text`] under a host theme; the built-in
/// pair are two slightly different whites.
pub fn title_text() -> Color {
    host_color(4).unwrap_or(COLOR_TITLE_TEXT)
}

/// Dimmed secondary text — picker hints, inactive tab labels.
pub fn dim_text() -> Color {
    host_color(5).unwrap_or(COLOR_DIM_TEXT)
}

/// Foreground over an accent fill — selected picker rows, active tabs.
pub fn active_text() -> Color {
    host_color(6).unwrap_or(COLOR_ACTIVE_TEXT)
}

/// The warm tone for a destructive answer or an error line.
pub fn warn_color() -> Option<Color> {
    host_color(7)
}

/// Scrollbar thumb: the terminal's foreground at [`SCROLLBAR_ALPHA`],
/// so it reads against whatever the track is drawn over.
pub fn scrollbar() -> Color {
    match host_fg() {
        Some(c) => Color {
            a: SCROLLBAR_ALPHA,
            ..c
        },
        None => COLOR_SCROLLBAR,
    }
}

/// Force a concrete accent, overriding both the host's and the brand's.
pub fn set_cli_accent(c: Color) {
    CLI_RGBA.store(pack(c), Ordering::Relaxed);
    CLI_SET.store(true, Ordering::Relaxed);
}

/// The accent as a concrete color, following the priority order above.
/// Used to derive shades the host does not publish as their own styles.
pub fn accent_color() -> Color {
    if CLI_SET.load(Ordering::Relaxed) {
        unpack(CLI_RGBA.load(Ordering::Relaxed))
    } else if HOST_THEMED.load(Ordering::Relaxed) {
        unpack(HOST_RGBA.load(Ordering::Relaxed))
    } else {
        unpack(BRAND_RGBA.load(Ordering::Relaxed))
    }
}

/// Accent fill/stroke style for chrome — a host `StyleRef` when the host
/// themes `host.*` and nothing overrides it, else a concrete color.
pub fn accent_style() -> Style {
    if CLI_SET.load(Ordering::Relaxed) {
        Style::Flat(accent_color())
    } else if HOST_THEMED.load(Ordering::Relaxed) {
        Style::Ref(HOST_ACCENT_STYLE_ID.to_string())
    } else {
        Style::Flat(accent_color())
    }
}

/// Translucent accent — the thumb behind title text and the highlight
/// behind a selected list row.
pub fn title_thumb_style() -> Style {
    Style::Flat(Color {
        a: THUMB_ALPHA,
        ..accent_color()
    })
}

/// Surface fill for modal/dialog backgrounds.
///
/// The host's own panel surface when it publishes one — a modal then
/// belongs to the same palette as the terminal behind it. Otherwise the
/// accent scaled toward black, so light foreground text stays legible
/// over arbitrary shell content, and [`COLOR_MODAL_BG`] untinted when
/// there is no accent either.
pub fn surface_style() -> Style {
    Style::Flat(modal_bg())
}

/// The accent-tinted fallback ground, for a terminal that publishes no
/// `host.*` colours.
fn tinted_modal_bg() -> Color {
    if CLI_SET.load(Ordering::Relaxed) || HOST_THEMED.load(Ordering::Relaxed) {
        let c = accent_color();
        const K: f32 = 0.20;
        Color {
            r: c.r * K,
            g: c.g * K,
            b: c.b * K,
            a: MODAL_ALPHA,
        }
    } else {
        COLOR_MODAL_BG
    }
}

/// A muted accent — 40% darker — for secondary markers that would
/// otherwise read as the same color as a full-accent badge. Derived as a
/// concrete `Style::Flat` rather than the `Style::Ref` [`accent_style`]
/// emits, since a host style ref cannot be shaded locally.
pub fn activity_style() -> Style {
    Style::Flat(darken(accent_color(), 0.4))
}

/// A color darkened by `amount` (0..1) — each channel scaled toward
/// black, hue and saturation preserved. Alpha is untouched.
pub fn darken(c: Color, amount: f32) -> Color {
    let k = 1.0 - amount;
    Color {
        r: c.r * k,
        g: c.g * k,
        b: c.b * k,
        a: c.a,
    }
}

/// Parse an accent spec: a named color, `#rgb`, `#rrggbb`, or
/// `#rrggbbaa` (the `#` is optional). Returns the packed `0xRRGGBBAA`.
pub fn parse_accent_color(s: &str) -> Result<u32, String> {
    let t = s.trim();
    if let Some(rgba) = named_color(&t.to_ascii_lowercase()) {
        return Ok(rgba);
    }
    let hex = t.strip_prefix('#').unwrap_or(t);
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("not a color name or hex value: {s}"));
    }
    let v = |i: usize, n: usize| u32::from_str_radix(&hex[i..i + n], 16).unwrap_or(0);
    let expand = |x: u32| x * 17; // 0xF → 0xFF
    Ok(match hex.len() {
        3 => (expand(v(0, 1)) << 24) | (expand(v(1, 1)) << 16) | (expand(v(2, 1)) << 8) | 0xFF,
        6 => (v(0, 2) << 24) | (v(2, 2) << 16) | (v(4, 2) << 8) | 0xFF,
        8 => (v(0, 2) << 24) | (v(2, 2) << 16) | (v(4, 2) << 8) | v(6, 2),
        _ => return Err(format!("hex color must be 3, 6, or 8 digits: {s}")),
    })
}

/// The handful of names accepted alongside hex specs. `blue` is the
/// brand color itself, so `--accent blue` is the default made explicit.
fn named_color(name: &str) -> Option<u32> {
    Some(match name {
        "red" => 0xd0_5c_5c_ff,
        "green" => 0x5c_a0_5c_ff,
        "blue" => 0x56_79_9f_ff,
        "yellow" => 0xc9_a8_4c_ff,
        "orange" => 0xcf_7d_3c_ff,
        "magenta" | "pink" => 0xb0_5c_9f_ff,
        "cyan" | "teal" => 0x4c_9f_9f_ff,
        "purple" | "violet" => 0x8c_6c_c0_ff,
        "white" | "gray" | "grey" => 0x9a_9a_9a_ff,
        _ => return None,
    })
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

    #[test]
    fn darken_scales_channels_toward_black() {
        let c = Color {
            r: 0.5,
            g: 0.4,
            b: 0.2,
            a: 0.8,
        };
        let d = darken(c, 0.4); // 40% darker → channels * 0.6
        assert!((d.r - 0.3).abs() < 1e-6);
        assert!((d.g - 0.24).abs() < 1e-6);
        assert!((d.b - 0.12).abs() < 1e-6);
        assert_eq!(d.a, c.a, "alpha is untouched");
        assert_eq!(darken(c, 0.0), c);
        let black = darken(c, 1.0);
        assert_eq!((black.r, black.g, black.b), (0.0, 0.0, 0.0));
    }

    #[test]
    fn parse_accent_color_variants() {
        assert_eq!(parse_accent_color("blue").unwrap(), 0x56_79_9f_ff);
        assert_eq!(parse_accent_color("#ff8800").unwrap(), 0xff_88_00_ff);
        assert_eq!(parse_accent_color("ff8800").unwrap(), 0xff_88_00_ff);
        assert_eq!(parse_accent_color("#f80").unwrap(), 0xff_88_00_ff);
        assert_eq!(parse_accent_color("#11223344").unwrap(), 0x11_22_33_44);
        assert!(parse_accent_color("nope").is_err());
        assert!(parse_accent_color("#12345").is_err());
        assert!(parse_accent_color("#zz0000").is_err());
    }

    /// With nothing configured the accent is the brand, and
    /// `accent_style` hands back a concrete color rather than a ref the
    /// host would not resolve.
    #[test]
    fn default_accent_is_the_brand() {
        assert_eq!(accent_color(), COLOR_BRAND);
        assert_eq!(accent_style(), Style::Flat(COLOR_BRAND));
    }
}
