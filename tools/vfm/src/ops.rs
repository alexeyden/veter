//! File operations, run off the event loop.
//!
//! Copying a directory tree can take minutes; doing it inline would
//! freeze the grid mid-repaint, so every mutation goes to a worker
//! thread and its outcome comes back as a message the status line
//! reports. The worker runs one operation at a time — file operations
//! are not something to parallelise behind the user's back.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

#[derive(Debug, Clone)]
pub enum Op {
    Copy { srcs: Vec<PathBuf>, dst: PathBuf },
    Move { srcs: Vec<PathBuf>, dst: PathBuf },
    Delete { paths: Vec<PathBuf> },
    Mkdir { path: PathBuf },
    Rename { from: PathBuf, to: PathBuf },
}

impl Op {
    /// Present-tense description for the "working" status line.
    pub fn verb(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "copying",
            Op::Move { .. } => "moving",
            Op::Delete { .. } => "deleting",
            Op::Mkdir { .. } => "creating directory",
            Op::Rename { .. } => "renaming",
        }
    }
}

/// What the worker reports when an operation finishes.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub message: String,
    pub failed: bool,
}

pub struct Runner {
    tx: Sender<Op>,
    rx: Receiver<Outcome>,
    /// Set while an operation is in flight, so the UI can say so and
    /// refuse to queue a conflicting one.
    busy: Option<String>,
}

impl Runner {
    pub fn new() -> Self {
        let (op_tx, op_rx) = channel::<Op>();
        let (out_tx, out_rx) = channel::<Outcome>();
        std::thread::spawn(move || {
            while let Ok(op) = op_rx.recv() {
                let outcome = run(op);
                if out_tx.send(outcome).is_err() {
                    return;
                }
            }
        });
        Runner {
            tx: op_tx,
            rx: out_rx,
            busy: None,
        }
    }

    pub fn busy(&self) -> Option<&str> {
        self.busy.as_deref()
    }

    /// Queue `op`. Refused (with a message) while another is running.
    pub fn submit(&mut self, op: Op) -> Result<String, String> {
        if let Some(busy) = &self.busy {
            return Err(format!("busy: {busy}"));
        }
        let verb = op.verb().to_string();
        self.busy = Some(verb.clone());
        self.tx
            .send(op)
            .map_err(|_| "file-operation worker is gone".to_string())?;
        Ok(verb)
    }

    /// The outcome of the running operation, once it lands.
    pub fn poll(&mut self) -> Option<Outcome> {
        match self.rx.try_recv() {
            Ok(o) => {
                self.busy = None;
                Some(o)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.busy = None;
                None
            }
        }
    }
}

fn run(op: Op) -> Outcome {
    let result = match &op {
        Op::Copy { srcs, dst } => transfer(srcs, dst, false),
        Op::Move { srcs, dst } => transfer(srcs, dst, true),
        Op::Delete { paths } => delete(paths),
        Op::Mkdir { path } => std::fs::create_dir_all(path)
            .map(|_| format!("created {}", name_of(path)))
            .map_err(|e| e.to_string()),
        Op::Rename { from, to } => {
            if to.exists() {
                Err(format!("{} already exists", name_of(to)))
            } else {
                std::fs::rename(from, to)
                    .map(|_| format!("renamed to {}", name_of(to)))
                    .map_err(|e| e.to_string())
            }
        }
    };
    match result {
        Ok(message) => Outcome {
            message,
            failed: false,
        },
        Err(e) => Outcome {
            message: format!("{}: {e}", op.verb()),
            failed: true,
        },
    }
}

/// Copy (or move) `srcs` into the directory `dst`.
fn transfer(srcs: &[PathBuf], dst: &Path, remove_source: bool) -> Result<String, String> {
    let mut done = 0usize;
    for src in srcs {
        let target = unique_target(dst, src);
        if remove_source {
            // A plain rename is the fast path; it fails across devices,
            // where we fall back to a copy and then unlink.
            match std::fs::rename(src, &target) {
                Ok(()) => {
                    done += 1;
                    continue;
                }
                Err(e) if e.raw_os_error() != Some(libc::EXDEV) => {
                    return Err(format!("{}: {e}", name_of(src)));
                }
                Err(_) => {}
            }
        }
        copy_tree(src, &target).map_err(|e| format!("{}: {e}", name_of(src)))?;
        if remove_source {
            remove_tree(src).map_err(|e| format!("{}: {e}", name_of(src)))?;
        }
        done += 1;
    }
    let verb = if remove_source { "moved" } else { "copied" };
    Ok(format!("{verb} {done} item{}", plural(done)))
}

fn delete(paths: &[PathBuf]) -> Result<String, String> {
    let mut done = 0usize;
    for p in paths {
        remove_tree(p).map_err(|e| format!("{}: {e}", name_of(p)))?;
        done += 1;
    }
    Ok(format!("deleted {done} item{}", plural(done)))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// `dst/<name of src>`, suffixed when something is already there, so a
/// paste into the source directory duplicates rather than clobbers.
pub fn unique_target(dst: &Path, src: &Path) -> PathBuf {
    let name = src
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("unnamed"));
    let direct = dst.join(&name);
    if !direct.exists() {
        return direct;
    }
    let base = Path::new(&name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string_lossy().into_owned());
    let ext = Path::new(&name)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..10_000 {
        let candidate = dst.join(format!("{base}_{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    direct
}

/// Recursive copy. Symlinks are recreated rather than followed, so a
/// link into a huge tree doesn't silently become a deep copy.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        return std::os::unix::fs::symlink(target, dst);
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for item in std::fs::read_dir(src)? {
            let item = item?;
            copy_tree(&item.path(), &dst.join(item.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

fn remove_tree(p: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(p)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfm-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn copy_duplicates_a_tree_and_leaves_the_source() {
        let d = tmpdir("copy");
        let src = d.join("tree");
        std::fs::create_dir_all(src.join("inner")).unwrap();
        std::fs::write(src.join("inner/a.txt"), b"hi").unwrap();
        let dst = d.join("dest");
        std::fs::create_dir(&dst).unwrap();

        transfer(&[src.clone()], &dst, false).unwrap();
        assert_eq!(
            std::fs::read(dst.join("tree/inner/a.txt")).unwrap(),
            b"hi".to_vec()
        );
        assert!(src.exists(), "source survives a copy");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn move_removes_the_source() {
        let d = tmpdir("move");
        let f = d.join("f.txt");
        std::fs::write(&f, b"x").unwrap();
        let dst = d.join("dest");
        std::fs::create_dir(&dst).unwrap();
        transfer(&[f.clone()], &dst, true).unwrap();
        assert!(!f.exists());
        assert!(dst.join("f.txt").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn pasting_beside_the_original_suffixes_instead_of_clobbering() {
        let d = tmpdir("dup");
        let f = d.join("note.txt");
        std::fs::write(&f, b"first").unwrap();
        transfer(&[f.clone()], &d, false).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"first".to_vec(), "untouched");
        assert!(d.join("note_1.txt").exists());
        transfer(&[f.clone()], &d, false).unwrap();
        assert!(d.join("note_2.txt").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn symlinks_are_recreated_not_followed() {
        let d = tmpdir("link");
        let target = d.join("real.txt");
        std::fs::write(&target, b"data").unwrap();
        let link = d.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dst = d.join("dest");
        std::fs::create_dir(&dst).unwrap();

        transfer(&[link], &dst, false).unwrap();
        let copied = dst.join("link.txt");
        assert!(std::fs::symlink_metadata(&copied).unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn delete_handles_files_and_trees() {
        let d = tmpdir("del");
        let f = d.join("f.txt");
        std::fs::write(&f, b"x").unwrap();
        let tree = d.join("tree/inner");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("b"), b"y").unwrap();

        delete(&[f.clone(), d.join("tree")]).unwrap();
        assert!(!f.exists());
        assert!(!d.join("tree").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rename_refuses_to_overwrite() {
        let d = tmpdir("rename");
        let a = d.join("a");
        let b = d.join("b");
        std::fs::write(&a, b"1").unwrap();
        std::fs::write(&b, b"2").unwrap();
        let out = run(Op::Rename {
            from: a.clone(),
            to: b.clone(),
        });
        assert!(out.failed, "{out:?}");
        assert_eq!(std::fs::read(&b).unwrap(), b"2".to_vec());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_failing_operation_reports_rather_than_panics() {
        let out = run(Op::Delete {
            paths: vec![PathBuf::from("/definitely/not/here")],
        });
        assert!(out.failed);
        assert!(out.message.contains("deleting"));
    }
}
