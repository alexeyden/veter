//! User configuration for the `veter` GUI binary.
//!
//! veter reads `$XDG_CONFIG_HOME/veter/config.toml` (falling back to
//! `$HOME/.config/veter/config.toml`) once at startup. Everything is
//! optional; a missing file or a parse error falls back to the built-in
//! defaults — the exact values that were hardcoded before this module
//! existed — logged to stderr but never fatal.
//!
//! Three things are configurable:
//!
//!  * `[accent]` — the shared accent palette the host publishes into the
//!    reserved `host.*` VGE style namespace (see VGE §7.3). vmux and
//!    other clients render their chrome from it via `host.accent`.
//!  * `[search]` — the search-chrome colors (search bar + match
//!    highlights).
//!  * `[keys]` — the host-intercepted key chords (search, scroll,
//!    overlay, copy, paste) and the in-search-overlay modal keys.
//!
//! This is a binary-local module: nothing here touches the `veter-host`
//! engine state, so `vsd` (which shares those engines) is unaffected and
//! keeps its built-in palette.

use std::path::PathBuf;

use femtovg::Color;
use serde::Deserialize;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Built-in accent palette (muted blue / olive / violet). Slot N maps to
/// `host.accent.{N+1}`; the contextual `host.accent` rotates through the
/// list by portal nesting depth.
const DEFAULT_ACCENT: [(u8, u8, u8); 3] = [
    (0x56, 0x79, 0x9F), // accent.1 — muted blue
    (0x85, 0x9F, 0x3D), // accent.2 — olive
    (0x5A, 0x3C, 0x9E), // accent.3 — violet
];

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

/// A straight (non-premultiplied) 8-bit RGBA color, deserialized from a
/// `#rrggbb` or `#rrggbbaa` hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    fn parse(s: &str) -> Result<Self, String> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        let byte = |i: usize| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|_| format!("invalid hex color '{s}'"))
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
    pub fn to_femto(self) -> Color {
        Color::rgba(self.r, self.g, self.b, self.a)
    }

    /// VGE protocol color (f32, straight alpha) for the accent palette.
    pub fn to_command_color(self) -> veter::vge::Color {
        veter::vge::Color {
            r: self.r as f32 / 255.0,
            g: self.g as f32 / 255.0,
            b: self.b as f32 / 255.0,
            a: self.a as f32 / 255.0,
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

/// `[accent]` — the ordered accent palette.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AccentConfig {
    /// Ordered accent palette; slot N maps to `host.accent.{N+1}`.
    pub palette: Vec<Rgba>,
}

impl Default for AccentConfig {
    fn default() -> Self {
        Self {
            palette: DEFAULT_ACCENT
                .iter()
                .map(|&(r, g, b)| Rgba::rgb(r, g, b))
                .collect(),
        }
    }
}

/// `[search]` — search-chrome colors. `bar_bg` is optional and defaults
/// to the first accent color when unset.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SearchColors {
    /// Search-bar background. `None` → first accent color.
    pub bar_bg: Option<Rgba>,
    /// Search-bar text.
    pub bar_text: Rgba,
    /// The active match (the one `n`/`N` navigates to).
    pub current_match: Rgba,
    /// All other (non-current) matches.
    #[serde(rename = "match")]
    pub match_color: Rgba,
}

impl Default for SearchColors {
    fn default() -> Self {
        Self {
            bar_bg: None,
            bar_text: Rgba::rgb(230, 230, 230),
            current_match: Rgba::rgb(220, 160, 0),
            match_color: Rgba::rgb(80, 80, 30),
        }
    }
}

// ---------------------------------------------------------------------------
// Key chords
// ---------------------------------------------------------------------------

/// Host actions intercepted before the PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAction {
    OpenSearch,
    OpenOverlay,
    ScrollPageUp,
    ScrollPageDown,
    Copy,
    Paste,
}

/// In-search-overlay modal actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchAction {
    Close,
    NextMatch,
    PrevMatch,
    ToggleCase,
    PageUp,
    PageDown,
}

/// How a chord's terminal key is matched against a winit event.
#[derive(Debug, Clone, PartialEq)]
enum KeyMatch {
    /// A printable character. Matched against the event's logical
    /// character; see [`Chord::matches`] for the case rules.
    Char(char),
    Named(NamedKey),
}

/// A parsed key chord: required modifiers plus a terminal key.
#[derive(Debug, Clone, PartialEq)]
pub struct Chord {
    ctrl: bool,
    shift: bool,
    alt: bool,
    key: KeyMatch,
}

impl Chord {
    fn parse(spec: &str) -> Result<Self, String> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut key: Option<KeyMatch> = None;
        for part in spec.split('+') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" | "option" | "meta" => alt = true,
                _ => {
                    if key.is_some() {
                        return Err(format!("chord '{spec}' has more than one key"));
                    }
                    key = Some(parse_key_token(p, spec)?);
                }
            }
        }
        let key = key.ok_or_else(|| format!("chord '{spec}' has no key"))?;
        Ok(Self {
            ctrl,
            shift,
            alt,
            key,
        })
    }

    /// Does this chord match a winit key event?
    ///
    /// Ctrl and Alt must always match the modifier state exactly. The
    /// two key regimes differ in how Shift is treated:
    ///
    ///  * A *named* key (PageUp, Space, …) checks Shift explicitly.
    ///  * A *character* key with no Ctrl/Alt relies on the logical
    ///    character already carrying the case (`shift+n` → `N`), so it
    ///    matches the character case-sensitively and ignores Shift.
    ///  * A *character* key with Ctrl or Alt (e.g. `Ctrl+Shift+C`)
    ///    matches the letter case-insensitively — under those modifiers
    ///    the reported character case is unreliable — and checks Shift
    ///    explicitly.
    pub fn matches(&self, key: &Key, mods: &ModifiersState) -> bool {
        if self.ctrl != mods.control_key() || self.alt != mods.alt_key() {
            return false;
        }
        match &self.key {
            KeyMatch::Named(NamedKey::Space) => {
                self.shift == mods.shift_key()
                    && (matches!(key, Key::Named(NamedKey::Space))
                        || matches!(key, Key::Character(c) if c.as_str() == " "))
            }
            KeyMatch::Named(named) => {
                self.shift == mods.shift_key() && matches!(key, Key::Named(k) if k == named)
            }
            KeyMatch::Char(want) => {
                let Key::Character(c) = key else { return false };
                let mut chars = c.chars();
                let Some(got) = chars.next() else { return false };
                if chars.next().is_some() {
                    return false;
                }
                if self.ctrl || self.alt {
                    self.shift == mods.shift_key() && got.eq_ignore_ascii_case(want)
                } else {
                    got == *want
                }
            }
        }
    }
}

fn parse_key_token(tok: &str, spec: &str) -> Result<KeyMatch, String> {
    let mut chars = tok.chars();
    let first = chars.next().ok_or_else(|| format!("chord '{spec}' has an empty key"))?;
    if chars.next().is_none() {
        return Ok(KeyMatch::Char(first));
    }
    let named = match tok.to_ascii_lowercase().as_str() {
        "space" => NamedKey::Space,
        "enter" | "return" => NamedKey::Enter,
        "escape" | "esc" => NamedKey::Escape,
        "backspace" => NamedKey::Backspace,
        "tab" => NamedKey::Tab,
        "pageup" | "pgup" => NamedKey::PageUp,
        "pagedown" | "pgdn" => NamedKey::PageDown,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "delete" | "del" => NamedKey::Delete,
        "insert" | "ins" => NamedKey::Insert,
        "up" => NamedKey::ArrowUp,
        "down" => NamedKey::ArrowDown,
        "left" => NamedKey::ArrowLeft,
        "right" => NamedKey::ArrowRight,
        "f1" => NamedKey::F1,
        "f2" => NamedKey::F2,
        "f3" => NamedKey::F3,
        "f4" => NamedKey::F4,
        "f5" => NamedKey::F5,
        "f6" => NamedKey::F6,
        "f7" => NamedKey::F7,
        "f8" => NamedKey::F8,
        "f9" => NamedKey::F9,
        "f10" => NamedKey::F10,
        "f11" => NamedKey::F11,
        "f12" => NamedKey::F12,
        _ => return Err(format!("chord '{spec}' has unknown key '{tok}'")),
    };
    Ok(KeyMatch::Named(named))
}

/// `[keys]` — host-intercepted chords (raw strings; parsed into
/// [`KeyBindings`] via [`Config::key_bindings`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KeyBindingsConfig {
    pub open_search: String,
    pub open_overlay: String,
    pub scroll_page_up: String,
    pub scroll_page_down: String,
    pub copy: String,
    pub paste: String,
    pub search: SearchKeysConfig,
}

impl Default for KeyBindingsConfig {
    fn default() -> Self {
        Self {
            open_search: "/".into(),
            open_overlay: "Ctrl+Shift+Space".into(),
            scroll_page_up: "Shift+PageUp".into(),
            scroll_page_down: "Shift+PageDown".into(),
            copy: "Ctrl+Shift+C".into(),
            paste: "Ctrl+Shift+V".into(),
            search: SearchKeysConfig::default(),
        }
    }
}

/// `[keys.search]` — in-search-overlay modal chords.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SearchKeysConfig {
    pub close: String,
    pub next_match: String,
    pub prev_match: String,
    pub toggle_case: String,
    pub page_up: String,
    pub page_down: String,
}

impl Default for SearchKeysConfig {
    fn default() -> Self {
        Self {
            close: "Escape".into(),
            next_match: "n".into(),
            prev_match: "N".into(),
            toggle_case: "Alt+C".into(),
            page_up: "PageUp".into(),
            page_down: "PageDown".into(),
        }
    }
}

/// One user-defined command run against the current selection.
#[derive(Debug, Clone, Deserialize)]
pub struct SelectionCommandConfig {
    /// Chord that triggers it (see [`Chord`] for syntax).
    pub key: String,
    /// Shell command, run via `$SHELL -c` with the selection in
    /// `$VETER_SELECTION` and `$1`.
    pub command: String,
    /// Optional human note; accepted for self-documentation but unused
    /// at runtime.
    #[serde(default)]
    #[allow(dead_code)]
    pub description: Option<String>,
}

/// Compiled chord → action tables, resolved from [`KeyBindingsConfig`].
pub struct KeyBindings {
    host: Vec<(Chord, HostAction)>,
    search: Vec<(Chord, SearchAction)>,
    /// Chord → shell command for selection commands (in config order).
    selection: Vec<(Chord, String)>,
}

impl KeyBindings {
    fn build(cfg: &KeyBindingsConfig, selection_cmds: &[SelectionCommandConfig]) -> Self {
        // Fall back to the built-in default chord for any spec that fails
        // to parse, so one bad line never disarms an action.
        let defaults = KeyBindingsConfig::default();
        let parse = |spec: &str, fallback: &str| -> Chord {
            match Chord::parse(spec) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("veter: config: {e}; using default '{fallback}'");
                    Chord::parse(fallback).expect("built-in default chord parses")
                }
            }
        };

        let host = vec![
            (
                parse(&cfg.open_search, &defaults.open_search),
                HostAction::OpenSearch,
            ),
            (
                parse(&cfg.open_overlay, &defaults.open_overlay),
                HostAction::OpenOverlay,
            ),
            (
                parse(&cfg.scroll_page_up, &defaults.scroll_page_up),
                HostAction::ScrollPageUp,
            ),
            (
                parse(&cfg.scroll_page_down, &defaults.scroll_page_down),
                HostAction::ScrollPageDown,
            ),
            (parse(&cfg.copy, &defaults.copy), HostAction::Copy),
            (parse(&cfg.paste, &defaults.paste), HostAction::Paste),
        ];

        let s = &cfg.search;
        let ds = &defaults.search;
        let search = vec![
            (parse(&s.close, &ds.close), SearchAction::Close),
            (parse(&s.next_match, &ds.next_match), SearchAction::NextMatch),
            (parse(&s.prev_match, &ds.prev_match), SearchAction::PrevMatch),
            (
                parse(&s.toggle_case, &ds.toggle_case),
                SearchAction::ToggleCase,
            ),
            (parse(&s.page_up, &ds.page_up), SearchAction::PageUp),
            (parse(&s.page_down, &ds.page_down), SearchAction::PageDown),
        ];

        // Selection commands have no built-in defaults; a bad chord is
        // skipped (its command simply won't be bound) with a warning.
        let mut selection = Vec::new();
        for sc in selection_cmds {
            match Chord::parse(&sc.key) {
                Ok(c) => selection.push((c, sc.command.clone())),
                Err(e) => eprintln!("veter: config: selection command: {e}; skipping"),
            }
        }

        Self {
            host,
            search,
            selection,
        }
    }

    pub fn resolve_host(&self, key: &Key, mods: &ModifiersState) -> Option<HostAction> {
        self.host
            .iter()
            .find(|(chord, _)| chord.matches(key, mods))
            .map(|(_, action)| *action)
    }

    pub fn resolve_search(&self, key: &Key, mods: &ModifiersState) -> Option<SearchAction> {
        self.search
            .iter()
            .find(|(chord, _)| chord.matches(key, mods))
            .map(|(_, action)| *action)
    }

    /// The shell command bound to this key, if any. Returns an owned
    /// `String` so the caller can then take `&mut self`.
    pub fn resolve_selection_command(&self, key: &Key, mods: &ModifiersState) -> Option<String> {
        self.selection
            .iter()
            .find(|(chord, _)| chord.matches(key, mods))
            .map(|(_, command)| command.clone())
    }
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub accent: AccentConfig,
    pub search: SearchColors,
    pub keys: KeyBindingsConfig,
    /// User-defined commands run against the current selection. Empty by
    /// default — there are no built-in selection commands.
    pub selection_commands: Vec<SelectionCommandConfig>,
}

impl Config {
    /// Load from the config path, or return defaults. A missing file is
    /// silent; a read or parse error logs one line to stderr and falls
    /// back to defaults.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Config::default();
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
            Err(e) => {
                eprintln!("veter: config: cannot read {}: {e}", path.display());
                return Config::default();
            }
        };
        match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("veter: config: parse error in {}: {e}", path.display());
                Config::default()
            }
        }
    }

    /// Accent palette as VGE colors, guaranteed non-empty (an empty
    /// configured list falls back to the built-in palette).
    pub fn accent_palette(&self) -> Vec<veter::vge::Color> {
        if self.accent.palette.is_empty() {
            AccentConfig::default().palette
        } else {
            self.accent.palette.clone()
        }
        .iter()
        .map(|c| c.to_command_color())
        .collect()
    }

    /// Effective search-bar background: the configured `bar_bg`, else the
    /// first accent color, else the built-in accent slot 0.
    pub fn search_bar_bg(&self) -> Rgba {
        self.search.bar_bg.unwrap_or_else(|| {
            self.accent.palette.first().copied().unwrap_or_else(|| {
                let (r, g, b) = DEFAULT_ACCENT[0];
                Rgba::rgb(r, g, b)
            })
        })
    }

    pub fn key_bindings(&self) -> KeyBindings {
        KeyBindings::build(&self.keys, &self.selection_commands)
    }
}

/// `$XDG_CONFIG_HOME/veter/config.toml`, else
/// `$HOME/.config/veter/config.toml`. `None` if neither env var is set.
fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir).join("veter").join("config.toml"));
    }
    let home = std::env::var_os("HOME").filter(|s| !s.is_empty())?;
    Some(PathBuf::from(home).join(".config").join("veter").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_colors() {
        assert_eq!(Rgba::parse("#56799f").unwrap(), Rgba::rgb(0x56, 0x79, 0x9f));
        assert_eq!(
            Rgba::parse("#12345678").unwrap(),
            Rgba {
                r: 0x12,
                g: 0x34,
                b: 0x56,
                a: 0x78
            }
        );
        // `#` is optional.
        assert_eq!(Rgba::parse("ffffff").unwrap(), Rgba::rgb(255, 255, 255));
        assert!(Rgba::parse("#xyz").is_err());
        assert!(Rgba::parse("#12345").is_err());
    }

    #[test]
    fn default_config_parses_all_chords() {
        // build() must never hit its error branch for the defaults.
        let keys = KeyBindings::build(&KeyBindingsConfig::default(), &[]);
        assert_eq!(keys.host.len(), 6);
        assert_eq!(keys.search.len(), 6);
        assert!(keys.selection.is_empty());
    }

    #[test]
    fn selection_commands_resolve_by_key() {
        let cfg: Config = toml::from_str(
            r#"
            [[selection_commands]]
            key = "o"
            command = 'xdg-open "$VETER_SELECTION"'

            [[selection_commands]]
            key = "Ctrl+B"
            command = "firefox \"$1\""
            "#,
        )
        .unwrap();
        let keys = cfg.key_bindings();
        let none = ModifiersState::empty();
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(
            keys.resolve_selection_command(&Key::Character("o".into()), &none)
                .as_deref(),
            Some(r#"xdg-open "$VETER_SELECTION""#)
        );
        assert_eq!(
            keys.resolve_selection_command(&Key::Character("b".into()), &ctrl)
                .as_deref(),
            Some(r#"firefox "$1""#)
        );
        // Unbound key → None.
        assert!(keys
            .resolve_selection_command(&Key::Character("z".into()), &none)
            .is_none());
    }

    #[test]
    fn selection_command_bad_chord_is_skipped() {
        let cfg: Config = toml::from_str(
            r#"
            [[selection_commands]]
            key = "Nonsense+Bad"
            command = "true"

            [[selection_commands]]
            key = "o"
            command = "true"
            "#,
        )
        .unwrap();
        // The bad entry is dropped; the good one still binds.
        let keys = cfg.key_bindings();
        assert_eq!(keys.selection.len(), 1);
        assert!(keys
            .resolve_selection_command(&Key::Character("o".into()), &ModifiersState::empty())
            .is_some());
    }

    #[test]
    fn chord_matches_modified_letters_case_insensitively() {
        let copy = Chord::parse("Ctrl+Shift+C").unwrap();
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        // Whether the OS reports 'c' or 'C', ctrl+shift+c matches.
        assert!(copy.matches(&Key::Character("c".into()), &ctrl_shift));
        assert!(copy.matches(&Key::Character("C".into()), &ctrl_shift));
        // Without shift it must not match.
        assert!(!copy.matches(&Key::Character("c".into()), &ModifiersState::CONTROL));
    }

    #[test]
    fn chord_matches_bare_letters_case_sensitively() {
        let next = Chord::parse("n").unwrap();
        let prev = Chord::parse("N").unwrap();
        let none = ModifiersState::empty();
        let shift = ModifiersState::SHIFT;
        assert!(next.matches(&Key::Character("n".into()), &none));
        assert!(!next.matches(&Key::Character("N".into()), &shift));
        assert!(prev.matches(&Key::Character("N".into()), &shift));
        assert!(!prev.matches(&Key::Character("n".into()), &none));
    }

    #[test]
    fn chord_matches_named_and_space() {
        let overlay = Chord::parse("Ctrl+Shift+Space").unwrap();
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert!(overlay.matches(&Key::Named(NamedKey::Space), &ctrl_shift));
        // Space can also arrive as a " " character key.
        assert!(overlay.matches(&Key::Character(" ".into()), &ctrl_shift));

        let pgup = Chord::parse("Shift+PageUp").unwrap();
        assert!(pgup.matches(&Key::Named(NamedKey::PageUp), &ModifiersState::SHIFT));
        assert!(!pgup.matches(&Key::Named(NamedKey::PageUp), &ModifiersState::empty()));
    }
}
