//! End-to-end: run the real `vplace` binary against a real pty, then
//! feed what it wrote through the real host engine and check where the
//! image landed.
//!
//! The point is the seam a unit test cannot cover: `vplace` resolves
//! nothing itself — it names a *string*, and the terminal, which owns
//! the grid, turns that into an absolute scrollback line. So the test
//! plays the application (print text, a marker, a reserved gap) and
//! then plays the terminal (`drive_terminal_stage`), in that order,
//! because the anchor is only correct if the marker is already on the
//! live screen when the command is processed.

use std::ffi::CStr;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use veter_host::vge::{VgeEngine, drive_terminal_stage};

const ROWS: u16 = 24;
const COLS: u16 = 80;
const CELL_W: u16 = 9;
const CELL_H: u16 = 20;

/// A pty pair whose winsize carries pixel dimensions, so `vplace` can
/// derive cell metrics from `TIOCGWINSZ` exactly as it does under a
/// real veter (§11.1). Returns the master fd and the slave's path.
fn open_pty() -> (OwnedFd, String) {
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(m), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(m), 0, "unlockpt failed");
        let name = CStr::from_ptr(libc::ptsname(m)).to_str().unwrap().to_owned();
        let ws = libc::winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: COLS * CELL_W,
            ws_ypixel: ROWS * CELL_H,
        };
        assert_eq!(libc::ioctl(m, libc::TIOCSWINSZ, &ws), 0, "TIOCSWINSZ failed");
        (OwnedFd::from_raw_fd(m), name)
    }
}

/// Drain the master without blocking. `vplace` has already exited, so
/// everything it wrote is sitting in the pty buffer; once it is drained
/// the read returns EAGAIN (no writer) or EIO (slave closed).
fn drain(master: &OwnedFd) -> Vec<u8> {
    unsafe {
        let fl = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(libc::dup(master.as_raw_fd())) };
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    out
}

fn tiny_png(path: &std::path::Path, w: u32, h: u32) {
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgba([(x * 8) as u8, (y * 8) as u8, 128, 255]);
    }
    img.save(path).unwrap();
}

/// Run `vplace` and return the bytes it wrote to the pane.
fn run_vplace(args: &[&str]) -> Vec<u8> {
    let (master, slave) = open_pty();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_vplace"))
        .args(args)
        .arg("--tty")
        .arg(&slave)
        .env("VETER", "test")
        // enc=3 advertises Raw|WebP, matching a real host's defaults.
        .env("VETER_LIMITS", "mib=268435456,mi=1024,enc=3,nest=16,mwb=1048576")
        .env_remove("VMUX_PANE_TTY")
        .output()
        .expect("spawn vplace");
    assert!(
        status.status.success(),
        "vplace failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    drain(&master)
}

/// Run `vplace` and return what it printed on stdout (`--measure`).
fn run_vplace_stdout(args: &[&str]) -> String {
    let (_master, slave) = open_pty();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_vplace"))
        .args(args)
        .arg("--tty")
        .arg(&slave)
        .env("VETER", "test")
        .env("VETER_LIMITS", "mib=268435456,mi=1024,enc=3,nest=16,mwb=1048576")
        .env_remove("VMUX_PANE_TTY")
        .output()
        .expect("spawn vplace");
    assert!(out.status.success(), "vplace --measure failed");
    String::from_utf8(out.stdout).expect("measure output is utf8")
}

/// Play the application, then the terminal. Returns the engine and the
/// parser so the caller can assert against both.
fn apply(app_output: &[u8], envelope: &[u8]) -> (VgeEngine, vt100::Parser) {
    let mut engine = VgeEngine::new((CELL_W, CELL_H), 1.0);
    let mut parser = vt100::Parser::new(ROWS, COLS, 1000);
    // Pick up the parser's line origin, as a real host does on its
    // first pass.
    engine.after_vt100_process(&mut parser);

    // The application's own output first — this is what puts the marker
    // on the grid and reserves the rows.
    drive_terminal_stage(&mut engine, &mut parser, app_output);
    // Then our out-of-band write.
    drive_terminal_stage(&mut engine, &mut parser, envelope);
    (engine, parser)
}

/// Row of the last live-screen line containing `needle`.
fn marker_row(parser: &vt100::Parser, needle: &str) -> u16 {
    let mut found = None;
    for (row, text) in parser.screen().rows(0, COLS).enumerate() {
        if text.contains(needle) {
            found = Some(row as u16);
        }
    }
    found.unwrap_or_else(|| panic!("marker {needle:?} is not on the live screen"))
}

#[test]
fn image_anchors_to_the_row_below_the_marker() {
    // The default placement, and the reason the marker row survives:
    // the image starts *under* it, so the line the application printed
    // stays readable as a caption.
    let dir = std::env::temp_dir().join("vplace-e2e-anchor");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("t.png");
    tiny_png(&png, 64, 32);

    let envelope = run_vplace(&[png.to_str().unwrap(), "--marker", "[IMAGE: t1]"]);
    assert!(!envelope.is_empty(), "vplace wrote nothing");

    // The application prints context, the marker, then a reserved gap.
    let app = b"hello\r\nworld\r\n[IMAGE: t1] t.png\r\n\r\n\r\n\r\n\r\n".to_vec();
    let (engine, parser) = apply(&app, &envelope);

    let els = engine.state.elements();
    let el = els
        .get("vplace.IMAGE-t1")
        .expect("element was not created — check the id namespace");

    let expected = engine.top_of_live_screen() + i64::from(marker_row(&parser, "[IMAGE: t1]")) + 1;
    assert_eq!(
        el.anchor_line, expected,
        "image must anchor one row below the marker line"
    );
    assert!(el.is_visible);
}

#[test]
fn offset_y_zero_places_over_the_marker_row() {
    // The opt-out, for a host whose renderer leaves no blank spacing
    // row under a paragraph. Nothing hides the marker text in this
    // mode — the image simply starts on top of it.
    let dir = std::env::temp_dir().join("vplace-e2e-offset0");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("t.png");
    tiny_png(&png, 64, 32);

    let envelope = run_vplace(&[
        png.to_str().unwrap(),
        "--marker",
        "[IMAGE: z0]",
        "--offset-y",
        "0",
    ]);
    let app = b"[IMAGE: z0] t.png\r\n\r\n\r\n\r\n".to_vec();
    let (engine, parser) = apply(&app, &envelope);

    let el = &engine.state.elements()["vplace.IMAGE-z0"];
    assert_eq!(
        el.anchor_line,
        engine.top_of_live_screen() + i64::from(marker_row(&parser, "[IMAGE: z0]")),
        "--offset-y 0 must land on the marker's own row"
    );
}

#[test]
fn anchor_is_the_marker_row_not_the_cursor() {
    // The distinguishing property. Marker anchoring exists precisely
    // because an out-of-band client cannot know where the cursor is —
    // so drive the cursor far away from the marker and confirm the
    // placement ignores it.
    let dir = std::env::temp_dir().join("vplace-e2e-cursor");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("t.png");
    tiny_png(&png, 32, 32);

    let envelope = run_vplace(&[png.to_str().unwrap(), "--marker", "[IMAGE: m]"]);

    // Marker near the top; then many more lines, leaving the cursor far
    // below it — exactly the situation a TUI's composer creates.
    let mut app = b"[IMAGE: m]\r\n".to_vec();
    app.extend_from_slice(&b"filler\r\n".repeat(10));
    let (engine, parser) = apply(&app, &envelope);

    let row = marker_row(&parser, "[IMAGE: m]");
    let cursor_row = parser.screen().cursor_position().0;
    assert_ne!(row, cursor_row, "test is meaningless if they coincide");

    let el = &engine.state.elements()["vplace.IMAGE-m"];
    assert_eq!(
        el.anchor_line,
        engine.top_of_live_screen() + i64::from(row) + 1,
        "anchored to the cursor instead of the marker"
    );
}

#[test]
fn placement_survives_scrolling_into_scrollback() {
    // `anchor_line` is absolute, so once resolved the image travels
    // with its text. This is what makes the approach usable at all:
    // Claude Code's completed messages scroll up out of the live
    // region, and the image has to go with them.
    let dir = std::env::temp_dir().join("vplace-e2e-scroll");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("t.png");
    tiny_png(&png, 32, 16);

    let envelope = run_vplace(&[png.to_str().unwrap(), "--marker", "[IMAGE: s]"]);

    let app = b"[IMAGE: s]\r\n\r\n\r\n\r\n".to_vec();
    let mut engine = VgeEngine::new((CELL_W, CELL_H), 1.0);
    let mut parser = vt100::Parser::new(ROWS, COLS, 1000);
    engine.after_vt100_process(&mut parser);
    drive_terminal_stage(&mut engine, &mut parser, &app);
    drive_terminal_stage(&mut engine, &mut parser, &envelope);

    let before = engine.state.elements()["vplace.IMAGE-s"].anchor_line;

    // Push the marker well up into scrollback.
    drive_terminal_stage(&mut engine, &mut parser, &b"scroll\r\n".repeat(40));

    let after = engine.state.elements()["vplace.IMAGE-s"].anchor_line;
    assert_eq!(before, after, "anchor must not move when content scrolls");
    assert!(
        engine.top_of_live_screen() > after,
        "marker should now be above the live screen"
    );
}

#[test]
fn the_marker_row_is_never_painted_over() {
    // The caption contract: the element draws the image and nothing
    // else, so whatever the application wrote on the marker row — id,
    // path, a human title — survives above the image. This used to be
    // a full-width rectangle painted over that row precisely because
    // the image landed on it.
    use vge_protocol::command::DrawCmd;

    let dir = std::env::temp_dir().join("vplace-e2e-caption");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("t.png");
    tiny_png(&png, 48, 24);

    let envelope = run_vplace(&[
        png.to_str().unwrap(),
        "--marker",
        "[IMAGE: cv]",
        "--offset-x",
        "2",
    ]);
    let caption = "[IMAGE: cv] /some/rather/long/path/to/an/image.png";
    let app = format!("{caption}\r\n\r\n\r\n").into_bytes();
    let (engine, parser) = apply(&app, &envelope);
    let el = &engine.state.elements()["vplace.IMAGE-cv"];

    assert_eq!(el.origin_x, 2.0, "--offset-x must indent the element");
    assert_eq!(el.commands.len(), 1, "the element must draw the image alone");
    assert!(
        matches!(el.commands[0], DrawCmd::DrawImage { .. }),
        "expected a lone DrawImage, got {:?}",
        el.commands[0]
    );

    // The image starts strictly below the caption's row, so no part of
    // the element overlaps the text.
    let row = marker_row(&parser, "[IMAGE: cv]");
    assert!(
        el.anchor_line > engine.top_of_live_screen() + i64::from(row),
        "element must begin below the caption row"
    );
}

#[test]
fn a_tall_image_is_scaled_to_fit_instead_of_overrunning() {
    // The bug this guards: width was clamped to the pane but height was
    // not, so a tall image ran past the rows the application reserved,
    // painted over the text below, and could leave the screen entirely.
    use vge_protocol::command::DrawCmd;

    let dir = std::env::temp_dir().join("vplace-e2e-tall");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("tall.png");
    // Far taller than the 24-row pane: 400x4000 px on 9x20 cells is
    // ~200 rows unclamped.
    tiny_png(&png, 400, 4000);

    let envelope = run_vplace(&[png.to_str().unwrap(), "--marker", "[IMAGE: tall]"]);
    let app = b"[IMAGE: tall]\r\n\r\n\r\n".to_vec();
    let (engine, _parser) = apply(&app, &envelope);

    let el = &engine.state.elements()["vplace.IMAGE-tall"];
    let h = match el.commands.iter().find_map(|c| match c {
        DrawCmd::DrawImage { target_rect, .. } => Some(target_rect.h),
        _ => None,
    }) {
        Some(h) => h,
        None => panic!("no DrawImage emitted"),
    };

    let budget = (ROWS as f32 * 2.0 / 3.0).floor();
    assert!(
        h <= budget,
        "image is {h} rows on a {ROWS}-row pane; must fit the {budget}-row budget"
    );
    assert!(h > 1.0, "scaled to nothing — the fit collapsed instead of shrinking");
}

#[test]
fn max_rows_is_honoured_and_preserves_aspect() {
    use vge_protocol::command::DrawCmd;

    let dir = std::env::temp_dir().join("vplace-e2e-maxrows");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("sq.png");
    tiny_png(&png, 400, 400); // square: rows and cols should track

    let envelope = run_vplace(&[
        png.to_str().unwrap(),
        "--marker",
        "[IMAGE: mr]",
        "--max-rows",
        "4",
    ]);
    let app = b"[IMAGE: mr]\r\n\r\n\r\n".to_vec();
    let (engine, _parser) = apply(&app, &envelope);
    let el = &engine.state.elements()["vplace.IMAGE-mr"];

    let rect = el
        .commands
        .iter()
        .find_map(|c| match c {
            DrawCmd::DrawImage { target_rect, .. } => Some(*target_rect),
            _ => None,
        })
        .expect("no DrawImage");
    assert!(rect.h <= 4.0, "--max-rows 4 but got {} rows", rect.h);

    // A square image on 9x20 cells is about 2.2x wider in cells than
    // tall; assert the ratio survived the shrink rather than the image
    // being squashed.
    let ratio = rect.w / rect.h;
    let expected = CELL_H as f32 / CELL_W as f32;
    assert!(
        (ratio - expected).abs() / expected < 0.25,
        "aspect drifted: {ratio:.2} cells w/h vs expected ~{expected:.2}"
    );
}

#[test]
fn clear_sweeps_only_this_tools_namespace() {
    let dir = std::env::temp_dir().join("vplace-e2e-clear");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("t.png");
    tiny_png(&png, 16, 16);

    let place = run_vplace(&[png.to_str().unwrap(), "--marker", "[IMAGE: c]"]);
    let clear = run_vplace(&["--clear"]);

    let app = b"[IMAGE: c]\r\n\r\n\r\n".to_vec();
    let mut engine = VgeEngine::new((CELL_W, CELL_H), 1.0);
    let mut parser = vt100::Parser::new(ROWS, COLS, 1000);
    engine.after_vt100_process(&mut parser);
    drive_terminal_stage(&mut engine, &mut parser, &app);
    drive_terminal_stage(&mut engine, &mut parser, &place);

    // A foreign element must survive the sweep.
    let foreign = vge_protocol::encode::build_envelope(&[(
        vge_protocol::command::Command::CreateElement(
            vge_protocol::command::CreateElementBody {
                id: "otherapp.keepme".into(),
                commands: vec![],
                origin: vge_protocol::codec::Point { x: 0.0, y: 0.0 },
                is_visible: true,
                draw_order: 0,
                parent: None,
                size: None,
                transform: None,
                anchor: vge_protocol::command::OriginAnchor::Viewport,
            },
        ),
        vge_protocol::frame::REQ_ID_NO_RESPONSE,
    )]);
    drive_terminal_stage(&mut engine, &mut parser, &foreign);
    assert!(engine.state.elements().contains_key("vplace.IMAGE-c"));

    drive_terminal_stage(&mut engine, &mut parser, &clear);
    assert!(
        !engine.state.elements().contains_key("vplace.IMAGE-c"),
        "--clear must remove this tool's elements"
    );
    assert!(
        engine.state.elements().contains_key("otherapp.keepme"),
        "--clear must not touch another client's ids"
    );
}

#[test]
fn measure_reports_the_gap_the_image_actually_fills() {
    // The invariant tying `--measure` to the placement: the image
    // starts on the *first* reserved row, so the blank-line count an
    // application is told to reserve is the image's height exactly.
    // Off by one here overruns the gap and paints over the paragraph
    // below — and nothing on either side can notice, which is why this
    // is pinned rather than left to the two call sites agreeing.
    use vge_protocol::command::DrawCmd;

    let dir = std::env::temp_dir().join("vplace-e2e-measure");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("m.png");
    tiny_png(&png, 400, 200);

    let out = run_vplace_stdout(&[png.to_str().unwrap(), "--measure", "--max-rows", "9"]);
    let field = |k: &str| -> u32 {
        out.split_whitespace()
            .find_map(|kv| kv.strip_prefix(k)?.parse().ok())
            .unwrap_or_else(|| panic!("no {k} in {out:?}"))
    };
    let rows = field("rows=");
    let gap = field("fence_blank_lines=");
    assert_eq!(
        gap, rows,
        "an application reserving `fence_blank_lines` rows must get exactly the image's height"
    );

    // And the placed image really is that tall: within the gap, and not
    // a whole row short of it either.
    let envelope = run_vplace(&[
        png.to_str().unwrap(),
        "--marker",
        "[IMAGE: ms]",
        "--max-rows",
        "9",
    ]);
    let app = b"[IMAGE: ms] m.png\r\n\r\n\r\n".to_vec();
    let (engine, _parser) = apply(&app, &envelope);
    let h = engine.state.elements()["vplace.IMAGE-ms"]
        .commands
        .iter()
        .find_map(|c| match c {
            DrawCmd::DrawImage { target_rect, .. } => Some(target_rect.h),
            _ => None,
        })
        .expect("no DrawImage");
    assert!(
        h <= gap as f32,
        "image is {h} rows but the message reserves {gap}"
    );
    assert!(
        h > gap as f32 - 1.0,
        "image is {h} rows in a {gap}-row gap — a whole row is wasted"
    );
}
