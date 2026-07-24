//! Full previews for the overlay that opens on Enter.
//!
//! Text and source files show their head, tab-expanded and clipped to
//! the modal's width; directories show their listing; anything else
//! shows what we know about it (type, size, and for pictures the pixel
//! dimensions). Everything is bounded — a preview never reads more than
//! [`MAX_PREVIEW_BYTES`] off disk, so opening a 4 GB log is instant.

use std::path::Path;

use crate::entry::{Entry, ListOpts, Media, human_size, read_dir};

/// Most bytes a text preview reads.
pub const MAX_PREVIEW_BYTES: usize = 128 * 1024;
/// Most lines a preview shows.
pub const MAX_PREVIEW_LINES: usize = 400;
/// Bytes sniffed to decide whether a file is text at all.
const SNIFF_BYTES: usize = 8 * 1024;

/// Build the modal body for `entry`: the title line first (the modal
/// draws it in its strip), then the content lines.
pub fn lines(entry: &Entry, width: usize, opts: &ListOpts) -> Vec<String> {
    let title = format!("{}  ·  {}", entry.name, entry.size_label());
    let mut out = vec![title];
    match entry.media() {
        Media::Dir => out.extend(dir_lines(&entry.path, opts)),
        Media::Image | Media::Video => out.extend(binary_lines(entry)),
        Media::Text => out.extend(text_lines(&entry.path, width)),
        _ => {
            // Unknown extension: sniff it, since plenty of text files
            // (READMEs, dotfiles, scripts) have none.
            if is_probably_text(&entry.path) {
                out.extend(text_lines(&entry.path, width));
            } else {
                out.extend(binary_lines(entry));
            }
        }
    }
    if out.len() == 1 {
        out.push("  (empty)".into());
    }
    out
}

/// A directory's own listing, one entry per line.
fn dir_lines(path: &Path, opts: &ListOpts) -> Vec<String> {
    // The preview ignores any active name filter — it describes the
    // directory, not the search.
    let opts = ListOpts {
        filter: String::new(),
        ..opts.clone()
    };
    match read_dir(path, &opts) {
        Ok(entries) => {
            let total = entries.len();
            let mut out: Vec<String> = entries
                .iter()
                .take(MAX_PREVIEW_LINES)
                .map(|e| {
                    let marker = if e.is_dir() { "/" } else { " " };
                    format!("  {}{:<40} {}", marker, e.name, e.size_label())
                })
                .collect();
            if total > out.len() {
                out.push(format!("  … {} more", total - out.len()));
            }
            out
        }
        Err(e) => vec![format!("  cannot read directory: {e}")],
    }
}

/// The head of a text file, tabs expanded and lines clipped to `width`.
fn text_lines(path: &Path, width: usize) -> Vec<String> {
    let Ok(bytes) = read_head(path) else {
        return vec!["  cannot read file".into()];
    };
    let text = String::from_utf8_lossy(&bytes);
    let clip = width.saturating_sub(4).max(8);
    let mut out: Vec<String> = text
        .lines()
        .take(MAX_PREVIEW_LINES)
        .map(|l| {
            let expanded = l.replace('\t', "    ");
            let mut s: String = expanded.chars().take(clip).collect();
            if expanded.chars().count() > clip {
                s.push('…');
            }
            format!("  {s}")
        })
        .collect();
    if bytes.len() == MAX_PREVIEW_BYTES {
        out.push("  … truncated".into());
    }
    out
}

/// What we can say about a file we can't render as text.
fn binary_lines(entry: &Entry) -> Vec<String> {
    let mut out = vec![
        format!("  type    {}", media_label(entry.media())),
        format!("  size    {}", human_size(entry.size)),
    ];
    if let Some(ext) = entry.ext() {
        out.push(format!("  ext     .{ext}"));
    }
    if entry.media() == Media::Image {
        if let Some((w, h)) = image_dimensions(&entry.path) {
            out.push(format!("  pixels  {w} × {h}"));
        }
    }
    out.push(String::new());
    out.push("  (no text preview for this file type)".into());
    out
}

fn media_label(m: Media) -> &'static str {
    match m {
        Media::Dir => "directory",
        Media::Image => "image",
        Media::Video => "video",
        Media::Audio => "audio",
        Media::Text => "text",
        Media::Archive => "archive",
        Media::Binary => "binary",
    }
}

/// Read at most [`MAX_PREVIEW_BYTES`] from the front of `path`.
fn read_head(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; MAX_PREVIEW_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Heuristic every pager uses: a NUL byte early on means binary.
pub fn is_probably_text(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; SNIFF_BYTES];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    n > 0 && !buf[..n].contains(&0)
}

/// Pixel dimensions without decoding the whole picture.
fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Kind;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfm-prev-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry_for(path: &Path) -> Entry {
        let meta = std::fs::metadata(path).unwrap();
        Entry {
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path: path.to_path_buf(),
            kind: if meta.is_dir() { Kind::Dir } else { Kind::File },
            size: meta.len(),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            is_link: false,
        }
    }

    #[test]
    fn text_preview_clips_long_lines_and_expands_tabs() {
        let d = tmpdir("text");
        let p = d.join("a.rs");
        std::fs::write(&p, "\tfn main() {}\n".to_string() + &"x".repeat(500)).unwrap();
        let out = lines(&entry_for(&p), 40, &ListOpts::default());
        assert!(out[1].starts_with("      fn main"), "{:?}", out[1]);
        assert!(out[2].chars().count() <= 40, "clipped to the modal width");
        assert!(out[2].ends_with('…'));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn directory_preview_lists_children_with_a_dir_marker() {
        let d = tmpdir("dir");
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("f.txt"), b"hello").unwrap();
        let out = lines(&entry_for(&d), 60, &ListOpts::default());
        assert!(out[1].contains("/sub"), "{:?}", out[1]);
        assert!(out[2].contains("f.txt") && out[2].contains("5B"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn binary_files_report_metadata_instead_of_bytes() {
        let d = tmpdir("bin");
        let p = d.join("blob.dat");
        std::fs::write(&p, [0u8, 1, 2, 3]).unwrap();
        let out = lines(&entry_for(&p), 60, &ListOpts::default());
        assert!(out.iter().any(|l| l.contains("type")));
        assert!(out.iter().any(|l| l.contains("no text preview")));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn extensionless_text_is_sniffed_and_previewed() {
        let d = tmpdir("sniff");
        let p = d.join("README");
        std::fs::write(&p, "plain words\n").unwrap();
        assert!(is_probably_text(&p));
        let out = lines(&entry_for(&p), 60, &ListOpts::default());
        assert!(out[1].contains("plain words"));

        let b = d.join("blob");
        std::fs::write(&b, [b'a', 0, b'b']).unwrap();
        assert!(!is_probably_text(&b));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_empty_file_still_produces_a_body() {
        let d = tmpdir("empty");
        let p = d.join("empty.txt");
        std::fs::write(&p, b"").unwrap();
        let out = lines(&entry_for(&p), 60, &ListOpts::default());
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("empty"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
