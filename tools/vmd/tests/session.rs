//! End-to-end: run the real `vmd` binary against a real pty, play the
//! terminal at it, then replay everything it wrote through the real
//! host engine.
//!
//! The unit tests already push vmd's draw commands through the protocol
//! crate's decoder, so what is left to cover is the part that only
//! exists once there is a process on a tty: the probe handshake vmd
//! blocks on before it draws anything, and whether the elements it then
//! creates actually land in a host's element table with the clip rect
//! and draw orders it meant. Both have failed silently in this repo
//! before — an unanswered probe looks like a hang, and a rejected
//! `CreateElement` fails atomically and looks like a blank screen.

use std::ffi::CStr;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use veter_host::vge::{VgeEngine, drive_terminal_stage};

const ROWS: u16 = 30;
const COLS: u16 = 100;
const CELL_W: u16 = 9;
const CELL_H: u16 = 20;

/// How long to wait for the child to say anything before giving up.
const TIMEOUT: Duration = Duration::from_secs(10);

const DOC: &str = "\
# The Title

Body text with **bold**, `code` and a [link](https://example.com).

## A section

- one
- two

```rust
fn main() {}
```

| a | b |
|---|--:|
| 1 | 2 |
";

/// A pty pair whose winsize carries pixel dimensions, so the client can
/// derive cell metrics from `TIOCGWINSZ` exactly as it does under a
/// real veter (VGE §11.1).
fn open_pty() -> (OwnedFd, String) {
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(m), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(m), 0, "unlockpt failed");
        let name = CStr::from_ptr(libc::ptsname(m))
            .to_str()
            .unwrap()
            .to_owned();
        let ws = libc::winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: COLS * CELL_W,
            ws_ypixel: ROWS * CELL_H,
        };
        assert_eq!(
            libc::ioctl(m, libc::TIOCSWINSZ, &ws),
            0,
            "TIOCSWINSZ failed"
        );
        (OwnedFd::from_raw_fd(m), name)
    }
}

/// The `ProbeResponse` a VGE-aware terminal would send back (§2.1),
/// wrapped in a terminal→client envelope.
fn probe_response() -> Vec<u8> {
    use vge_protocol::envelope::{ProbeBody, append_frame, wrap_t2c_envelope};
    use vge_protocol::frame::RSP_PROBE;

    let body = ProbeBody {
        protocol_version: 0,
        cell_pixel_width: CELL_W,
        cell_pixel_height: CELL_H,
        scale_factor: 1.0,
        max_elements: 4096,
        max_commands_per_element: 4096,
        max_text_bytes: 65536,
        max_image_bytes: 32 * 1024 * 1024,
        max_images: 1024,
        supported_image_encodings: 0x03,
        max_nesting_depth: 16,
    }
    .encode();
    let mut frames = Vec::new();
    append_frame(&mut frames, RSP_PROBE, 1, &body);
    wrap_t2c_envelope(&frames)
}

struct Terminal {
    master: std::fs::File,
    seen: Vec<u8>,
}

impl Terminal {
    fn new(master: &OwnedFd) -> Self {
        unsafe {
            let fl = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        Terminal {
            master: unsafe { std::fs::File::from_raw_fd(libc::dup(master.as_raw_fd())) },
            seen: Vec::new(),
        }
    }

    /// Pull whatever is waiting, without blocking.
    fn poll(&mut self) {
        let mut buf = [0u8; 65536];
        loop {
            match self.master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.seen.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    }

    /// Read until `want` returns true of everything seen so far.
    fn wait_for(&mut self, what: &str, want: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            self.poll();
            if want(&self.seen) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "timed out waiting for {what}; vmd wrote {} bytes: {:?}",
            self.seen.len(),
            String::from_utf8_lossy(&self.seen[..self.seen.len().min(400)])
        );
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }
}

/// Count the client→terminal VGE envelopes in a byte stream.
fn envelopes(bytes: &[u8]) -> usize {
    vge_protocol::apc::ApcStream::with_marker(*b"VGE")
        .feed(bytes)
        .payloads
        .len()
}

/// Spawn vmd on `path`, answer its probe, and return everything it
/// wrote up to the point the page was created — plus the terminal, so
/// the caller can keep talking to it.
fn run(path: &std::path::Path) -> (Terminal, std::process::Child) {
    let (master, slave) = open_pty();
    // `Terminal` dups the master, so the pty outlives this frame even
    // though the original fd is closed on the way out.
    let mut terminal = Terminal::new(&master);

    let open_slave = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave)
            .unwrap()
    };
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_vmd"))
        .arg(path)
        .stdin(open_slave())
        .stdout(open_slave())
        .stderr(open_slave())
        .env("TERM", "xterm-256color")
        .spawn()
        .expect("spawn vmd");

    // vmd blocks on the VGE probe before it draws anything, so nothing
    // else happens until the terminal answers.
    terminal.wait_for("the VGE probe", |seen| envelopes(seen) >= 1);
    terminal.send(&probe_response());
    // Then the page and the chrome. vmd also sends a PRT probe for the
    // host theme, which nothing here answers — it falls back to its own
    // colours after the timeout, which is part of what this checks.
    terminal.wait_for("the page element", |seen| {
        String::from_utf8_lossy(seen).contains("vmd.page")
    });
    (terminal, child)
}

/// Replay what vmd wrote through the real host engine, in order.
fn replay(bytes: &[u8]) -> VgeEngine {
    let mut engine = VgeEngine::new((CELL_W, CELL_H), 1.0);
    let mut parser = vt100::Parser::new(ROWS, COLS, 1000);
    engine.after_vt100_process(&mut parser);
    drive_terminal_stage(&mut engine, &mut parser, bytes, None);
    engine
}

fn write_doc(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("vmd-e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, DOC).unwrap();
    path
}

#[test]
fn the_page_and_chrome_land_in_a_real_host_element_table() {
    let path = write_doc("basic.md");
    let (mut terminal, mut child) = run(&path);

    // Let the first few frames settle, then snapshot the session *as
    // it was on screen* — quitting sweeps the namespace, so replaying
    // past the exit would find an empty table by design.
    std::thread::sleep(Duration::from_millis(120));
    terminal.poll();
    let on_screen = terminal.seen.clone();
    terminal.send(b"q");
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "vmd exited with {status:?}");

    let engine = replay(&on_screen);
    let elements = engine.state.elements();

    let page = elements
        .get("vmd.page")
        .expect("the page element is missing — a rejected CreateElement is atomic");
    assert!(
        !page.commands.is_empty(),
        "the page element carries no draw commands"
    );
    // The clip rect is what keeps a half-scrolled row off the header
    // and the status line (§9.2).
    let clip = page.clip_size.expect("the page must be clipped");
    assert!((clip.x - f32::from(COLS)).abs() < 1e-3, "{clip:?}");
    assert!((clip.y - f32::from(ROWS - 2)).abs() < 1e-3, "{clip:?}");

    let chrome = elements.get("vmd.chrome").expect("the chrome element");
    assert!(!chrome.commands.is_empty());
    assert!(
        chrome.draw_order > page.draw_order,
        "chrome must paint over the page"
    );

    // The document's own title reached the header bar.
    let text: Vec<&str> = chrome
        .commands
        .iter()
        .filter_map(|c| match c {
            vge_protocol::command::DrawCmd::DrawText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.iter().any(|t| t.contains("The Title")),
        "header bar should name the document: {text:?}"
    );
}

#[test]
fn a_heading_is_drawn_larger_than_the_body_text() {
    // The whole reason to draw a document with VGE rather than cells.
    let path = write_doc("scaled.md");
    let (mut terminal, mut child) = run(&path);
    std::thread::sleep(Duration::from_millis(120));
    terminal.poll();
    let on_screen = terminal.seen.clone();
    terminal.send(b"q");
    wait_for_exit(&mut child);

    let engine = replay(&on_screen);
    let page = engine.state.elements().get("vmd.page").unwrap();
    let mut title_scale = None;
    let mut body_scale = None;
    for cmd in &page.commands {
        if let vge_protocol::command::DrawCmd::DrawText {
            text, font_scale, ..
        } = cmd
        {
            if text.contains("The Title") {
                title_scale = Some(*font_scale);
            }
            if text.starts_with("Body text with") {
                body_scale = Some(*font_scale);
            }
        }
    }
    let title = title_scale.expect("the h1 should be on the page");
    let body = body_scale.expect("the paragraph should be on the page");
    assert!(title > body * 1.5, "h1 {title} vs body {body}");
    assert!((body - 1.0).abs() < 1e-3, "body text is the cell size");
}

#[test]
fn quitting_sweeps_the_namespace_it_created() {
    // The element and image tables outlive the process (§8.0), so a run
    // that leaves entries behind makes the *next* one collide with its
    // predecessor's ids.
    let path = write_doc("sweep.md");
    let (mut terminal, mut child) = run(&path);
    std::thread::sleep(Duration::from_millis(120));
    terminal.send(b"q");
    let status = wait_for_exit(&mut child);
    assert!(status.success());
    terminal.poll();

    let engine = replay(&terminal.seen);
    let left: Vec<&String> = engine
        .state
        .elements()
        .keys()
        .filter(|k| k.starts_with("vmd."))
        .collect();
    assert!(left.is_empty(), "vmd left elements behind: {left:?}");
}

#[test]
fn scrolling_and_zooming_keep_redrawing_the_page() {
    let path = write_doc("keys.md");
    let (mut terminal, mut child) = run(&path);
    std::thread::sleep(Duration::from_millis(80));
    terminal.poll();
    let before = envelopes(&terminal.seen);

    // A zoom is a full re-layout, so it must produce fresh commands.
    terminal.send(b"+");
    terminal.wait_for("a redraw after zooming", |seen| envelopes(seen) > before);
    let after_zoom = envelopes(&terminal.seen);

    terminal.send(b"G");
    terminal.wait_for("a redraw after jumping to the end", |seen| {
        envelopes(seen) > after_zoom
    });
    let on_screen = terminal.seen.clone();

    terminal.send(b"q");
    let status = wait_for_exit(&mut child);
    assert!(status.success());

    // Everything sent across the whole session — probe, first paint,
    // zoom, jump — still leaves a real host engine holding one live
    // page.
    let engine = replay(&on_screen);
    let page = engine
        .state
        .elements()
        .get("vmd.page")
        .expect("the page survived a zoom and a jump");
    assert!(!page.commands.is_empty());
}

#[test]
fn a_terminal_that_never_answers_the_probe_gives_up_rather_than_hanging() {
    let path = write_doc("noprobe.md");
    let (master, slave) = open_pty();
    let mut terminal = Terminal::new(&master);
    let open_slave = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave)
            .unwrap()
    };
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_vmd"))
        .arg(&path)
        .stdin(open_slave())
        .stdout(open_slave())
        .stderr(open_slave())
        .spawn()
        .expect("spawn vmd");

    let status = wait_for_exit(&mut child);
    terminal.poll();
    assert!(
        !status.success(),
        "vmd should fail on a terminal that does not speak VGE"
    );
    let said = String::from_utf8_lossy(&terminal.seen).to_lowercase();
    assert!(said.contains("probe"), "vmd should say why: {said:?}");
}

/// Wait for the child, killing it if it overstays — a hung viewer must
/// fail the test rather than the whole run.
fn wait_for_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    panic!("vmd did not exit within {TIMEOUT:?}");
}
