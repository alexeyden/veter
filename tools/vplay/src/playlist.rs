//! The files sitting next to the opened one — the list `n`/`p` cycle
//! through: sibling stills in image mode (the left/right arrows too),
//! sibling videos in video mode.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Extensions [`crate::image_src::load_image`] can decode: the `image`
/// crate is built with png/jpeg/webp only. `gif` is deliberately absent —
/// [`VIDEO_EXTS`] claims it, so a gif opens in video mode and never takes
/// part in a still playlist.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "jpe", "jfif", "webp"];

/// Extensions that open in video mode unless `--image` says otherwise.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "mpg", "mpeg", "wmv", "flv", "ts", "gif", "ogv",
    "3gp",
];

fn has_ext(p: &Path, exts: &[&str]) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
}

pub fn is_image_ext(p: &Path) -> bool {
    has_ext(p, IMAGE_EXTS)
}

pub fn is_video_ext(p: &Path) -> bool {
    has_ext(p, VIDEO_EXTS)
}

/// The opened file's siblings of the same kind in lexicographical order,
/// with a cursor on the one currently displayed.
///
/// Names are stored relative to the opened path's parent so
/// [`Self::current`] reproduces the caller's path form (`foo.png` stays
/// `foo.png` rather than becoming `./foo.png`). The directory is scanned
/// once, at startup: what a session cycles through is what was there when
/// it opened.
///
/// `names` is never empty — the opened file is always a member — so the
/// cursor always has a home and `current()` never panics.
pub struct Playlist {
    dir: PathBuf,
    names: Vec<OsString>,
    idx: usize,
}

impl Playlist {
    /// Scan `path`'s directory for the files `member` accepts (by
    /// extension — [`is_image_ext`] or [`is_video_ext`]), sorted by file
    /// name. `path` itself joins the list even when `member` rejects it
    /// (`--image` / `--video` force the mode), and an unreadable
    /// directory degrades to a one-entry list.
    pub fn scan(path: &Path, member: fn(&Path) -> bool) -> Self {
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let Some(self_name) = path.file_name().map(|n| n.to_os_string()) else {
            // No trailing file name (`..`, `/`) — nothing to scan
            // against, so the playlist is the path itself, verbatim.
            return Self {
                dir: PathBuf::new(),
                names: vec![path.as_os_str().to_os_string()],
                idx: 0,
            };
        };
        // An empty parent means the path was a bare file name; read the
        // working directory, but keep `dir` empty so joining round-trips.
        let target = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir.as_path()
        };
        let mut names: Vec<OsString> = std::fs::read_dir(target)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                // A directory called `foo.png` is not a still; follow
                // symlinks (metadata, not symlink_metadata) so a link to
                // an image counts as one.
                member(&e.path()) && e.metadata().is_ok_and(|m| m.is_file())
            })
            .map(|e| e.file_name())
            .collect();
        // Plain lexicographical order on the raw name bytes.
        names.sort_unstable();
        let idx = match names.binary_search(&self_name) {
            Ok(i) => i,
            Err(i) => {
                names.insert(i, self_name);
                i
            }
        };
        Self { dir, names, idx }
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn index(&self) -> usize {
        self.idx
    }

    pub fn set_index(&mut self, idx: usize) {
        self.idx = idx.min(self.len() - 1);
    }

    /// Move the cursor `delta` entries, wrapping at both ends, and return
    /// the path it now points at.
    pub fn step(&mut self, delta: isize) -> PathBuf {
        let n = self.len();
        self.idx = (self.idx + delta.rem_euclid(n as isize) as usize) % n;
        self.current()
    }

    pub fn current(&self) -> PathBuf {
        self.dir.join(&self.names[self.idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(names: &[&str], idx: usize) -> Playlist {
        Playlist {
            dir: PathBuf::from("/pics"),
            names: names.iter().map(OsString::from).collect(),
            idx,
        }
    }

    #[test]
    fn recognises_still_extensions() {
        assert!(is_image_ext(Path::new("a.png")));
        assert!(is_image_ext(Path::new("a.JPEG")));
        assert!(is_image_ext(Path::new("dir/a.webp")));
        // Video mode owns gif; other files and extensionless names are
        // not stills.
        assert!(!is_image_ext(Path::new("a.gif")));
        assert!(!is_image_ext(Path::new("a.mp4")));
        assert!(!is_image_ext(Path::new("README")));
    }

    #[test]
    fn recognises_video_extensions() {
        assert!(is_video_ext(Path::new("a.mp4")));
        assert!(is_video_ext(Path::new("a.MKV")));
        assert!(is_video_ext(Path::new("a.gif")));
        assert!(!is_video_ext(Path::new("a.png")));
        assert!(!is_video_ext(Path::new("README")));
    }

    #[test]
    fn step_wraps_both_ways() {
        let mut pl = playlist(&["a.png", "b.png", "c.png"], 0);
        assert_eq!(pl.step(1), PathBuf::from("/pics/b.png"));
        assert_eq!(pl.step(1), PathBuf::from("/pics/c.png"));
        assert_eq!(pl.step(1), PathBuf::from("/pics/a.png"));
        assert_eq!(pl.step(-1), PathBuf::from("/pics/c.png"));
        assert_eq!(pl.index(), 2);
    }

    #[test]
    fn step_on_lone_entry_stays_put() {
        let mut pl = playlist(&["only.png"], 0);
        assert_eq!(pl.step(1), PathBuf::from("/pics/only.png"));
        assert_eq!(pl.step(-1), PathBuf::from("/pics/only.png"));
        assert_eq!(pl.index(), 0);
    }

    #[test]
    fn scan_sorts_stills_and_finds_the_opened_file() {
        let dir = std::env::temp_dir().join(format!("vplay-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["b.png", "a.JPG", "c.webp", "notes.txt", "clip.mp4"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        std::fs::create_dir(dir.join("sub.png")).unwrap();

        let pl = Playlist::scan(&dir.join("b.png"), is_image_ext);
        assert_eq!(pl.len(), 3, "only the three stills; sub.png is a dir");
        assert_eq!(pl.index(), 1, "uppercase A sorts before lowercase b");
        assert_eq!(pl.current(), dir.join("b.png"));

        // A file the scan filters out still anchors the cursor, so
        // `--image` on an odd extension can cycle out of it.
        let pl = Playlist::scan(&dir.join("notes.txt"), is_image_ext);
        assert_eq!(pl.len(), 4);
        assert_eq!(pl.current(), dir.join("notes.txt"));

        // Video mode walks the videos instead.
        let pl = Playlist::scan(&dir.join("clip.mp4"), is_video_ext);
        assert_eq!(pl.len(), 1);
        assert_eq!(pl.current(), dir.join("clip.mp4"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
