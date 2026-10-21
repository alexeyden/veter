//! Colour themes: the grid palette, the chrome, and the accents the
//! host publishes to in-terminal clients.
//!
//! A published colourscheme gives you an ANSI-16 table plus a
//! background and a foreground. It does not give you "the fill of a
//! recessed chip in a search panel". So a [`Theme`] carries the small
//! set a scheme actually publishes and *derives* the rest — every
//! derived value overridable, which is what lets the built-in default
//! reproduce the exact colours veter used before this module existed
//! while the four scheme themes spell out only their own palette.
//!
//! Where each field lands:
//!
//!  * `ansi` / `background` / `foreground` / `cursor` / `selection_*`
//!    back [`crate::renderer::Palette`] — the base an `OSC 4 / 10 / 11
//!    / 12` override sits on top of, and what `OSC 104 / 110 / 111 /
//!    112` resets back to.
//!  * `accents` is published into the reserved `host.*` VGE style
//!    namespace (`doc/vector-graphics-extension.md` §7.3) and reported
//!    in the PRT probe, so `vmux` and friends draw their chrome from
//!    the same palette.
//!  * the surface / text / warn group is veter's own overlay chrome
//!    (the search panel, the close prompt) *and* the rest of the
//!    `host.*` namespace, so a client's modal belongs to the same
//!    palette as the terminal behind it.
//!
//! Themes come from three places, resolved by [`resolve`]: the
//! built-in table below, a user file at `<config>/themes/<name>.toml`,
//! and per-key overrides in the config's own `[theme]` section. All
//! three share one schema — a user file is a `Theme` with the keys it
//! cares about, merged onto an unpinned base (see [`load_file`]).

use std::path::Path;

use femtovg::Color;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

/// A straight (non-premultiplied) 8-bit RGBA color, read from and
/// written as a `#rrggbb` or `#rrggbbaa` hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Parse `#rrggbb` / `#rrggbbaa`; the `#` is optional.
    pub fn parse(s: &str) -> Result<Self, String> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("invalid hex color '{s}'"));
        }
        let byte = |i: usize| {
            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| format!("invalid hex color '{s}'"))
        };
        match hex.len() {
            6 => Ok(Self {
                r: byte(0)?,
                g: byte(2)?,
                b: byte(4)?,
                a: 255,
            }),
            8 => Ok(Self {
                r: byte(0)?,
                g: byte(2)?,
                b: byte(4)?,
                a: byte(6)?,
            }),
            _ => Err(format!("color '{s}' must be #rrggbb or #rrggbbaa")),
        }
    }

    /// femtovg color for GUI chrome painting.
    #[must_use]
    pub fn to_femto(self) -> Color {
        Color::rgba(self.r, self.g, self.b, self.a)
    }

    /// VGE protocol color (f32, straight alpha) for the `host.*` styles.
    #[must_use]
    pub fn to_command_color(self) -> crate::vge::Color {
        crate::vge::Color {
            r: self.r as f32 / 255.0,
            g: self.g as f32 / 255.0,
            b: self.b as f32 / 255.0,
            a: self.a as f32 / 255.0,
        }
    }

    /// Straight RGBA8 quad, the shape the PRT probe reports.
    #[must_use]
    pub fn to_rgba8(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// Blend toward `to` by `amount` (`0.0`..=`1.0`). Alpha comes from
    /// `self`: mixing is for picking a shade, never for fading one out.
    #[must_use]
    pub fn mix(self, to: Rgba, amount: f32) -> Self {
        let t = amount.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Self {
            r: lerp(self.r, to.r),
            g: lerp(self.g, to.g),
            b: lerp(self.b, to.b),
            a: self.a,
        }
    }

    /// Each channel scaled toward black; hue and saturation preserved.
    #[must_use]
    pub fn darken(self, amount: f32) -> Self {
        self.mix(Rgba::rgb(0, 0, 0), amount)
    }

    /// Blended toward white.
    #[must_use]
    pub fn lighten(self, amount: f32) -> Self {
        self.mix(Rgba::rgb(255, 255, 255), amount)
    }

    /// The grey of the same [`Self::luminance`]: the shade with the hue
    /// taken out.
    #[must_use]
    pub fn grey(self) -> Self {
        let v = (self.luminance() * 255.0).round() as u8;
        Self { r: v, g: v, b: v, a: self.a }
    }

    /// sRGB-weighted relative luminance, `0.0`..=`1.0`. Used to pick
    /// the higher-contrast text colour over a fill.
    #[must_use]
    pub fn luminance(self) -> f32 {
        (0.299 * self.r as f32 + 0.587 * self.g as f32 + 0.114 * self.b as f32) / 255.0
    }
}

impl Serialize for Rgba {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let Self { r, g, b, a } = *self;
        if a == 255 {
            s.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}"))
        } else {
            s.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}{a:02x}"))
        }
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Rgba::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A colour list that also takes a bare colour as a list of one — how
/// `text_on_accent` reads both its per-slot form and the single pin it
/// was before it had slots, which existing configs still spell.
fn one_or_many<'de, D>(deserializer: D) -> Result<Vec<Rgba>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Rgba),
        Many(Vec<Rgba>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(c) => vec![c],
        OneOrMany::Many(v) => v,
    })
}

/// An accent slot as a theme spells it: a colour, or the name of one of
/// its [`Theme::swatches`]. A `#` marks the colour, so a swatch name can
/// never be misread as hex — `"#bad"` is a colour, `"bad"` a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accent {
    Color(Rgba),
    Swatch(String),
}

impl From<Rgba> for Accent {
    fn from(c: Rgba) -> Self {
        Self::Color(c)
    }
}

impl Serialize for Accent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Color(c) => c.serialize(s),
            Self::Swatch(name) => s.serialize_str(name),
        }
    }
}

impl<'de> Deserialize<'de> for Accent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.starts_with('#') {
            Rgba::parse(&s).map(Self::Color).map_err(serde::de::Error::custom)
        } else if veter_host::vge::is_swatch_name(&s) {
            Ok(Self::Swatch(s))
        } else {
            Err(serde::de::Error::custom(format!(
                "accent '{s}' is neither a #hex colour nor a swatch name"
            )))
        }
    }
}

/// A swatch as a theme spells it: the fill alone (its text derives), or
/// the fill with the text the theme picked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SwatchSpec {
    Fill(Rgba),
    Pinned {
        fill: Rgba,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<Rgba>,
    },
}

impl SwatchSpec {
    fn fill(self) -> Rgba {
        match self {
            Self::Fill(fill) | Self::Pinned { fill, .. } => fill,
        }
    }

    fn text(self) -> Option<Rgba> {
        match self {
            Self::Fill(_) => None,
            Self::Pinned { text, .. } => text,
        }
    }
}

/// A resolved swatch: the fill and the text that goes on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swatch {
    pub name: String,
    pub fill: Rgba,
    pub text: Rgba,
}

/// The swatch names every theme answers to, in publishing order — the
/// colour words a client's `--accent` takes. A theme may add more.
pub const SWATCH_NAMES: [&str; 8] = [
    "red", "orange", "yellow", "green", "cyan", "blue", "magenta", "purple",
];

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// One colour theme.
///
/// Only `ansi`, `background` and `foreground` are required — a scheme
/// always publishes those. Every `Option` field is a *pin*: `Some`
/// takes the value as given, `None` derives it from the required three
/// (see the accessor next to each). The built-in default pins
/// everything, so a config with no `[theme]` section is identical to
/// veter before themes existed.
#[derive(Debug, Clone, Serialize, Deserialize)]
// Missing keys fall back to the *unpinned* base, not to `Default`: a
// pin left unset is skipped when [`merge`] round-trips a theme through
// TOML (which has no null), so reading it back with veter's pins as
// the fallback turned every scheme's derived surfaces into veter's
// dark ones the moment any `[theme]` key was overridden.
#[serde(default = "scheme_base")]
pub struct Theme {
    /// The ANSI-16 table. Indices 16..=255 are the standard 6×6×6 cube
    /// and greyscale ramp, computed rather than stored.
    pub ansi: [Rgba; 16],
    pub background: Rgba,
    pub foreground: Rgba,
    /// `None` keeps veter's built-in cursor: the cell in reverse video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Rgba>,
    /// `None` keeps the built-in selection: the cell in reverse video.
    /// Setting only `selection_bg` paints that behind the cell and
    /// leaves the character its own colour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_bg: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_fg: Option<Rgba>,

    /// Ordered accent palette; slot N is `host.accent.{N+1}`, and the
    /// contextual `host.accent` rotates through it by portal nesting
    /// depth. Each entry is a `#hex` colour or a swatch name. Empty
    /// derives blue / magenta / cyan from `ansi`.
    pub accents: Vec<Accent>,

    /// Named colours, each with the text that sits on it — published as
    /// `host.swatch.<name>`, which is what `vmux --accent orange` looks
    /// up so a nested multiplexer paints this theme's orange rather
    /// than one of its own. Merged by name over the set derived from
    /// `ansi` (see [`Theme::swatches`]), so a theme pins only the names
    /// it cares about.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub swatches: std::collections::BTreeMap<String, SwatchSpec>,

    /// Overlay panel fill. Derives as the background lifted a tenth of
    /// the way toward the foreground.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<Rgba>,
    /// A recessed surface inside a panel — the query field, a chip that
    /// is switched off. Derives as the background darkened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_inset: Option<Rgba>,
    /// Primary text on `surface`. Derives as the foreground.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<Rgba>,
    /// Secondary text — key hints, inactive labels. Derives as `text`
    /// faded toward `surface`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_dim: Option<Rgba>,
    /// Text over each accent fill, slot for slot with `accents`: entry N
    /// is the text for accent slot N+1. A slot the list does not reach
    /// derives, to whichever of near-white and near-black contrasts
    /// better with *that* accent — so pinning one colour pins slot 1
    /// alone, and a light accent further down still gets dark text.
    /// A single string is read as a one-entry list.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "one_or_many"
    )]
    pub text_on_accent: Vec<Rgba>,

    /// The warm trio for the answer that gets you nothing — the close
    /// prompt's Quit button, the search panel's `no matches`. All three
    /// derive from the red pair in `ansi`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warn_fill: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warn_border: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warn_text: Option<Rgba>,

    /// Search-panel chrome tint. Derives to accent slot 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_accent: Option<Rgba>,
    /// Query text in the search panel. Derives to `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_text: Option<Rgba>,
    /// The active match (the one `n`/`N` navigates to). Derives from
    /// the bright yellow in `ansi`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_current_match: Option<Rgba>,
    /// All other matches. Derives as a dark shade of the same yellow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_match: Option<Rgba>,
}

impl Default for Theme {
    fn default() -> Self {
        veter()
    }
}

impl Theme {
    /// Grid colour for an ANSI index: the theme's own table for 0..=15,
    /// then [`indexed_extended`].
    #[must_use]
    pub fn indexed(&self, idx: u8) -> Rgba {
        match idx {
            0..=15 => self.ansi[idx as usize],
            _ => indexed_extended(idx),
        }
    }

    /// The accent palette, guaranteed non-empty: an unset `accents`
    /// derives blue / magenta / cyan from the bright half of `ansi`.
    ///
    /// Deliberately not the green. Slot 2 is the accent a program one
    /// level in gets — anything launched inside a multiplexer, which is
    /// to say nearly everything — so it is the one on screen most of
    /// the time, and green is both the odd hue out against the cool
    /// palettes most schemes have and perceptually the brightest, so it
    /// reads louder than slot 1 at the same nominal saturation. Blue,
    /// magenta and cyan stay near one another in weight.
    #[must_use]
    pub fn accents(&self) -> Vec<Rgba> {
        self.accent_slots().into_iter().map(|s| s.fill).collect()
    }

    /// The accent palette with each slot's name and text: a slot that
    /// names a swatch is that swatch, text included; a literal colour
    /// has no name. A name the theme has no swatch for is dropped
    /// ([`resolve`] says so once), and a list that drops to nothing
    /// derives as if unset. A `text_on_accent` pin beats the swatch's
    /// own text.
    #[must_use]
    pub fn accent_slots(&self) -> Vec<Swatch> {
        let swatches = self.swatches();
        let mut slots: Vec<Swatch> = self
            .accents
            .iter()
            .filter_map(|a| match a {
                Accent::Color(fill) => Some(Swatch {
                    name: String::new(),
                    fill: *fill,
                    text: self.text_on(*fill),
                }),
                Accent::Swatch(name) => swatches.iter().find(|s| &s.name == name).cloned(),
            })
            .collect();
        if slots.is_empty() {
            slots = [12, 13, 14]
                .map(|i| Swatch {
                    name: String::new(),
                    fill: self.ansi[i],
                    text: self.text_on(self.ansi[i]),
                })
                .to_vec();
        }
        for (slot, pin) in slots.iter_mut().zip(&self.text_on_accent) {
            slot.text = *pin;
        }
        slots
    }

    /// Say once, at load, what [`Self::accent_slots`] and
    /// [`Self::swatches`] will quietly skip.
    fn warn_unresolved(&self) {
        let swatches = self.swatches();
        for a in &self.accents {
            if let Accent::Swatch(name) = a
                && !swatches.iter().any(|s| &s.name == name)
            {
                eprintln!("veter: config: accent '{name}' names no swatch; skipping it");
            }
        }
        for name in self.swatches.keys() {
            if !veter_host::vge::is_swatch_name(name) {
                eprintln!(
                    "veter: config: swatch '{name}' is not a plain name \
                     (letters, digits, '-', '_', at most 32); skipping it"
                );
            }
        }
    }

    /// The theme's named swatches: one per [`SWATCH_NAMES`] entry,
    /// derived from the bright half of `ansi` (orange between its red
    /// and yellow, purple between its blue and magenta), with every
    /// name in `swatches` laid over — so a scheme file that sets only
    /// its palette still answers `--accent orange`, and a theme that
    /// pins its own orange is not second-guessed. Text derives against
    /// each fill unless pinned.
    #[must_use]
    pub fn swatches(&self) -> Vec<Swatch> {
        let a = &self.ansi;
        let derived = [
            a[9],
            a[9].mix(a[11], 0.5),
            a[11],
            a[10],
            a[14],
            a[12],
            a[13],
            a[12].mix(a[13], 0.5),
        ];
        let mut out: Vec<Swatch> = SWATCH_NAMES
            .iter()
            .zip(derived)
            .map(|(name, fill)| Swatch {
                name: (*name).to_string(),
                fill,
                text: self.text_on(fill),
            })
            .collect();
        for (name, spec) in &self.swatches {
            if !veter_host::vge::is_swatch_name(name) {
                continue;
            }
            let swatch = Swatch {
                name: name.clone(),
                fill: spec.fill(),
                text: spec.text().unwrap_or_else(|| self.text_on(spec.fill())),
            };
            match out.iter_mut().find(|s| &s.name == name) {
                Some(slot) => *slot = swatch,
                None => out.push(swatch),
            }
        }
        out
    }

    /// Accent slot 1 — the one veter's own chrome and the grid
    /// selection outline key off.
    #[must_use]
    pub fn accent_primary(&self) -> Rgba {
        self.accents()[0]
    }

    /// Whether the ground is lighter than the ink. Several derivations
    /// have a direction — text over a fill has to move *away* from it,
    /// and which way that is depends on the theme, not on a constant.
    #[must_use]
    pub fn is_light(&self) -> bool {
        self.background.luminance() > self.foreground.luminance()
    }

    /// The darker of the ground and the ink, and the lighter. Deriving
    /// a near-black or a near-white from these rather than from
    /// `background` / `foreground` by name is what makes the results
    /// hold on a light theme: there the roles are swapped, and
    /// "lighten the foreground" only reaches a mid grey.
    #[must_use]
    fn darkest(&self) -> Rgba {
        if self.is_light() {
            self.foreground
        } else {
            self.background
        }
    }

    #[must_use]
    fn lightest(&self) -> Rgba {
        if self.is_light() {
            self.background
        } else {
            self.foreground
        }
    }

    #[must_use]
    pub fn surface(&self) -> Rgba {
        self.surface
            .unwrap_or_else(|| self.background.mix(self.foreground, 0.10))
    }

    #[must_use]
    pub fn surface_inset(&self) -> Rgba {
        self.surface_inset
            .unwrap_or_else(|| self.background.darken(0.25))
    }

    #[must_use]
    pub fn text(&self) -> Rgba {
        self.text.unwrap_or(self.foreground)
    }

    #[must_use]
    pub fn text_dim(&self) -> Rgba {
        self.text_dim
            .unwrap_or_else(|| self.text().mix(self.surface(), 0.45))
    }

    /// Text over an accent fill. Derived by contrast rather than by
    /// "always white": a light accent (nord's frost, gruvbox's yellow)
    /// needs dark text on it, and a dark one needs light. Both poles
    /// come from the theme's own extremes, so a light theme — where the
    /// foreground is the dark one — gets a near-white here rather than
    /// the mid grey that lightening its foreground would give.
    #[must_use]
    pub fn text_on_accent(&self) -> Rgba {
        self.accent_slots()[0].text
    }

    /// Text over an arbitrary solid `fill`, by the rule
    /// [`Self::text_on_accent`] derives with. For a fill that is not the
    /// primary accent — `[search] accent` set apart from the theme's —
    /// where the pinned or derived `text_on_accent` was never picked
    /// against what is painted.
    ///
    /// The poles keep the theme's lightness but not its hue: a light
    /// scheme's ink is often a saturated colour of its own (tokyonight-
    /// day's is the blue of its first accent), and text in that hue over
    /// a different accent reads as the wrong accent rather than as text.
    #[must_use]
    pub fn text_on(&self, fill: Rgba) -> Rgba {
        if fill.luminance() > 0.55 {
            self.darkest().darken(0.35).grey()
        } else {
            self.lightest().lighten(0.5).grey()
        }
    }

    /// Text over [`Self::search_accent`]. The theme's own
    /// `text_on_accent` while the two accents agree, so a pin still
    /// applies; picked against the search accent once it differs.
    #[must_use]
    pub fn text_on_search_accent(&self, search_accent: Rgba) -> Rgba {
        if search_accent == self.accent_primary() {
            self.text_on_accent()
        } else {
            self.text_on(search_accent)
        }
    }

    /// A warm tint of the background rather than a shade of red: the
    /// fill has to read as part of the panel it sits on, with the
    /// border and the label carrying the warning.
    #[must_use]
    pub fn warn_fill(&self) -> Rgba {
        self.warn_fill
            .unwrap_or_else(|| self.background.mix(self.ansi[1], 0.18))
    }

    #[must_use]
    pub fn warn_border(&self) -> Rgba {
        self.warn_border
            .unwrap_or_else(|| self.ansi[1].mix(self.ansi[9], 0.5).darken(0.3))
    }

    /// The label on the warm chip. It has to move away from
    /// [`Self::warn_fill`], which is a tint of the ground — so on a
    /// light theme that means darkening the red, not lightening it.
    #[must_use]
    pub fn warn_text(&self) -> Rgba {
        self.warn_text.unwrap_or_else(|| {
            if self.is_light() {
                self.ansi[1].darken(0.35)
            } else {
                self.ansi[9].lighten(0.35)
            }
        })
    }

    #[must_use]
    pub fn search_accent(&self) -> Rgba {
        self.search_accent.unwrap_or_else(|| self.accent_primary())
    }

    #[must_use]
    pub fn search_text(&self) -> Rgba {
        self.search_text.unwrap_or_else(|| self.text())
    }

    /// Both match colours are cell *backgrounds*, painted under text
    /// that keeps its own foreground — so they derive by tinting the
    /// background toward the theme's yellow rather than by taking that
    /// yellow as-is. A scheme with a pastel yellow (catppuccin's
    /// `#f9e2af`) would otherwise highlight matched text in something
    /// as light as the text itself.
    #[must_use]
    pub fn search_current_match(&self) -> Rgba {
        self.search_current_match
            .unwrap_or_else(|| self.background.mix(self.ansi[11], 0.55))
    }

    /// All other matches: the same tint, weaker, off the normal yellow.
    #[must_use]
    pub fn search_match(&self) -> Rgba {
        self.search_match
            .unwrap_or_else(|| self.background.mix(self.ansi[3], 0.35))
    }

    /// The theme as the host palette published into the reserved
    /// `host.*` VGE style namespace (VGE §7.3) and reported in the PRT
    /// probe (`doc/portal-extension.md` §10).
    #[must_use]
    pub fn host_palette(&self) -> veter_host::vge::HostThemePalette {
        let publish = |s: Swatch| veter_host::vge::HostSwatch {
            name: s.name,
            fill: s.fill.to_command_color(),
            text: s.text.to_command_color(),
        };
        veter_host::vge::HostThemePalette {
            accents: self.accent_slots().into_iter().map(publish).collect(),
            swatches: self.swatches().into_iter().map(publish).collect(),
            colors: Some(veter_host::vge::HostThemeColors {
                bg: self.background.to_command_color(),
                fg: self.foreground.to_command_color(),
                surface: self.surface().to_command_color(),
                surface_inset: self.surface_inset().to_command_color(),
                text: self.text().to_command_color(),
                text_dim: self.text_dim().to_command_color(),
                text_on_accent: self.text_on_accent().to_command_color(),
                warn: self.warn_border().to_command_color(),
            }),
        }
    }
}

/// The part of the 256-colour table no theme owns: the standard 6×6×6
/// colour cube (16..=231) and the 24-step greyscale ramp (232..=255).
/// Every terminal agrees on these, and a program that indexes into them
/// is naming a colour by its coordinates, not asking for a palette
/// entry. Panics below 16, which the callers handle themselves.
#[must_use]
pub fn indexed_extended(idx: u8) -> Rgba {
    match idx {
        0..=15 => unreachable!("indices below 16 belong to the theme's own table"),
        16..=231 => {
            let idx = idx - 16;
            let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            Rgba::rgb(level(idx / 36), level((idx / 6) % 6), level(idx % 6))
        }
        232..=255 => {
            let v = (idx - 232) * 10 + 8;
            Rgba::rgb(v, v, v)
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Names the built-in table answers to, in the order `veter --help`
/// and `assets/config.toml` list them.
pub const BUILTIN_NAMES: [&str; 7] = [
    "veter",
    "tokyonight-storm",
    "tokyonight-day",
    "catppuccin-mocha",
    "catppuccin-latte",
    "gruvbox-dark",
    "nord",
];

/// A built-in theme by name, or `None` if the name is not one of
/// [`BUILTIN_NAMES`] — in which case it is a user file.
#[must_use]
pub fn builtin(name: &str) -> Option<Theme> {
    match name {
        "veter" | "default" => Some(veter()),
        "tokyonight-storm" | "tokyonight" => Some(tokyonight_storm()),
        "tokyonight-day" => Some(tokyonight_day()),
        "catppuccin-mocha" | "catppuccin" => Some(catppuccin_mocha()),
        "catppuccin-latte" => Some(catppuccin_latte()),
        "gruvbox-dark" | "gruvbox" => Some(gruvbox_dark()),
        "nord" => Some(nord()),
        _ => None,
    }
}

/// Resolve the config's `[theme]` section into a concrete theme.
///
/// `name` picks the base — a built-in, else `<config_dir>/themes/<name>.toml`
/// — and every other key in the section is a per-field override on top
/// of it. Anything that goes wrong (unknown name, unreadable file, bad
/// colour) logs one line and falls back, the same way [`crate::renderer`]
/// consumers expect a broken config never to stop veter from starting.
#[must_use]
pub fn resolve(section: &toml::Table, config_dir: Option<&Path>) -> Theme {
    let base = match section.get("name").and_then(toml::Value::as_str) {
        None => Theme::default(),
        Some(name) => builtin(name)
            .or_else(|| load_file(config_dir, name))
            .unwrap_or_else(|| {
                eprintln!(
                    "veter: config: unknown theme '{name}'; known: {}",
                    BUILTIN_NAMES.join(", ")
                );
                Theme::default()
            }),
    };
    let overrides: toml::Table = section
        .iter()
        .filter(|(k, _)| k.as_str() != "name")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let theme = merge(base, &overrides, "config [theme]");
    theme.warn_unresolved();
    theme
}

/// Load `<config_dir>/themes/<name>.toml` as a theme: the keys it sets,
/// merged onto [`scheme_base`] — nothing pinned, so a file carrying only
/// a scheme's palette gets surfaces, text, accents and swatches derived
/// from *that* palette, the way the scheme built-ins do. Merging onto
/// the `veter` default instead handed every such file veter's own dark
/// panels and muted accents, whatever its background.
fn load_file(config_dir: Option<&Path>, name: &str) -> Option<Theme> {
    // The name indexes a directory, so it must not be able to walk out
    // of one.
    if name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return None;
    }
    let path = config_dir?.join("themes").join(format!("{name}.toml"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("veter: theme: cannot read {}: {e}", path.display());
            return None;
        }
    };
    let table: toml::Table = match toml::from_str(&text) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("veter: theme: parse error in {}: {e}", path.display());
            return None;
        }
    };
    Some(merge(scheme_base(), &table, &path.display().to_string()))
}

/// Apply `overrides` onto `base`, key by key. Round-tripping through a
/// TOML table rather than a hand-written field-by-field merge is what
/// keeps the three sources on one schema: a field added to [`Theme`] is
/// overridable from the config section and from a user file without
/// being named anywhere else.
fn merge(base: Theme, overrides: &toml::Table, whence: &str) -> Theme {
    if overrides.is_empty() {
        return base;
    }
    let Ok(toml::Value::Table(mut table)) = toml::Value::try_from(&base) else {
        return base;
    };
    for (k, v) in overrides {
        // A table-valued key (`swatches`) merges by entry, so pinning
        // one swatch does not drop the base theme's others.
        match (table.get_mut(k), v) {
            (Some(toml::Value::Table(base)), toml::Value::Table(over)) => {
                for (name, entry) in over {
                    base.insert(name.clone(), entry.clone());
                }
            }
            _ => {
                table.insert(k.clone(), v.clone());
            }
        }
    }
    match toml::Value::Table(table).try_into() {
        Ok(theme) => theme,
        Err(e) => {
            eprintln!("veter: theme: {whence}: {e}; keeping the unmodified theme");
            base
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in themes
// ---------------------------------------------------------------------------

/// A `0xRRGGBB` literal as an opaque colour. The built-in tables below
/// are transcribed from each project's own terminal-colour file, so
/// they read in the same notation those files use.
const fn c(hex: u32) -> Rgba {
    Rgba::rgb(
        (hex >> 16) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    )
}

/// A built-in's swatches, one per [`SWATCH_NAMES`] entry in that order,
/// each taken from the scheme's own published palette rather than its
/// ANSI table — where a scheme names an orange, that is the orange.
/// Text derives against each.
fn swatch_set(fills: [u32; 8]) -> std::collections::BTreeMap<String, SwatchSpec> {
    SWATCH_NAMES
        .iter()
        .zip(fills)
        .map(|(name, hex)| ((*name).to_string(), SwatchSpec::Fill(c(hex))))
        .collect()
}

/// veter's own look, and the shape of a fully-pinned theme: every
/// derived field is spelled out, so this reproduces exactly what veter
/// painted before themes existed. The grid is the Tango palette the
/// renderer used to hardcode.
///
/// The exception is accent slots 2 and 3, which were an olive and a
/// violet: the olive was both the odd hue out against the blue and,
/// being green, perceptually brighter than it (0.55 against 0.45), so
/// a pane one level in shouted. The trio is now blue / plum / amber —
/// the blue's own hue rotated, at its own weight, so the three read as
/// a set. See [`Theme::accents`] for why slot 2 is the one that
/// matters.
fn veter() -> Theme {
    Theme {
        ansi: [
            c(0x000000),
            c(0xcc0000),
            c(0x4e9a06),
            c(0xc4a000),
            c(0x3465a4),
            c(0x75507b),
            c(0x06989a),
            c(0xd3d7cf),
            c(0x555753),
            c(0xef2929),
            c(0x8ae234),
            c(0xfce94f),
            c(0x729fcf),
            c(0xad7fa8),
            c(0x34e2e2),
            c(0xeeeeec),
        ],
        background: c(0x1e1e1e),
        foreground: c(0xcccccc),
        cursor: None,
        selection_bg: None,
        selection_fg: None,
        accents: [c(0x56799f), c(0x9f5685), c(0x9f7c56)].map(Accent::from).to_vec(),
        swatches: swatch_set([0xd05c5c, 0xcf7d3c, 0xc9a84c, 0x5ca05c, 0x4c9f9f, 0x56799f, 0xb05c9f, 0x8c6cc0]),
        surface: Some(c(0x26262a)),
        surface_inset: Some(c(0x1a1a1e)),
        text: Some(c(0xebebeb)),
        text_dim: None,
        text_on_accent: vec![c(0xf5f5f5); 3],
        warn_fill: Some(c(0x34282a)),
        warn_border: Some(c(0x964646)),
        warn_text: Some(c(0xe88282)),
        search_accent: None,
        search_text: Some(c(0xe6e6e6)),
        search_current_match: Some(c(0xdca000)),
        search_match: Some(c(0x50501e)),
    }
}

/// folke/tokyonight.nvim, `storm` variant. Palette from the project's
/// own `extras/kitty/tokyonight_storm.conf`.
fn tokyonight_storm() -> Theme {
    Theme {
        ansi: [
            c(0x1d202f),
            c(0xf7768e),
            c(0x9ece6a),
            c(0xe0af68),
            c(0x7aa2f7),
            c(0xbb9af7),
            c(0x7dcfff),
            c(0xa9b1d6),
            c(0x414868),
            c(0xff899d),
            c(0x9fe044),
            c(0xfaba4a),
            c(0x8db0ff),
            c(0xc7a9ff),
            c(0xa4daff),
            c(0xc0caf5),
        ],
        background: c(0x24283b),
        foreground: c(0xc0caf5),
        cursor: Some(c(0xc0caf5)),
        selection_bg: Some(c(0x2e3c64)),
        selection_fg: Some(c(0xc0caf5)),
        accents: [c(0x7aa2f7), c(0xbb9af7), c(0x7dcfff)].map(Accent::from).to_vec(),
        swatches: swatch_set([0xf7768e, 0xff9e64, 0xe0af68, 0x9ece6a, 0x7dcfff, 0x7aa2f7, 0xbb9af7, 0x9d7cd8]),
        ..derived()
    }
}

/// folke/tokyonight.nvim, `day` variant — the light one. Palette from
/// the project's own `extras/kitty/tokyonight_day.conf`.
///
/// As in every published light port, `ansi` swaps the roles at the ends
/// of the table: "black" is a light grey and "white" a dark blue, so a
/// program that paints color0 as a ground and color7 as ink still comes
/// out the right way up.
fn tokyonight_day() -> Theme {
    Theme {
        ansi: [
            c(0xb4b5b9), c(0xf52a65), c(0x587539), c(0x8c6c3e),
            c(0x2e7de9), c(0x9854f1), c(0x007197), c(0x6172b0),
            c(0xa1a6c5), c(0xff4774), c(0x5c8524), c(0xa27629),
            c(0x358aff), c(0xa463ff), c(0x007ea8), c(0x3760bf),
        ],
        background: c(0xe1e2e7),
        foreground: c(0x3760bf),
        cursor: Some(c(0x3760bf)),
        selection_bg: Some(c(0xb7c1e3)),
        selection_fg: Some(c(0x3760bf)),
        accents: [c(0x2e7de9), c(0x9854f1), c(0x007197)].map(Accent::from).to_vec(),
        swatches: swatch_set([0xf52a65, 0xb15c00, 0x8c6c3e, 0x587539, 0x007197, 0x2e7de9, 0x9854f1, 0x7847bd]),
        ..derived()
    }
}

/// catppuccin, `mocha` flavour. Palette from catppuccin/alacritty.
fn catppuccin_mocha() -> Theme {
    Theme {
        ansi: [
            c(0x45475a),
            c(0xf38ba8),
            c(0xa6e3a1),
            c(0xf9e2af),
            c(0x89b4fa),
            c(0xf5c2e7),
            c(0x94e2d5),
            c(0xbac2de),
            c(0x585b70),
            c(0xf38ba8),
            c(0xa6e3a1),
            c(0xf9e2af),
            c(0x89b4fa),
            c(0xf5c2e7),
            c(0x94e2d5),
            c(0xa6adc8),
        ],
        background: c(0x1e1e2e),
        foreground: c(0xcdd6f4),
        cursor: Some(c(0xf5e0dc)),
        // `surface2`, not the port's rosewater selection: veter's selection
        // sits under ordinary shell text rather than an editor's, and a
        // near-white fill there swallows the row it highlights.
        selection_bg: Some(c(0x585b70)),
        selection_fg: None,
        accents: [c(0x89b4fa), c(0xcba6f7), c(0x94e2d5)].map(Accent::from).to_vec(),
        swatches: swatch_set([0xf38ba8, 0xfab387, 0xf9e2af, 0xa6e3a1, 0x94e2d5, 0x89b4fa, 0xf5c2e7, 0xcba6f7]),
        ..derived()
    }
}

/// catppuccin, `latte` flavour — the light one. Palette from
/// catppuccin/alacritty; see [`tokyonight_day`] on the swapped ends of
/// `ansi`.
fn catppuccin_latte() -> Theme {
    Theme {
        ansi: [
            c(0xbcc0cc), c(0xd20f39), c(0x40a02b), c(0xdf8e1d),
            c(0x1e66f5), c(0xea76cb), c(0x179299), c(0x5c5f77),
            c(0xacb0be), c(0xd20f39), c(0x40a02b), c(0xdf8e1d),
            c(0x1e66f5), c(0xea76cb), c(0x179299), c(0x6c6f85),
        ],
        background: c(0xeff1f5),
        foreground: c(0x4c4f69),
        cursor: Some(c(0xdc8a78)),
        // `surface2`, not the port's rosewater selection — the same
        // reason as mocha: a selection that loud swallows the row it
        // marks when it sits under ordinary shell text.
        selection_bg: Some(c(0xacb0be)),
        selection_fg: None,
        accents: [c(0x1e66f5), c(0x8839ef), c(0x179299)].map(Accent::from).to_vec(),
        swatches: swatch_set([0xd20f39, 0xfe640b, 0xdf8e1d, 0x40a02b, 0x179299, 0x1e66f5, 0xea76cb, 0x8839ef]),
        ..derived()
    }
}

/// morhetz/gruvbox, dark medium. Palette from the project's own
/// `colors/gruvbox.vim`; the accents are its bright aqua, pink and
/// yellow, which read better as chrome on `dark0` than the neutral
/// ones.
fn gruvbox_dark() -> Theme {
    Theme {
        ansi: [
            c(0x282828),
            c(0xcc241d),
            c(0x98971a),
            c(0xd79921),
            c(0x458588),
            c(0xb16286),
            c(0x689d6a),
            c(0xa89984),
            c(0x928374),
            c(0xfb4934),
            c(0xb8bb26),
            c(0xfabd2f),
            c(0x83a598),
            c(0xd3869b),
            c(0x8ec07c),
            c(0xebdbb2),
        ],
        background: c(0x282828),
        foreground: c(0xebdbb2),
        cursor: Some(c(0xebdbb2)),
        selection_bg: Some(c(0x504945)),
        selection_fg: None,
        accents: [c(0x83a598), c(0xd3869b), c(0xfabd2f)].map(Accent::from).to_vec(),
        swatches: swatch_set([0xfb4934, 0xfe8019, 0xfabd2f, 0xb8bb26, 0x8ec07c, 0x83a598, 0xd3869b, 0xb16286]),
        ..derived()
    }
}

/// nordtheme/nord, in the terminal mapping the project's own ports use:
/// `nord1` as black, the aurora colours for red/green/yellow/purple,
/// the frost ones for blue/cyan.
fn nord() -> Theme {
    Theme {
        ansi: [
            c(0x3b4252),
            c(0xbf616a),
            c(0xa3be8c),
            c(0xebcb8b),
            c(0x81a1c1),
            c(0xb48ead),
            c(0x88c0d0),
            c(0xe5e9f0),
            c(0x4c566a),
            c(0xbf616a),
            c(0xa3be8c),
            c(0xebcb8b),
            c(0x81a1c1),
            c(0xb48ead),
            c(0x8fbcbb),
            c(0xeceff4),
        ],
        background: c(0x2e3440),
        foreground: c(0xd8dee9),
        cursor: Some(c(0xd8dee9)),
        selection_bg: Some(c(0x434c5e)),
        selection_fg: None,
        accents: [c(0x88c0d0), c(0xb48ead), c(0x81a1c1)].map(Accent::from).to_vec(),
        swatches: swatch_set([0xbf616a, 0xd08770, 0xebcb8b, 0xa3be8c, 0x88c0d0, 0x81a1c1, 0xb48ead, 0xb48ead]),
        ..derived()
    }
}

/// What a user theme file is laid over: [`derived`], with the three
/// required fields taken from the default so a file that leaves one out
/// still has a grid to draw — and everything else free to derive.
fn scheme_base() -> Theme {
    let veter = veter();
    Theme {
        ansi: veter.ansi,
        background: veter.background,
        foreground: veter.foreground,
        ..derived()
    }
}

/// The tail every scheme theme spreads: nothing pinned, so the surface,
/// text and warn groups derive from that scheme's own palette. The
/// three required fields are placeholders each theme overwrites.
fn derived() -> Theme {
    Theme {
        ansi: [Rgba::rgb(0, 0, 0); 16],
        background: Rgba::rgb(0, 0, 0),
        foreground: Rgba::rgb(0, 0, 0),
        cursor: None,
        selection_bg: None,
        selection_fg: None,
        accents: Vec::new(),
        swatches: std::collections::BTreeMap::new(),
        surface: None,
        surface_inset: None,
        text: None,
        text_dim: None,
        text_on_accent: Vec::new(),
        warn_fill: None,
        warn_border: None,
        warn_text: None,
        search_accent: None,
        search_text: None,
        search_current_match: None,
        search_match: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parses_both_lengths_and_rejects_the_rest() {
        assert_eq!(Rgba::parse("#102030").unwrap(), Rgba::rgb(16, 32, 48));
        assert_eq!(
            Rgba::parse("102030ff").unwrap(),
            Rgba {
                r: 16,
                g: 32,
                b: 48,
                a: 255
            }
        );
        assert!(Rgba::parse("#12345").is_err());
        assert!(Rgba::parse("#gggggg").is_err());
    }

    /// The point of the default theme: veter with no `[theme]` section
    /// paints what it painted before this module existed. The one
    /// deliberate exception is accent slots 2 and 3 — see
    /// [`the_accent_trios_are_the_ones_that_were_chosen`].
    #[test]
    fn the_default_theme_is_the_pre_theme_look() {
        let t = Theme::default();
        assert_eq!(t.background, Rgba::rgb(30, 30, 30));
        assert_eq!(t.foreground, Rgba::rgb(204, 204, 204));
        assert_eq!(t.ansi[1], Rgba::rgb(204, 0, 0));
        assert_eq!(t.accent_primary(), Rgba::rgb(0x56, 0x79, 0x9f));
        assert_eq!(t.surface(), Rgba::rgb(38, 38, 42));
        assert_eq!(t.surface_inset(), Rgba::rgb(26, 26, 30));
        assert_eq!(t.text(), Rgba::rgb(235, 235, 235));
        assert_eq!(t.text_on_accent(), Rgba::rgb(245, 245, 245));
        assert_eq!(
            (t.warn_fill(), t.warn_border(), t.warn_text()),
            (
                Rgba::rgb(52, 40, 42),
                Rgba::rgb(150, 70, 70),
                Rgba::rgb(232, 130, 130)
            )
        );
        assert_eq!(t.search_text(), Rgba::rgb(230, 230, 230));
        assert_eq!(t.search_current_match(), Rgba::rgb(220, 160, 0));
        assert_eq!(t.search_match(), Rgba::rgb(80, 80, 30));
        // No cursor or selection colour of its own — both stay reverse
        // video, as they always were.
        assert!(t.cursor.is_none() && t.selection_bg.is_none());
    }

    /// The indexed table beyond 15 is the standard cube/ramp, and is
    /// not something a theme can get wrong.
    #[test]
    fn the_256_colour_cube_and_ramp_are_theme_independent() {
        for t in [Theme::default(), builtin("nord").unwrap()] {
            assert_eq!(t.indexed(16), Rgba::rgb(0, 0, 0));
            assert_eq!(t.indexed(231), Rgba::rgb(255, 255, 255));
            assert_eq!(t.indexed(232), Rgba::rgb(8, 8, 8));
            assert_eq!(t.indexed(255), Rgba::rgb(238, 238, 238));
        }
        assert_eq!(
            builtin("nord").unwrap().indexed(1),
            Rgba::rgb(0xbf, 0x61, 0x6a)
        );
    }

    /// A scheme theme pins only its own palette; everything else is a
    /// function of it, and must land somewhere legible.
    #[test]
    fn a_scheme_theme_derives_its_chrome_from_its_palette() {
        let t = builtin("gruvbox-dark").unwrap();
        assert!(t.surface.is_none());
        // Lifted off the background, but nowhere near the foreground.
        assert!(t.surface().luminance() > t.background.luminance());
        assert!(t.surface().luminance() < t.foreground.luminance());
        // Dim text sits between the two.
        assert!(t.text_dim().luminance() < t.text().luminance());
        assert!(t.text_dim().luminance() > t.surface().luminance());
        // Every shipped scheme's accent is a light pastel, so text over
        // it has to be dark — "always white on the accent" would be
        // unreadable on all four.
        for name in [
            "tokyonight-storm",
            "catppuccin-mocha",
            "gruvbox-dark",
            "nord",
        ] {
            let t = builtin(name).unwrap();
            assert!(t.accent_primary().luminance() > 0.55, "{name}");
            assert!(t.text_on_accent().luminance() < 0.25, "{name}");
        }
        // A dark accent flips it the other way.
        let t = Theme {
            accents: vec![Rgba::rgb(0x2a, 0x2f, 0x6e).into()],
            ..builtin("nord").unwrap()
        };
        assert!(t.text_on_accent().luminance() > 0.8);
    }

    /// How far `c` sits from the ground, in the direction of the ink.
    /// Negative means it went the wrong way — toward the background
    /// instead of away from it — which is the shape every "does this
    /// read?" check below wants, and the shape that differs between a
    /// light theme and a dark one.
    fn toward_ink(t: &Theme, c: Rgba) -> f32 {
        let d = c.luminance() - t.background.luminance();
        if t.is_light() { -d } else { d }
    }

    /// The two match colours are cell *backgrounds* under text that
    /// keeps its own foreground, so each has to stay clear of the
    /// theme's foreground — the bug the derivation is tuned against is
    /// a pastel-yellow scheme highlighting matched text in something as
    /// light as the text — while still lifting off the ground enough to
    /// be seen at all.
    #[test]
    fn the_search_highlights_stay_readable_under_the_foreground() {
        for name in BUILTIN_NAMES {
            // The pins removed: what is under test is the *derivation*,
            // and the default theme pins its own pre-theme values.
            let t = Theme {
                search_current_match: None,
                search_match: None,
                ..builtin(name).unwrap()
            };
            for (what, c) in [
                ("current", t.search_current_match()),
                ("other", t.search_match()),
            ] {
                assert!(
                    (t.foreground.luminance() - c.luminance()).abs() > 0.2,
                    "{name}: {what} match is too close to the foreground"
                );
                assert!(
                    toward_ink(&t, c) > 0.08,
                    "{name}: {what} match must lift off the background"
                );
            }
            // …and from each other, so `n`/`N` shows which one it is
            // on. On a light theme the current match is the *darker* of
            // the two rather than the lighter; what matters is that
            // they differ, not which way.
            assert!(
                (t.search_current_match().luminance() - t.search_match().luminance()).abs() > 0.05,
                "{name}: the two match colours are indistinguishable"
            );
        }
    }

    /// A warm chip belongs to the panel it sits on: the fill is a tint
    /// of the ground, with the border and then the label stepping
    /// further toward the ink. Which way "toward the ink" points is the
    /// whole reason [`Theme::warn_text`] asks `is_light` — on a light
    /// theme it darkens the red instead of lightening it.
    #[test]
    fn the_warn_trio_is_a_chip_not_a_block_of_red() {
        for name in BUILTIN_NAMES {
            let t = Theme {
                warn_fill: None,
                warn_border: None,
                warn_text: None,
                surface: None,
                ..builtin(name).unwrap()
            };
            assert!(
                (t.warn_fill().luminance() - t.surface().luminance()).abs() < 0.15,
                "{name}: warn fill does not sit at panel weight"
            );
            let (fill, border, text) = (
                toward_ink(&t, t.warn_fill()),
                toward_ink(&t, t.warn_border()),
                toward_ink(&t, t.warn_text()),
            );
            assert!(border > fill, "{name}: warn border must read against its own fill");
            assert!(text > border, "{name}: warn text must read against the border");
            assert!(
                text - fill > 0.25,
                "{name}: warn text is too close to the fill it sits on"
            );
        }
    }

    /// Slot 2 is the accent a program one level in gets — anything
    /// launched inside vmux — so it is the one on screen most of the
    /// time. These trios are a judgement rather than a formula, so pin
    /// them: each is drawn from its own scheme's published colours,
    /// weighted to sit near slot 1, and none of them puts a green
    /// there. Change one on purpose, not by accident.
    #[test]
    fn the_accent_trios_are_the_ones_that_were_chosen() {
        let want: [(&str, [u32; 3]); 7] = [
            ("veter", [0x56799f, 0x9f5685, 0x9f7c56]),
            ("tokyonight-storm", [0x7aa2f7, 0xbb9af7, 0x7dcfff]),
            ("tokyonight-day", [0x2e7de9, 0x9854f1, 0x007197]),
            ("catppuccin-mocha", [0x89b4fa, 0xcba6f7, 0x94e2d5]),
            ("catppuccin-latte", [0x1e66f5, 0x8839ef, 0x179299]),
            ("gruvbox-dark", [0x83a598, 0xd3869b, 0xfabd2f]),
            ("nord", [0x88c0d0, 0xb48ead, 0x81a1c1]),
        ];
        for (name, hexes) in want {
            let got = builtin(name).unwrap().accents();
            assert_eq!(got, hexes.map(c).to_vec(), "{name}");
        }
    }

    /// The same rule for a dropped-in scheme file, which names no
    /// accents of its own: the derivation must not hand slot 2 the
    /// palette's green.
    #[test]
    fn the_derived_accents_skip_the_green() {
        let t = Theme {
            accents: Vec::new(),
            ..builtin("nord").unwrap()
        };
        assert_eq!(
            t.accents(),
            vec![t.ansi[12], t.ansi[13], t.ansi[14]],
            "derived accents are bright blue / magenta / cyan"
        );
        assert!(!t.accents().contains(&t.ansi[10]), "ansi bright green");
    }

    /// `[search] accent` can name a fill the theme's `text_on_accent`
    /// was never picked against: a light one over the default theme,
    /// whose pinned near-white is right only for its muted blue. While
    /// the two accents agree, the pin still wins.
    #[test]
    fn search_chip_text_follows_the_search_accent() {
        let t = builtin("veter").unwrap();
        assert_eq!(t.text_on_search_accent(t.accent_primary()), t.text_on_accent());
        let yellow = Rgba::rgb(0xff, 0xd7, 0x00);
        assert!(t.text_on_search_accent(yellow).luminance() < 0.25);
        assert!(t.text_on_accent().luminance() > 0.8);
    }

    /// Text over an accent fill has to be readable whichever way the
    /// theme runs. A light theme's foreground is the *dark* colour, so
    /// deriving the light pole by lightening it — as this did before
    /// the light themes existed — reaches only a mid grey.
    #[test]
    fn text_on_accent_is_readable_on_a_light_theme_too() {
        for name in BUILTIN_NAMES {
            let t = Theme {
                text_on_accent: Vec::new(),
                ..builtin(name).unwrap()
            };
            let gap = (t.text_on_accent().luminance() - t.accent_primary().luminance()).abs();
            assert!(gap > 0.35, "{name}: text on the accent barely reads ({gap:.2})");
        }
    }

    /// Every slot of every built-in gets text that reads on *that*
    /// slot — the colour a nested client paints, not just slot 1.
    #[test]
    fn text_on_every_accent_slot_reads() {
        for name in BUILTIN_NAMES {
            let t = builtin(name).unwrap();
            for (i, slot) in t.accent_slots().iter().enumerate() {
                let gap = (slot.text.luminance() - slot.fill.luminance()).abs();
                assert!(gap > 0.3, "{name} slot {}: {gap:.2}", i + 1);
            }
        }
    }

    /// …and so does every swatch: these are what `--accent` paints, a
    /// whole tab strip of it, so a name that came out unreadable on
    /// some theme would be a trap only that theme's users fall into.
    #[test]
    fn every_builtin_swatch_reads_and_is_complete() {
        for name in BUILTIN_NAMES {
            let t = builtin(name).unwrap();
            let swatches = t.swatches();
            let names: Vec<&str> = swatches.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, SWATCH_NAMES, "{name}");
            for s in &swatches {
                let gap = (s.text.luminance() - s.fill.luminance()).abs();
                assert!(gap > 0.3, "{name} {}: {gap:.2}", s.name);
            }
        }
    }

    /// A scheme's own orange, not one mixed from its ANSI table and not
    /// vmux's: this is the colour `vmux -A orange` paints there.
    #[test]
    fn a_builtin_swatch_is_the_schemes_own() {
        let t = builtin("tokyonight-day").unwrap();
        let orange = t.swatches().into_iter().find(|s| s.name == "orange").unwrap();
        assert_eq!(orange.fill, Rgba::rgb(0xb1, 0x5c, 0x00));
        assert!(orange.text.luminance() > 0.8, "light text on a dark orange");
    }

    /// A scheme file with no swatches still answers every name, from
    /// its ANSI table; a pin lands by name and leaves the others alone,
    /// from the config section too (which merges per entry rather than
    /// replacing the table).
    #[test]
    fn swatches_derive_and_pin_by_name() {
        let t = Theme {
            swatches: std::collections::BTreeMap::new(),
            ..builtin("nord").unwrap()
        };
        let red = t.swatches().into_iter().find(|s| s.name == "red").unwrap();
        assert_eq!(red.fill, t.ansi[9]);

        let section: toml::Table = toml::from_str(
            r##"
            name = "nord"
            [swatches]
            orange = { fill = "#ff8800", text = "#000000" }
            teal = "#008080"
            "##,
        )
        .unwrap();
        let t = resolve(&section, None);
        let get = |n: &str| t.swatches().into_iter().find(|s| s.name == n).unwrap();
        assert_eq!(get("orange").fill, Rgba::rgb(0xff, 0x88, 0x00));
        assert_eq!(get("orange").text, Rgba::rgb(0, 0, 0));
        assert_eq!(get("teal").fill, Rgba::rgb(0, 0x80, 0x80));
        assert_eq!(get("red").fill, Rgba::rgb(0xbf, 0x61, 0x6a), "nord's own red kept");
    }

    /// An accent can name a swatch, and then it is that swatch — its
    /// fill, its text, and its name on the wire. A `#` keeps hex hex.
    #[test]
    fn accents_name_swatches() {
        let section: toml::Table = toml::from_str(
            r##"
            name = "tokyonight-day"
            accents = ["orange", "#2e7de9", "nonesuch"]
            "##,
        )
        .unwrap();
        let t = resolve(&section, None);
        let slots = t.accent_slots();
        assert_eq!(slots.len(), 2, "an unknown name is skipped");
        assert_eq!(slots[0].name, "orange");
        assert_eq!(slots[0].fill, Rgba::rgb(0xb1, 0x5c, 0x00));
        assert_eq!(slots[1].name, "");
        let palette = t.host_palette();
        assert_eq!(palette.accents[0].name, "orange");
        assert_eq!(palette.swatches.len(), SWATCH_NAMES.len());
    }

    /// Text over an accent carries no hue of its own: tokyonight-day's
    /// ink is a saturated blue, which over its orange slot read as the
    /// blue accent instead of as text.
    #[test]
    fn text_on_an_accent_is_neutral() {
        let t = builtin("tokyonight-day").unwrap();
        for fill in [Rgba::rgb(0xcf, 0x7d, 0x3c), Rgba::rgb(0x2e, 0x7d, 0xe9)] {
            let Rgba { r, g, b, .. } = t.text_on(fill);
            assert!(r == g && g == b, "{fill:?}");
        }
    }

    /// The pin list runs slot for slot, and a slot it does not reach
    /// derives against its own accent — so a light accent below a dark
    /// one gets dark text even when slot 1's light text is pinned. The
    /// single string older configs spell is a one-entry list.
    #[test]
    fn text_on_accent_pins_per_slot() {
        let dark = Rgba::rgb(0x2a, 0x2f, 0x6e);
        let light = Rgba::rgb(0xff, 0xd7, 0x00);
        let section: toml::Table = toml::from_str(
            r##"
            accents = ["#2a2f6e", "#ffd700", "#2a2f6e"]
            text_on_accent = "#f5f5f5"
            "##,
        )
        .unwrap();
        let t = resolve(&section, None);
        assert_eq!(t.text_on_accent, vec![Rgba::rgb(0xf5, 0xf5, 0xf5)]);
        let texts: Vec<Rgba> = t.accent_slots().iter().map(|s| s.text).collect();
        assert_eq!(texts, [Rgba::rgb(0xf5, 0xf5, 0xf5), t.text_on(light), t.text_on(dark)]);
        assert!(t.text_on(light).luminance() < 0.25);

        let section: toml::Table = toml::from_str(
            r##"
            accents = ["#2a2f6e", "#ffd700"]
            text_on_accent = ["#ffffff", "#000000"]
            "##,
        )
        .unwrap();
        let t = resolve(&section, None);
        let palette = t.host_palette();
        assert_eq!(palette.accents[1].text, Rgba::rgb(0, 0, 0).to_command_color());
    }

    #[test]
    fn every_builtin_name_resolves() {
        for name in BUILTIN_NAMES {
            assert!(builtin(name).is_some(), "{name}");
        }
        assert!(builtin("solarized").is_none());
    }

    #[test]
    fn a_named_theme_takes_per_key_overrides() {
        let section: toml::Table = toml::from_str(
            r##"
            name = "nord"
            background = "#000000"
            accents = ["#ff0000"]
            "##,
        )
        .unwrap();
        let t = resolve(&section, None);
        assert_eq!(t.background, Rgba::rgb(0, 0, 0));
        assert_eq!(t.accent_primary(), Rgba::rgb(255, 0, 0));
        // Everything not named still comes from nord.
        assert_eq!(t.foreground, Rgba::rgb(0xd8, 0xde, 0xe9));
        assert_eq!(t.ansi[2], Rgba::rgb(0xa3, 0xbe, 0x8c));
    }

    /// A section with no `name` overrides the default theme, and an
    /// unknown name falls back to it rather than failing.
    #[test]
    fn resolution_falls_back_instead_of_failing() {
        let section: toml::Table = toml::from_str(r##"foreground = "#ffffff""##).unwrap();
        let t = resolve(&section, None);
        assert_eq!(t.foreground, Rgba::rgb(255, 255, 255));
        assert_eq!(t.background, Theme::default().background);

        let section: toml::Table = toml::from_str(r#"name = "nope""#).unwrap();
        assert_eq!(
            resolve(&section, None).background,
            Theme::default().background
        );

        // A bad colour keeps the base theme rather than dropping veter
        // to defaults mid-way through a merge.
        let section: toml::Table =
            toml::from_str("name = \"nord\"\nbackground = \"not a colour\"").unwrap();
        assert_eq!(
            resolve(&section, None).background,
            builtin("nord").unwrap().background
        );
    }

    /// Overriding one key of a scheme keeps the rest of it as the
    /// scheme had it — derived where the scheme derives — rather than
    /// filling every unpinned field with veter's own chrome.
    #[test]
    fn an_override_keeps_the_schemes_derived_fields() {
        let section: toml::Table =
            toml::from_str("name = \"tokyonight-day\"\ncursor = \"#ff0000\"").unwrap();
        let t = resolve(&section, None);
        assert_eq!(t.cursor, Some(Rgba::rgb(255, 0, 0)));
        let day = builtin("tokyonight-day").unwrap();
        assert_eq!(t.surface, None);
        assert_eq!(t.surface(), day.surface());
        assert_eq!(t.text_on_accent, day.text_on_accent);
        assert_eq!(t.swatches, day.swatches);
    }

    /// A user theme file is the same schema, merged onto an unpinned
    /// base — so what it leaves out derives from what it gives rather
    /// than being veter's own dark chrome — and cannot be used to read
    /// a file outside the themes directory.
    #[test]
    fn a_user_theme_file_derives_what_it_leaves_out() {
        let dir = std::env::temp_dir().join(format!("veter-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        std::fs::write(
            dir.join("themes").join("mine.toml"),
            "background = \"#0a0b0c\"\nforeground = \"#fafbfc\"\n",
        )
        .unwrap();

        let section: toml::Table = toml::from_str(r#"name = "mine""#).unwrap();
        let t = resolve(&section, Some(&dir));
        assert_eq!(t.background, Rgba::rgb(10, 11, 12));
        assert_eq!(t.foreground, Rgba::rgb(250, 251, 252));
        // A required key it leaves out comes from the default theme…
        assert_eq!(t.ansi[1], Theme::default().ansi[1]);
        // …and nothing veter pins for itself does: the chrome derives
        // from this file's own ground and ink.
        assert_eq!(t.surface, None);
        assert_eq!(t.surface(), t.background.mix(t.foreground, 0.10));
        assert_eq!(t.text(), t.foreground);
        assert!(t.accents.is_empty() && t.swatches.is_empty());
        assert_eq!(t.accents(), vec![t.ansi[12], t.ansi[13], t.ansi[14]]);

        // A light scheme gets a light panel, not veter's dark one.
        std::fs::write(
            dir.join("themes").join("light.toml"),
            "background = \"#f0f0f0\"\nforeground = \"#202020\"\n",
        )
        .unwrap();
        let section: toml::Table = toml::from_str(r#"name = "light""#).unwrap();
        let t = resolve(&section, Some(&dir));
        assert!(t.surface().luminance() > 0.8, "{:?}", t.surface());
        assert!(t.text_on_accent().luminance() > 0.8 || t.accent_primary().luminance() > 0.55);

        for escape in ["../mine", "..", ".mine"] {
            let section: toml::Table = toml::Table::from_iter([("name".into(), escape.into())]);
            assert_eq!(
                resolve(&section, Some(&dir)).background,
                Theme::default().background,
                "{escape}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
