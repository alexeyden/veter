//! The directory model: what a listing row is, how it is classified,
//! and how a directory is read, filtered and sorted.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

/// What an entry is, as far as navigation cares. Symlinks are resolved
/// once at listing time — a link to a directory navigates like one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    /// A symlink whose target could not be stat'ed (broken link).
    Broken,
}

/// What an entry is, as far as *previewing* cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Dir,
    Image,
    Video,
    Audio,
    Text,
    Archive,
    Binary,
}

/// Extensions we can decode into a thumbnail with the `image` crate.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];
/// Extensions worth handing to ffmpeg for a poster frame.
const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v", "mpg", "mpeg", "wmv"];
const AUDIO_EXTS: &[&str] = &["mp3", "flac", "ogg", "opus", "wav", "m4a", "aac"];
const ARCHIVE_EXTS: &[&str] = &[
    "zip", "tar", "gz", "xz", "bz2", "zst", "7z", "rar", "tgz", "deb", "rpm",
];
/// Extensions we are confident are text, so a preview can read them
/// without sniffing. Anything else is sniffed for NUL bytes on demand.
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "rs", "toml", "json", "yaml", "yml", "c", "h", "cpp", "hpp", "cc", "py", "sh",
    "zsh", "bash", "js", "ts", "tsx", "jsx", "go", "java", "rb", "pl", "lua", "vim", "css", "html",
    "xml", "ini", "conf", "cfg", "log", "csv", "tex", "sql", "make", "mk", "cmake", "excalidraw",
];

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    /// Size in bytes. Directories report whatever the filesystem says,
    /// which is not a child count — [`Entry::size_label`] shows "dir".
    pub size: u64,
    pub mtime: SystemTime,
    /// True when the entry itself is a symlink, whatever it points at.
    pub is_link: bool,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    /// Lowercased extension, if any.
    pub fn ext(&self) -> Option<String> {
        self.path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
    }

    pub fn media(&self) -> Media {
        if self.is_dir() {
            return Media::Dir;
        }
        let Some(ext) = self.ext() else {
            return Media::Binary;
        };
        let e = ext.as_str();
        if IMAGE_EXTS.contains(&e) {
            Media::Image
        } else if VIDEO_EXTS.contains(&e) {
            Media::Video
        } else if AUDIO_EXTS.contains(&e) {
            Media::Audio
        } else if ARCHIVE_EXTS.contains(&e) {
            Media::Archive
        } else if TEXT_EXTS.contains(&e) {
            Media::Text
        } else {
            Media::Binary
        }
    }

    /// True when this entry can produce a picture thumbnail.
    pub fn thumbnailable(&self) -> bool {
        matches!(self.media(), Media::Image | Media::Video)
    }

    /// Human-readable size — `4.2M`, `312K`, `18B`.
    pub fn size_label(&self) -> String {
        if self.is_dir() {
            return "dir".into();
        }
        human_size(self.size)
    }

    /// Modification time as the detail list shows it: `2026-07-30 14:02`,
    /// in the viewer's local time.
    pub fn mtime_label(&self) -> String {
        local_stamp(self.mtime)
    }
}

/// Format a `SystemTime` as `YYYY-MM-DD HH:MM` in local time. Times the
/// clock cannot place (a filesystem reporting something before the epoch,
/// or a broken link's placeholder) come back as dashes rather than a
/// misleading date.
pub fn local_stamp(t: SystemTime) -> String {
    let Ok(since) = t.duration_since(SystemTime::UNIX_EPOCH) else {
        return "—".into();
    };
    let secs = since.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` fills the caller's `tm` — no shared state and
    // no allocation, unlike `localtime`. A null return means the time is
    // out of range for the calendar, which the check below handles.
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return "—".into();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// Size in the compact form the status line and tiles use.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes}{}", UNITS[0])
    } else if v < 10.0 {
        format!("{v:.1}{}", UNITS[u])
    } else {
        format!("{v:.0}{}", UNITS[u])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Name,
    Size,
    Mtime,
    Ext,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Size => "size",
            Sort::Mtime => "mtime",
            Sort::Ext => "ext",
        }
    }
}

/// How a listing is filtered and ordered. Shared by every column so the
/// parent panes show the same view the grid does.
#[derive(Debug, Clone)]
pub struct ListOpts {
    pub show_hidden: bool,
    pub sort: Sort,
    pub reverse: bool,
    pub dirs_first: bool,
    /// Case-insensitive substring the name must contain. Applies to the
    /// current directory only; the caller clears it when navigating.
    pub filter: String,
}

impl Default for ListOpts {
    fn default() -> Self {
        ListOpts {
            show_hidden: false,
            sort: Sort::Name,
            reverse: false,
            dirs_first: true,
            filter: String::new(),
        }
    }
}

/// Read `dir`, applying `opts`. Unreadable children are skipped rather
/// than failing the whole listing — a directory with one bad symlink is
/// still worth showing.
pub fn read_dir(dir: &Path, opts: &ListOpts) -> Result<Vec<Entry>> {
    let rd = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let needle = opts.filter.to_lowercase();
    let mut out = Vec::new();
    for item in rd {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        if !opts.show_hidden && name.starts_with('.') {
            continue;
        }
        if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
            continue;
        }
        let path = item.path();
        let is_link = item
            .file_type()
            .map(|t| t.is_symlink())
            .unwrap_or(false);
        // Follow links so a link to a directory navigates like one; fall
        // back to the link's own metadata when the target is gone.
        let (kind, size, mtime) = match std::fs::metadata(&path) {
            Ok(m) => (
                if m.is_dir() { Kind::Dir } else { Kind::File },
                m.len(),
                m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            ),
            Err(_) => (Kind::Broken, 0, SystemTime::UNIX_EPOCH),
        };
        out.push(Entry {
            name,
            path,
            kind,
            size,
            mtime,
            is_link,
        });
    }
    sort_entries(&mut out, opts);
    Ok(out)
}

/// Order `entries` per `opts`. Directories float to the top when
/// `dirs_first`; `reverse` flips the comparison but never the grouping.
pub fn sort_entries(entries: &mut [Entry], opts: &ListOpts) {
    entries.sort_by(|a, b| {
        if opts.dirs_first {
            match (a.is_dir(), b.is_dir()) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
        }
        let ord = match opts.sort {
            Sort::Name => natural_cmp(&a.name, &b.name),
            Sort::Size => a.size.cmp(&b.size),
            Sort::Mtime => a.mtime.cmp(&b.mtime),
            Sort::Ext => a
                .ext()
                .unwrap_or_default()
                .cmp(&b.ext().unwrap_or_default())
                .then_with(|| natural_cmp(&a.name, &b.name)),
        };
        if opts.reverse { ord.reverse() } else { ord }
    });
}

/// Case-insensitive comparison that orders embedded digit runs
/// numerically, so `img2` sorts before `img10`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let xn = take_number(&mut ai);
                    let yn = take_number(&mut bi);
                    match xn.cmp(&yn) {
                        std::cmp::Ordering::Equal => continue,
                        ord => return ord,
                    }
                }
                let (xl, yl) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
                match xl.cmp(&yl) {
                    std::cmp::Ordering::Equal => {
                        ai.next();
                        bi.next();
                    }
                    ord => return ord,
                }
            }
        }
    }
}

/// Consume the digit run at the front of `it` as a number. Runs longer
/// than a u64 saturate, which only matters for absurd filenames.
fn take_number(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> u64 {
    let mut n: u64 = 0;
    while let Some(c) = it.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        n = n.saturating_mul(10).saturating_add((c as u8 - b'0') as u64);
        it.next();
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool, size: u64) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from(name),
            kind: if dir { Kind::Dir } else { Kind::File },
            size,
            mtime: SystemTime::UNIX_EPOCH,
            is_link: false,
        }
    }

    #[test]
    fn media_classification_follows_the_extension() {
        assert_eq!(entry("a.png", false, 0).media(), Media::Image);
        assert_eq!(entry("a.MP4", false, 0).media(), Media::Video);
        assert_eq!(entry("a.rs", false, 0).media(), Media::Text);
        assert_eq!(entry("a.tar.gz", false, 0).media(), Media::Archive);
        assert_eq!(entry("a.bin", false, 0).media(), Media::Binary);
        assert_eq!(entry("noext", false, 0).media(), Media::Binary);
        assert_eq!(entry("d", true, 0).media(), Media::Dir);
    }

    #[test]
    fn only_pictures_and_video_get_thumbnails() {
        assert!(entry("a.jpeg", false, 0).thumbnailable());
        assert!(entry("a.mkv", false, 0).thumbnailable());
        assert!(!entry("a.rs", false, 0).thumbnailable());
        assert!(!entry("d", true, 0).thumbnailable());
    }

    #[test]
    fn directories_float_above_files_whatever_the_sort() {
        let mut v = vec![
            entry("b.txt", false, 10),
            entry("a-dir", true, 0),
            entry("a.txt", false, 5),
        ];
        let mut opts = ListOpts::default();
        sort_entries(&mut v, &opts);
        assert_eq!(v[0].name, "a-dir");
        opts.sort = Sort::Size;
        opts.reverse = true;
        sort_entries(&mut v, &opts);
        assert_eq!(v[0].name, "a-dir", "grouping survives a reversed sort");
        assert_eq!(v[1].name, "b.txt");
    }

    #[test]
    fn names_sort_naturally_and_case_insensitively() {
        let mut v = vec![
            entry("img10.png", false, 0),
            entry("IMG2.png", false, 0),
            entry("img1.png", false, 0),
        ];
        sort_entries(&mut v, &ListOpts::default());
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["img1.png", "IMG2.png", "img10.png"]);
    }

    #[test]
    fn the_mtime_column_is_a_fixed_width_stamp() {
        let e = entry("a.txt", false, 0);
        let s = e.mtime_label();
        assert_eq!(s.chars().count(), 16, "{s}");
        // The epoch lands on 1970 in every timezone we could be in.
        assert!(s.starts_with("1969-") || s.starts_with("1970-"), "{s}");
        let later = Entry {
            mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000),
            ..entry("b.txt", false, 0)
        };
        assert!(later.mtime_label().starts_with("2026-"), "{}", later.mtime_label());
    }

    #[test]
    fn human_size_is_compact() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(999), "999B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(20 * 1024), "20K");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }

    #[test]
    fn listing_honours_hidden_and_filter() {
        let dir = std::env::temp_dir().join(format!("vfm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("visible.txt"), b"x").unwrap();
        std::fs::write(dir.join(".hidden"), b"x").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();

        let mut opts = ListOpts::default();
        let v = read_dir(&dir, &opts).unwrap();
        assert_eq!(v.len(), 2, "the dotfile is filtered out");
        assert_eq!(v[0].name, "sub");

        opts.show_hidden = true;
        assert_eq!(read_dir(&dir, &opts).unwrap().len(), 3);

        opts.filter = "VIS".into();
        let v = read_dir(&dir, &opts).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "visible.txt");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
