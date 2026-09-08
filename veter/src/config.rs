//! User configuration for the `veter` GUI binary.
//!
//! veter reads `$XDG_CONFIG_HOME/veter/config.toml` (falling back to
//! `$HOME/.config/veter/config.toml`) once at startup. Everything is
//! optional; a missing file or a parse error falls back to the built-in
//! defaults — the exact values that were hardcoded before this module
//! existed — logged to stderr but never fatal.
//!
//! Six things are configurable:
//!
//!  * `[theme]` — every colour: the grid palette, the terminal's
//!    default fore/background, the accents published into the reserved
//!    `host.*` VGE style namespace (VGE §7.3), and veter's own overlay
//!    chrome. `name` picks one of the built-ins in [`veter::theme`] or
//!    a user file under `themes/`, and any other key overrides that
//!    theme's own value.
//!  * `[font]` — the primary family and the fallback families tried for
//!    a character it lacks.
//!  * `[search]` — the search-chrome colors (search bar + match
//!    highlights), each key overriding the theme's own.
//!  * `[keys]` — the host-intercepted key chords (search, scroll,
//!    overlay, hints, copy, paste, save-image) and the in-overlay modal
//!    keys.
//!  * `[window]` — window-level behavior (the close confirmation).
//!  * `[hints]` — which detectors the overlay's hint mode runs, and in
//!    what priority order.
//!
//! This is a binary-local module: nothing here touches the `veter-host`
//! engine state, so `vsd` (which shares those engines) has no config of
//! its own. It borrows the *rendering* client's palette instead —
//! reported in the PRT probe on attach (`doc/portal-extension.md` §10)
//! — which is what keeps a client started in a detached session on the
//! same theme as one started under a live renderer.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use veter::hints::{HintConfig, HintKind};
use veter::theme::Theme;

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

// The hex-string colour type and its parser live with the themes, since
// a theme file is written in the same notation as a config key. Re-exported
// here because `[theme]`, `[search]` and every other colour key in this
// module is one.
pub use veter::theme::Rgba;

/// `[search]` — search-chrome colors. Every key is optional; an unset
/// one comes from the theme, which is what lets `[theme] name = …`
/// restyle the whole panel on its own.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SearchColors {
    /// Accent tint for the search panel's chrome (border, caret, chip
    /// fills). `None` → the theme's search accent, which is the first
    /// accent colour unless the theme pins one of its own. `bar_bg` is
    /// the pre-panel name for the same key.
    #[serde(alias = "bar_bg")]
    pub accent: Option<Rgba>,
    /// Search-bar text.
    pub bar_text: Option<Rgba>,
    /// The active match (the one `n`/`N` navigates to).
    pub current_match: Option<Rgba>,
    /// All other (non-current) matches.
    #[serde(rename = "match")]
    pub match_color: Option<Rgba>,
}

/// `[window]` — window-level behavior.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    /// Ask before acting on the window manager's close request. `false`
    /// quits immediately, as veter did before the prompt existed. Only
    /// the WM close path is guarded — the child exiting on its own still
    /// ends the process without asking.
    pub confirm_close: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            confirm_close: true,
        }
    }
}

/// `[font]` — which faces the grid is drawn with.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    /// Primary family. Anything Fontconfig resolves works, including the
    /// generic aliases (`monospace`, `monospace:style=Regular`), which is
    /// what the default leaves it to.
    pub family: String,
    /// Families tried, in order, for a character the primary lacks —
    /// before the Fontconfig lookup that would otherwise pick one.
    ///
    /// Empty by default, and worth leaving that way unless a particular
    /// face is wanted: Fontconfig's charset match tends to land on a
    /// patched monospace font, whose cell proportions are close enough
    /// to the primary's to need no correction, whereas a standalone
    /// symbol face advances a full em per glyph — at a 19.2px cell,
    /// Symbols Nerd Font Mono advances 32px — and has to be scaled down
    /// to fit, which leaves its icons noticeably short.
    pub fallback: Vec<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            fallback: Vec::new(),
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
    /// Open the overlay straight into hint mode — labels on every
    /// auto-detected URL / path / hash on screen, no query to type.
    OpenHints,
    ScrollPageUp,
    ScrollPageDown,
    Copy,
    Paste,
    /// Write the selected VGE image to a file the user picks. Only
    /// consumed when an image is actually selected; otherwise the chord
    /// falls through to the inner program, so binding a common key
    /// costs nothing the rest of the time.
    SaveImage,
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
    /// Flip the open overlay between typed-query search and hint mode,
    /// keeping the target leaf and scroll position.
    ToggleHints,
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
    pub open_hints: String,
    pub scroll_page_up: String,
    pub scroll_page_down: String,
    pub copy: String,
    pub paste: String,
    pub save_image: String,
    pub search: SearchKeysConfig,
}

impl Default for KeyBindingsConfig {
    fn default() -> Self {
        Self {
            open_search: "/".into(),
            open_overlay: "Ctrl+Shift+Space".into(),
            open_hints: "Ctrl+Shift+F".into(),
            scroll_page_up: "Shift+PageUp".into(),
            scroll_page_down: "Shift+PageDown".into(),
            copy: "Ctrl+Shift+C".into(),
            paste: "Ctrl+Shift+V".into(),
            save_image: "Ctrl+Shift+S".into(),
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
    pub toggle_hints: String,
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
            toggle_hints: "Tab".into(),
        }
    }
}

/// `[hints]` — which detectors hint mode runs, in priority order.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HintsConfig {
    /// Detector names (`url`, `email`, `uuid`, `ip`, `path`, `file`,
    /// `hash`, `color`). Order is priority: when two detectors claim
    /// overlapping text the earlier one wins. Drop a name to disable it.
    pub kinds: Vec<String>,
    /// Extra extensions the `file` detector should accept, on top of its
    /// built-in list (`veter::hints::FILE_EXTENSIONS`). Written with or
    /// without the leading dot; matched case-insensitively.
    pub extra_file_extensions: Vec<String>,
}

impl Default for HintsConfig {
    fn default() -> Self {
        Self {
            kinds: HintKind::DEFAULT_ORDER
                .iter()
                .map(|k| k.name().to_string())
                .collect(),
            extra_file_extensions: Vec::new(),
        }
    }
}

/// One user-defined action run against the current selection. Exactly
/// one of `command` (spawn a shell) or `input` (type text back into the
/// terminal) must be present.
#[derive(Debug, Clone, Deserialize)]
pub struct SelectionCommandConfig {
    /// Chord that triggers it (see [`Chord`] for syntax).
    pub key: String,
    /// Shell command, run via `$SHELL -c` with the selection in
    /// `$VETER_SELECTION` and `$1`.
    #[serde(default)]
    pub command: Option<String>,
    /// Text template written to the PTY as if typed, with `%`
    /// substitutions expanded (see [`expand_selection_input`]).
    #[serde(default)]
    pub input: Option<String>,
    /// Optional human note; accepted for self-documentation but unused
    /// at runtime.
    #[serde(default)]
    #[allow(dead_code)]
    pub description: Option<String>,
}

/// What a bound selection chord does when it fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionAction {
    /// Run this shell command via `$SHELL -c`, detached. The selection
    /// reaches it through the environment and argv, not the string.
    Command(String),
    /// Write this template to the PTY as if the user had typed it, after
    /// expanding its `%` placeholders against the selection.
    Input(String),
}

/// Compiled chord → action tables, resolved from [`KeyBindingsConfig`].
pub struct KeyBindings {
    host: Vec<(Chord, HostAction)>,
    search: Vec<(Chord, SearchAction)>,
    /// Chord → action for selection commands (in config order).
    selection: Vec<(Chord, SelectionAction)>,
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
                parse(&cfg.open_hints, &defaults.open_hints),
                HostAction::OpenHints,
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
            (
                parse(&cfg.save_image, &defaults.save_image),
                HostAction::SaveImage,
            ),
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
            (
                parse(&s.toggle_hints, &ds.toggle_hints),
                SearchAction::ToggleHints,
            ),
        ];

        // Selection commands have no built-in defaults; a bad chord — or
        // an entry that names neither or both of `command` / `input` — is
        // skipped (it simply won't be bound) with a warning.
        let mut selection = Vec::new();
        for sc in selection_cmds {
            let action = match (&sc.command, &sc.input) {
                (Some(c), None) => SelectionAction::Command(c.clone()),
                (None, Some(i)) => SelectionAction::Input(i.clone()),
                (Some(_), Some(_)) => {
                    eprintln!(
                        "veter: config: selection command '{}': `command` and `input` are mutually exclusive; skipping",
                        sc.key
                    );
                    continue;
                }
                (None, None) => {
                    eprintln!(
                        "veter: config: selection command '{}': needs either `command` or `input`; skipping",
                        sc.key
                    );
                    continue;
                }
            };
            match Chord::parse(&sc.key) {
                Ok(c) => selection.push((c, action)),
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

    /// The selection action bound to this key, if any. Returns an owned
    /// value so the caller can then take `&mut self`.
    pub fn resolve_selection_action(
        &self,
        key: &Key,
        mods: &ModifiersState,
    ) -> Option<SelectionAction> {
        self.selection
            .iter()
            .find(|(chord, _)| chord.matches(key, mods))
            .map(|(_, action)| action.clone())
    }
}

/// Expand a selection `input` template against the selected text.
///
/// * `%`  — the selection, verbatim.
/// * `%q` — the selection, POSIX-shell-quoted, so `cd %q` survives a
///   path with spaces or quotes in it.
/// * `%k` — the hint kind (`url`, `path`, …) when the selection came
///   from a hint label, and the empty string otherwise.
/// * `%%` — a literal `%`.
///
/// Everything else is copied through untouched, including the newline a
/// TOML basic string already produced from `\n` — which is what makes
/// `input = "cd %q\n"` press Enter as well as type the line.
pub fn expand_selection_input(
    template: &str,
    selection: &str,
    kind: Option<HintKind>,
) -> String {
    let mut out = String::with_capacity(template.len() + selection.len());
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some('q') => {
                chars.next();
                out.push_str(&shell_quote(selection));
            }
            Some('k') => {
                chars.next();
                out.push_str(kind.map(|k| k.name()).unwrap_or(""));
            }
            _ => out.push_str(selection),
        }
    }
    out
}

/// POSIX single-quote a string: wrap in `'…'`, and render any embedded
/// `'` as `'\''`. Safe for arbitrary text including spaces and `$`.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The `[theme]` section, kept as a raw table: `name` picks the
    /// base and every other key overrides one of its fields, so the
    /// schema is [`veter::theme::Theme`]'s own rather than a second
    /// copy of it here. Resolved by [`Config::theme`].
    pub theme: toml::Table,
    pub font: FontConfig,
    pub search: SearchColors,
    pub keys: KeyBindingsConfig,
    pub window: WindowConfig,
    pub hints: HintsConfig,
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

    /// What hint mode should look for. Unknown detector names are
    /// reported and skipped; a list that ends up empty falls back to the
    /// built-in order, since an overlay with no detectors at all would
    /// just look broken.
    pub fn hint_config(&self) -> HintConfig {
        let mut kinds = Vec::with_capacity(self.hints.kinds.len());
        for name in &self.hints.kinds {
            match HintKind::parse(name) {
                Some(k) if !kinds.contains(&k) => kinds.push(k),
                Some(_) => {}
                None => eprintln!("veter: config: unknown hint kind '{name}'; skipping"),
            }
        }
        if kinds.is_empty() {
            kinds.extend_from_slice(HintKind::DEFAULT_ORDER);
        }
        HintConfig {
            kinds,
            extra_file_extensions: self.hints.extra_file_extensions.clone(),
        }
    }

    /// The resolved colour theme — the one place the accents are
    /// decided.
    ///
    /// They live on the theme rather than in a section beside it
    /// because several of its fields are *derived from* the accent:
    /// `text_on_accent` picks light or dark by the accent's luminance,
    /// and `search_accent` defaults to it. A section that overwrote the
    /// accents from outside would leave those derived from the accent
    /// the named theme shipped with, so setting a dark accent over a
    /// light-accented theme would put near-black text on it.
    pub fn theme(&self) -> Theme {
        let dir = config_path().and_then(|p| p.parent().map(Path::to_path_buf));
        veter::theme::resolve(&self.theme, dir.as_deref())
    }

    /// Effective search-chrome colours: `[search]` where it is set,
    /// else the theme's — whose search accent defaults to `[theme]
    /// accents`, so setting an accent restyles the panel with it.
    pub fn search_colors(&self, theme: &Theme) -> [Rgba; 4] {
        [
            self.search.accent.unwrap_or_else(|| theme.search_accent()),
            self.search.bar_text.unwrap_or_else(|| theme.search_text()),
            self.search
                .current_match
                .unwrap_or_else(|| theme.search_current_match()),
            self.search
                .match_color
                .unwrap_or_else(|| theme.search_match()),
        ]
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

    /// A config that predates `[theme]` — and one that names a theme
    /// but pins nothing — must resolve exactly the colours veter drew
    /// before themes existed.
    #[test]
    fn a_theme_free_config_keeps_the_pre_theme_colours() {
        let bare: Config = toml::from_str("").unwrap();
        let theme = bare.theme();
        assert_eq!(theme.background, Rgba::rgb(30, 30, 30));
        assert_eq!(theme.accent_primary(), Rgba::rgb(0x56, 0x79, 0x9f));
        assert_eq!(
            bare.search_colors(&theme),
            [
                Rgba::rgb(0x56, 0x79, 0x9f),
                Rgba::rgb(230, 230, 230),
                Rgba::rgb(220, 160, 0),
                Rgba::rgb(80, 80, 30),
            ]
        );
    }

    /// `[search]` is an override *over* the theme, not beside it, and
    /// the accents live on the theme itself: naming a theme re-accents
    /// every client, and setting `accents` holds them across a theme
    /// change.
    #[test]
    fn the_search_chrome_falls_through_to_the_theme() {
        let themed: Config = toml::from_str("[theme]\nname = \"nord\"\n").unwrap();
        let theme = themed.theme();
        assert_eq!(theme.accent_primary(), Rgba::rgb(0x88, 0xc0, 0xd0));
        // The search panel follows the theme's accent with it.
        assert_eq!(themed.search_colors(&theme)[0], Rgba::rgb(0x88, 0xc0, 0xd0));

        let pinned: Config = toml::from_str(
            "[theme]\nname = \"nord\"\naccents = [\"#ff0000\"]\n[search]\nmatch = \"#010203\"\n",
        )
        .unwrap();
        let theme = pinned.theme();
        assert_eq!(theme.accent_primary(), Rgba::rgb(255, 0, 0));
        // …and the panel with it, since its accent resolves through
        // the theme's own accent slot.
        assert_eq!(pinned.search_colors(&theme)[0], Rgba::rgb(255, 0, 0));
        assert_eq!(pinned.search_colors(&theme)[3], Rgba::rgb(1, 2, 3));
        // The unpinned search keys still come from nord.
        assert_eq!(pinned.search_colors(&theme)[1], theme.search_text());
    }

    /// The accent is not just published — several theme fields are
    /// *derived from* it. Setting one of the opposite lightness has to
    /// carry those with it, or a client paints near-black text on a
    /// dark accent fill.
    #[test]
    fn a_pinned_accent_carries_the_fields_derived_from_it() {
        // nord's accent is a light frost blue, so text over it is dark.
        let light: Config = toml::from_str("[theme]\nname = \"nord\"\n").unwrap();
        assert!(light.theme().text_on_accent().luminance() < 0.25);

        // A dark accent must flip it to light text.
        let dark: Config =
            toml::from_str("[theme]\nname = \"nord\"\naccents = [\"#2a2f6e\"]\n").unwrap();
        assert!(dark.theme().text_on_accent().luminance() > 0.8);
    }

    /// The pre-`[theme]` spelling of the search-panel tint still works.
    #[test]
    fn the_bar_bg_alias_survives() {
        let cfg: Config = toml::from_str("[search]\nbar_bg = \"#0a0b0c\"\n").unwrap();
        let theme = cfg.theme();
        assert_eq!(cfg.search_colors(&theme)[0], Rgba::rgb(10, 11, 12));
    }

    #[test]
    fn font_section_is_optional_and_partial() {
        // A config predating the section keeps the built-in symbol
        // fallbacks — that is what fixes icon glyphs out of the box.
        let bare: Config = toml::from_str("").unwrap();
        assert_eq!(bare.font.family, "monospace");
        assert!(bare.font.fallback.is_empty());

        // Naming one key leaves the other at its default.
        let partial: Config = toml::from_str("[font]\nfamily = \"Iosevka\"\n").unwrap();
        assert_eq!(partial.font.family, "Iosevka");
        assert!(partial.font.fallback.is_empty());

        let pinned: Config =
            toml::from_str("[font]\nfallback = [\"Symbols Nerd Font\"]\n").unwrap();
        assert_eq!(pinned.font.fallback, ["Symbols Nerd Font"]);
        assert_eq!(pinned.font.family, "monospace");
    }

    #[test]
    fn default_config_parses_all_chords() {
        // build() must never hit its error branch for the defaults.
        let keys = KeyBindings::build(&KeyBindingsConfig::default(), &[]);
        assert_eq!(keys.host.len(), 8);
        assert_eq!(keys.search.len(), 7);
        assert!(keys.selection.is_empty());
    }

    #[test]
    fn hint_kinds_default_to_the_built_in_order() {
        let cfg = Config::default();
        assert_eq!(cfg.hint_config().kinds, HintKind::DEFAULT_ORDER.to_vec());
    }

    #[test]
    fn hint_kinds_follow_the_configured_order_and_drop_junk() {
        let cfg: Config = toml::from_str(
            r#"
            [hints]
            kinds = ["path", "nonsense", "url", "path"]
            "#,
        )
        .unwrap();
        // Order preserved, unknown name skipped, duplicate ignored.
        assert_eq!(cfg.hint_config().kinds, vec![HintKind::Path, HintKind::Url]);
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
            keys.resolve_selection_action(&Key::Character("o".into()), &none),
            Some(SelectionAction::Command(
                r#"xdg-open "$VETER_SELECTION""#.into()
            ))
        );
        assert_eq!(
            keys.resolve_selection_action(&Key::Character("b".into()), &ctrl),
            Some(SelectionAction::Command(r#"firefox "$1""#.into()))
        );
        // Unbound key → None.
        assert!(keys
            .resolve_selection_action(&Key::Character("z".into()), &none)
            .is_none());
    }

    #[test]
    fn selection_input_binds_and_expands() {
        let cfg: Config = toml::from_str(
            r#"
            [[selection_commands]]
            key = "C"
            input = "cd %q\n"
            "#,
        )
        .unwrap();
        let keys = cfg.key_bindings();
        let template = match keys
            .resolve_selection_action(&Key::Character("C".into()), &ModifiersState::empty())
        {
            Some(SelectionAction::Input(t)) => t,
            other => panic!("expected an input action, got {other:?}"),
        };
        // TOML's `\n` is already a newline by the time we see it.
        assert_eq!(template, "cd %q\n");
        assert_eq!(expand_selection_input(&template, "veter", None), "cd 'veter'\n");
    }

    #[test]
    fn expand_selection_input_placeholders() {
        // Bare `%` is verbatim, `%q` quotes, `%%` is a literal percent.
        assert_eq!(expand_selection_input("cd %", "veter", None), "cd veter");
        assert_eq!(expand_selection_input("cd %q", "my dir", None), "cd 'my dir'");
        assert_eq!(expand_selection_input("cd %q", "it's", None), r#"cd 'it'\''s'"#);
        assert_eq!(expand_selection_input("100%% of %", "x", None), "100% of x");
        // A trailing `%` still substitutes; a template with no
        // placeholder is sent as-is.
        assert_eq!(expand_selection_input("echo %", "hi", None), "echo hi");
        assert_eq!(expand_selection_input("clear\n", "hi", None), "clear\n");
        // Every occurrence is replaced, not just the first.
        assert_eq!(expand_selection_input("% %", "a", None), "a a");
    }

    #[test]
    fn expand_selection_input_kind_placeholder() {
        assert_eq!(
            expand_selection_input("open %k %q", "/tmp/a", Some(HintKind::Path)),
            "open path '/tmp/a'"
        );
        // A selection that didn't come from a hint has no kind, and `%k`
        // expands to nothing rather than to a placeholder word.
        assert_eq!(
            expand_selection_input("open %k %q", "/tmp/a", None),
            "open  '/tmp/a'"
        );
    }

    #[test]
    fn selection_entry_needs_exactly_one_of_command_and_input() {
        let cfg: Config = toml::from_str(
            r#"
            [[selection_commands]]
            key = "a"

            [[selection_commands]]
            key = "b"
            command = "true"
            input = "true\n"

            [[selection_commands]]
            key = "c"
            input = "true\n"
            "#,
        )
        .unwrap();
        // Only the last entry is well-formed.
        let keys = cfg.key_bindings();
        assert_eq!(keys.selection.len(), 1);
        assert!(keys
            .resolve_selection_action(&Key::Character("c".into()), &ModifiersState::empty())
            .is_some());
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
            .resolve_selection_action(&Key::Character("o".into()), &ModifiersState::empty())
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
