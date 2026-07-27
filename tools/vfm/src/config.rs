//! vfm's config file: how to open files.
//!
//! TOML, loaded from `$XDG_CONFIG_HOME/vfm/config.toml` (else
//! `~/.config/vfm/config.toml`), overridable with `--config`. Every
//! field is optional; a missing file is silent and an unreadable /
//! malformed one logs one line and falls back to built-in defaults —
//! the same non-fatal contract veter's own config uses.
//!
//! The only thing configured today is *opening*: which program handles a
//! file, and whether it runs in this terminal (an editor, vplay) or
//! detached (a GUI app like xdg-open). Resolution for a file is
//! `[open.ext].<ext>` → `[open].<media>` → a media built-in →
//! `[open].default` → the `xdg-open` fallback.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::entry::{Entry, Media};

/// One open rule: a command template plus whether it takes over the
/// terminal.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenRule {
    /// Shell command run via `$SHELL -c`. Each `%` is replaced by the
    /// file path (shell-quoted); if there is no `%`, the path is
    /// appended. Because it goes through the shell, `$EDITOR`, pipes and
    /// globs all work.
    pub command: String,
    /// `true`: run in this terminal — vfm tears down its UI, waits for
    /// the program to exit, then restores. For editors, pagers, vplay.
    /// `false`: launch detached (own session, null stdio) and keep
    /// browsing. For GUI apps / `xdg-open`.
    #[serde(default)]
    pub terminal: bool,
}

/// The `[open]` table: per-media rules, per-extension overrides, and a
/// catch-all. All optional — anything unset uses a built-in.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OpenConfig {
    pub image: Option<OpenRule>,
    pub video: Option<OpenRule>,
    pub audio: Option<OpenRule>,
    pub text: Option<OpenRule>,
    pub archive: Option<OpenRule>,
    pub binary: Option<OpenRule>,
    /// Catch-all for media with no specific rule or built-in
    /// (audio/archive/binary, and anything else).
    pub default: Option<OpenRule>,
    /// Highest-precedence overrides keyed by lowercase extension.
    pub ext: HashMap<String, OpenRule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub open: OpenConfig,
}

/// A resolved, ready-to-run open action: the final shell command line
/// (already `%`-substituted) and how to run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOpen {
    pub command: String,
    pub terminal: bool,
}

impl Config {
    /// Load from `path`. Missing → silent defaults; read/parse error →
    /// one stderr line + defaults. Never fails.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
            Err(e) => {
                eprintln!("vfm: config: cannot read {}: {e}", path.display());
                return Config::default();
            }
        };
        match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("vfm: config: parse error in {}: {e}", path.display());
                Config::default()
            }
        }
    }

    /// The command that opens `entry`, or `None` for a directory (those
    /// navigate, they don't open).
    pub fn resolve(&self, entry: &Entry) -> Option<ResolvedOpen> {
        let media = entry.media();
        if media == Media::Dir {
            return None;
        }
        let by_ext = entry
            .ext()
            .and_then(|e| self.open.ext.get(&e))
            .cloned();
        let by_media = match media {
            Media::Image => &self.open.image,
            Media::Video => &self.open.video,
            Media::Audio => &self.open.audio,
            Media::Text => &self.open.text,
            Media::Archive => &self.open.archive,
            Media::Binary => &self.open.binary,
            Media::Dir => &None,
        };
        let rule = by_ext
            .or_else(|| by_media.clone())
            .or_else(|| builtin_media(media))
            .or_else(|| self.open.default.clone())
            .unwrap_or_else(builtin_default);
        Some(ResolvedOpen {
            command: substitute(&rule.command, &entry.path),
            terminal: rule.terminal,
        })
    }
}

/// Build a one-off open from a command typed at the palette (`:open
/// [-t] CMD…`) rather than the config. `%` in `template` is replaced by
/// `path` (shell-quoted; appended if absent), exactly as for a config
/// rule.
pub fn resolve_command(template: &str, terminal: bool, path: &Path) -> ResolvedOpen {
    ResolvedOpen {
        command: substitute(template, path),
        terminal,
    }
}

/// `$XDG_CONFIG_HOME/vfm/config.toml`, else `~/.config/vfm/config.toml`.
/// `None` if neither env var is set (→ built-in defaults).
pub fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir).join("vfm").join("config.toml"));
    }
    let home = std::env::var_os("HOME").filter(|s| !s.is_empty())?;
    Some(PathBuf::from(home).join(".config").join("vfm").join("config.toml"))
}

/// Built-in handler for the media types veter has native tools for.
/// `None` for the rest, which fall through to `[open].default` /
/// [`builtin_default`].
fn builtin_media(media: Media) -> Option<OpenRule> {
    let t = |command: String, terminal: bool| Some(OpenRule { command, terminal });
    match media {
        // vplay is veter's image *and video* viewer, run in-terminal.
        Media::Image | Media::Video => t("vplay %".into(), true),
        // The user's editor, in this terminal; resolved now so it works
        // even when $EDITOR is unset.
        Media::Text => t(format!("{} %", editor()), true),
        _ => None,
    }
}

/// The last-resort handler: hand it to the desktop, detached.
fn builtin_default() -> OpenRule {
    OpenRule {
        command: "xdg-open %".into(),
        terminal: false,
    }
}

/// `$VISUAL`, else `$EDITOR`, else `vi`.
fn editor() -> String {
    for key in ["VISUAL", "EDITOR"] {
        if let Some(v) = std::env::var_os(key).filter(|s| !s.is_empty()) {
            return v.to_string_lossy().into_owned();
        }
    }
    "vi".into()
}

/// Replace each `%` in `template` with the shell-quoted `path`; if there
/// is no `%`, append it. The result is a `$SHELL -c` line.
fn substitute(template: &str, path: &Path) -> String {
    let quoted = shell_quote(&path.to_string_lossy());
    if template.contains('%') {
        template.replace('%', &quoted)
    } else {
        format!("{template} {quoted}")
    }
}

/// POSIX single-quote a string: wrap in `'…'`, and render any embedded
/// `'` as `'\''`. Safe for arbitrary bytes including spaces and `$`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Kind;
    use std::time::SystemTime;

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from(format!("/files/{name}")),
            kind: Kind::File,
            size: 0,
            mtime: SystemTime::UNIX_EPOCH,
            is_link: false,
        }
    }

    fn cfg(toml_src: &str) -> Config {
        toml::from_str(toml_src).expect("valid toml")
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn substitute_replaces_percent_or_appends() {
        let p = Path::new("/a/b c.mp4");
        assert_eq!(substitute("mpv %", p), "mpv '/a/b c.mp4'");
        assert_eq!(substitute("feh --", p), "feh -- '/a/b c.mp4'");
        assert_eq!(substitute("cp % /dst/%", p), "cp '/a/b c.mp4' /dst/'/a/b c.mp4'");
    }

    #[test]
    fn builtins_apply_when_nothing_is_configured() {
        let c = Config::default();
        let img = c.resolve(&entry("p.png")).unwrap();
        assert_eq!(img.command, "vplay '/files/p.png'");
        assert!(img.terminal);

        let vid = c.resolve(&entry("m.mkv")).unwrap();
        assert_eq!(vid.command, "vplay '/files/m.mkv'");
        assert!(vid.terminal);

        // Audio/archive/binary have no built-in → xdg-open, detached.
        let bin = c.resolve(&entry("blob.bin")).unwrap();
        assert_eq!(bin.command, "xdg-open '/files/blob.bin'");
        assert!(!bin.terminal);
    }

    #[test]
    fn text_default_uses_the_editor_env_with_a_vi_fallback() {
        // SAFETY: single-threaded test; we set then clear.
        unsafe {
            std::env::remove_var("VISUAL");
            std::env::set_var("EDITOR", "nvim");
        }
        let c = Config::default();
        assert_eq!(c.resolve(&entry("a.rs")).unwrap().command, "nvim '/files/a.rs'");
        unsafe {
            std::env::remove_var("EDITOR");
        }
        assert_eq!(c.resolve(&entry("a.rs")).unwrap().command, "vi '/files/a.rs'");
    }

    #[test]
    fn ext_override_beats_media_beats_default() {
        let c = cfg(
            r#"
            [open]
            image = { command = "feh %", terminal = false }
            default = { command = "handlr open %", terminal = false }
            [open.ext]
            png = { command = "gimp %", terminal = false }
            "#,
        );
        // ext wins over the media rule
        assert_eq!(c.resolve(&entry("p.png")).unwrap().command, "gimp '/files/p.png'");
        // media rule (no ext override for jpg) wins over built-in
        assert_eq!(c.resolve(&entry("p.jpg")).unwrap().command, "feh '/files/p.jpg'");
        // audio has no media rule / built-in → user default
        assert_eq!(
            c.resolve(&entry("s.mp3")).unwrap().command,
            "handlr open '/files/s.mp3'"
        );
    }

    #[test]
    fn a_terminal_flag_round_trips() {
        let c = cfg(r#"[open]
            text = { command = "hx %", terminal = true }
        "#);
        let r = c.resolve(&entry("a.txt")).unwrap();
        assert_eq!(r.command, "hx '/files/a.txt'");
        assert!(r.terminal);
    }

    #[test]
    fn directories_never_resolve_to_an_open() {
        let mut e = entry("sub");
        e.kind = Kind::Dir;
        assert!(Config::default().resolve(&e).is_none());
    }

    #[test]
    fn a_malformed_config_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("vfm-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, b"this is not = = valid toml").unwrap();
        let c = Config::load(&p);
        // Still resolves via built-ins.
        assert_eq!(c.resolve(&entry("p.png")).unwrap().command, "vplay '/files/p.png'");
        // A missing file is also fine.
        assert!(Config::load(&dir.join("nope.toml")).open.ext.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
