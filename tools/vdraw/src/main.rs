//! vdraw — interactive block-diagram editor for VGE-aware terminals.
//!
//! Phase 1 gave the document a camera: geometry is sent once at
//! startup, and pan/zoom is a single `UpdateTransform` on the `canvas`
//! element (§9.11), so nothing is re-encoded while the view moves.
//!
//! Phase 2 adds the bottom-centre tool palette and its options row.
//! Chrome is top-level rather than parented to the canvas, so it is
//! untouched by the camera.
//!
//! Phase 3 adds drag-to-create for the five shape tools, with geometry
//! snapped to the cell grid (`drag.rs`). The live preview is one
//! long-lived element re-pointed per frame, so dragging costs an
//! origin + commands update rather than a create/delete cycle.
//!
//! Phase 4 adds selection, moving, corner/endpoint resize and delete
//! (`hit.rs`), driven by an explicit `Interaction` state machine.
//!
//! Phase 5 adds text: the T tool places a text element by click, and
//! Enter on a selection edits its label. Editing is modal — it has to
//! swallow the tool letters and `q`, or typing would switch tools and
//! quit.
//!
//! Phase 6 adds `.excalidraw` load/save (`vdraw [path]`, Ctrl-S) and
//! snapshot-based undo/redo (`history.rs`, Ctrl-Z / Ctrl-Y). Undo takes
//! one checkpoint per *gesture*, on mouse-down, so a drag is a single
//! step rather than one per motion event.

mod camera;
mod chrome;
mod doc;
mod drag;
mod history;
mod hit;
mod input;
mod render;
mod tools;

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use vge_protocol::codec::Point;
use vge_protocol::command::{
    Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, Style, UpdateCommandsBody,
    UpdateTextBody, UpdateTextRange,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_render::probe::run_probe;
use vge_render::tty::{
    RawTty, drain_stale_stdin, install_sigwinch, poll_stdin_until, read_stdin, take_sigwinch,
    winsize,
};

use camera::Camera;
use chrome::{Action, CHROME_ID};
use drag::Drag;
use input::{Dir, Event, InputParser};
use render::{ACCENT, CANVAS_ID, CHROME_ORDER, PREVIEW_ID, PREVIEW_ORDER};
use tools::{Tool, ToolState};

/// What the pointer is currently doing. Making this explicit keeps the
/// mouse arms from having to reason about several independent
/// `Option`s that must never be `Some` at the same time.
#[derive(Debug, Clone, Copy)]
enum Interaction {
    None,
    Panning { from: (u16, u16) },
    Drawing(Drag),
    Moving { index: usize, last: Point },
    Resizing { index: usize, handle: hit::Handle },
}

const FRAME_DT: Duration = Duration::from_millis(16); // ~60 Hz
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const STATUS_ID: &str = "chrome.status";
/// Arrow-key pan step, in screen cells.
const PAN_STEP: f32 = 2.0;

fn main() -> Result<()> {
    use std::io::IsTerminal;

    // `vdraw [file.excalidraw]` — a missing file is created on save.
    let path: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vdraw must run with stdin and stdout connected to a terminal");
    }

    let _raw = RawTty::enable()?;
    let winch = install_sigwinch();
    let mut out = std::io::stdout();
    // Alt screen, hide cursor, clear, then button-event mouse tracking
    // (?1002) in SGR encoding (?1006) — the same pair vplay uses.
    out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H\x1b[?1002h\x1b[?1006h")?;
    out.flush()?;
    let _term = TermExit;

    drain_stale_stdin();
    let probe = run_probe(PROBE_TIMEOUT)?.ok_or_else(|| {
        anyhow!("VGE probe timed out — this terminal does not appear to support VGE")
    })?;

    let mut cam = Camera::new(
        probe.cell_pixel_width.max(1) as f32,
        probe.cell_pixel_height.max(1) as f32,
    );
    let (mut cols, mut rows) = term_size()?;

    // Existing geometry goes out once; the camera never re-sends it.
    // New shapes append one `CreateElement` each as they are committed.
    let mut document = match &path {
        Some(p) if p.exists() => doc::Document::load(p)?,
        // No file, or a path that doesn't exist yet: start empty.
        // `Ctrl-S` creates the file.
        _ => doc::Document::default(),
    };
    let mut next_id = document.elements.len() as u32 + 1;
    let mut history = history::History::new();
    let mut dirty_doc = false;

    let mut cursor = (0u16, 0u16);
    let mut state = ToolState::default();
    let mut bar = chrome::layout(cols, rows, &state, None);
    send(&full_render(&document, &cam, &bar, cursor, &state, rows))?;

    let mut parser = InputParser::new();
    let mut buf = [0u8; 1024];
    // Cell-granular drag tracking: SGR reports whole cells, so a drag
    // delta is only ever an integral number of cells.
    let mut interaction = Interaction::None;
    // Index into `document.elements`, not an id — the document is
    // append-only (deletes tombstone), so indices stay valid.
    let mut selected: Option<usize> = None;
    // While `Some`, every keystroke goes to that element's text rather
    // than being read as a tool shortcut.
    let mut editing: Option<usize> = None;
    // Transient message shown in the status line after a save attempt.
    let mut saved_note: Option<String> = None;
    let mut cam_dirty = false;
    let mut status_dirty = false;
    let mut chrome_dirty = false;
    let mut preview_dirty = false;
    let mut selection_dirty = false;

    loop {
        let deadline = Instant::now() + FRAME_DT;
        let mut events = Vec::new();
        while poll_stdin_until(deadline)? {
            let n = read_stdin(&mut buf)?;
            if n == 0 {
                break;
            }
            events.extend(parser.feed(&buf[..n]));
        }
        events.extend(parser.flush());

        // Commands produced while handling events (committed shapes),
        // flushed with the rest of the frame below.
        let mut frame_extra: Vec<(Command, u32)> = Vec::new();
        // The options row reflects the selection, so any change of
        // selection has to rebuild it. Compared once here rather than
        // flagged at each of the half-dozen sites that can change it.
        let selection_before = selected;

        for ev in events {
            if let Some(pos) = pointer_pos(&ev) {
                if pos != cursor {
                    cursor = pos;
                    status_dirty = true;
                }
            }
            // A save message stays up until the next real interaction —
            // clearing on mouse motion would make it vanish instantly.
            if saved_note.is_some()
                && !matches!(ev, Event::Save | Event::MouseMove { .. })
            {
                saved_note = None;
                status_dirty = true;
            }
            // Text editing is modal: it must swallow the tool letters,
            // `q`, and the zoom keys, or typing "box" would switch tools
            // and quit.
            if let Some(i) = editing {
                let mut done = false;
                match ev {
                    Event::Key(c) => document.elements[i].text.push(c),
                    Event::Delete => {
                        document.elements[i].text.pop();
                    }
                    Event::Enter | Event::Escape => done = true,
                    Event::MouseDown { .. } => done = true,
                    // Ctrl-C still quits, even mid-edit.
                    Event::Quit => return Ok(()),
                    _ => continue,
                }
                resize_text_box(&mut document.elements[i], &cam);
                push_element_update(
                    &mut frame_extra,
                    &document.elements[i],
                    i as i32 + 1,
                    &cam,
                    !done,
                );
                if done {
                    // An empty text element would be invisible and
                    // unselectable, so drop it rather than orphan it.
                    let el = &mut document.elements[i];
                    if el.shape() == Some(doc::Shape::Text) && el.text.trim().is_empty() {
                        el.is_deleted = true;
                        let id = el.id.clone();
                        frame_extra
                            .push((Command::DeleteElement { id }, REQ_ID_NO_RESPONSE));
                        selected = None;
                    }
                    editing = None;
                    selection_dirty = true;
                    status_dirty = true;
                }
                continue;
            }

            match ev {
                Event::Quit => return Ok(()),
                Event::Key('q') | Event::Key('Q') => return Ok(()),
                Event::Enter => {
                    // Enter on a selection edits its label in place.
                    if let Some(i) = selected {
                        history.checkpoint(&document);
                        dirty_doc = true;
                        editing = Some(i);
                        push_element_update(
                            &mut frame_extra,
                            &document.elements[i],
                            i as i32 + 1,
                            &cam,
                            true,
                        );
                        status_dirty = true;
                    }
                }
                Event::Escape => {
                    if matches!(interaction, Interaction::Drawing(_)) {
                        preview_dirty = true; // abandon the in-progress shape
                    }
                    interaction = Interaction::None;
                    if selected.take().is_some() {
                        selection_dirty = true;
                        status_dirty = true;
                    }
                }

                Event::Key('+') | Event::Key('=') => {
                    cam.zoom_in_at(cols as f32 / 2.0, rows as f32 / 2.0);
                    cam_dirty = true;
                    status_dirty = true;
                }
                Event::Key('-') | Event::Key('_') => {
                    cam.zoom_out_at(cols as f32 / 2.0, rows as f32 / 2.0);
                    cam_dirty = true;
                    status_dirty = true;
                }
                Event::Key('0') => {
                    cam.reset();
                    cam_dirty = true;
                    status_dirty = true;
                }

                Event::WheelUp { col, row } => {
                    cam.zoom_in_at(col as f32, row as f32);
                    cam_dirty = true;
                    status_dirty = true;
                }
                Event::WheelDown { col, row } => {
                    cam.zoom_out_at(col as f32, row as f32);
                    cam_dirty = true;
                    status_dirty = true;
                }

                Event::Arrow(d) => {
                    let (dx, dy) = match d {
                        Dir::Left => (PAN_STEP, 0.0),
                        Dir::Right => (-PAN_STEP, 0.0),
                        Dir::Up => (0.0, PAN_STEP),
                        Dir::Down => (0.0, -PAN_STEP),
                    };
                    cam.pan_by(dx, dy);
                    cam_dirty = true;
                }

                Event::Undo | Event::Redo => {
                    let changed = if matches!(ev, Event::Undo) {
                        history.undo(&mut document)
                    } else {
                        history.redo(&mut document)
                    };
                    if changed {
                        // The element list was replaced wholesale, so a
                        // stale index would point at the wrong shape.
                        selected = None;
                        editing = None;
                        dirty_doc = true;
                        frame_extra.extend(full_render(
                            &document, &cam, &bar, cursor, &state, rows,
                        ));
                    }
                }
                Event::Save => {
                    match &path {
                        Some(p) => match document.save(p) {
                            Ok(()) => {
                                dirty_doc = false;
                                saved_note = Some(format!("saved {}", p.display()));
                            }
                            Err(e) => saved_note = Some(format!("save failed: {e}")),
                        },
                        None => {
                            saved_note =
                                Some("no file — run `vdraw <path>` to save".into())
                        }
                    }
                    status_dirty = true;
                }

                Event::Delete => {
                    if let Some(i) = selected.take() {
                        history.checkpoint(&document);
                        dirty_doc = true;
                        let id = document.elements[i].id.clone();
                        // Tombstone rather than remove: indices held
                        // elsewhere stay valid, and phase 6's undo needs
                        // the element to still be there.
                        document.elements[i].is_deleted = true;
                        frame_extra
                            .push((Command::DeleteElement { id }, REQ_ID_NO_RESPONSE));
                        selection_dirty = true;
                    }
                }

                Event::MouseDown { button, col, row } => {
                    // Chrome claims the click before the canvas sees it,
                    // so a press on the palette never starts a pan.
                    if let Some(action) = bar.hit(col, row) {
                        apply(&mut state, action);
                        // With something selected, a style option
                        // restyles it as well as becoming the default
                        // for the next shape.
                        if let Some(i) = selected {
                            // Probe on a copy so the undo checkpoint is
                            // only taken when the option actually
                            // applies to this shape.
                            let mut probe = document.elements[i].clone();
                            if restyle(&mut probe, action) {
                                history.checkpoint(&document);
                                document.elements[i] = probe;
                                dirty_doc = true;
                                push_element_update(
                                    &mut frame_extra,
                                    &document.elements[i],
                                    i as i32 + 1,
                                    &cam,
                                    false,
                                );
                            }
                        }
                        chrome_dirty = true;
                        status_dirty = true;
                    } else if bar.covers(col, row) {
                        // Panel padding: swallow it.
                    } else if button == input::Button::Left && state.tool.creates_by_drag() {
                        // One checkpoint per gesture, taken on press —
                        // not per motion event, or undo would step back
                        // one pixel at a time.
                        history.checkpoint(&document);
                        interaction = Interaction::Drawing(Drag::new(drag::snap_screen(
                            col, row, &cam,
                        )));
                        preview_dirty = true;
                    } else if button == input::Button::Left && state.tool == Tool::Text {
                        // Text is placed by click and typed, not dragged.
                        let p = drag::snap_screen(col, row, &cam);
                        if let Some(el) =
                            state.new_element(format!("el-{next_id}"), p.x, p.y, 0.0, 0.0)
                        {
                            history.checkpoint(&document);
                            dirty_doc = true;
                            next_id += 1;
                            let i = document.elements.len();
                            document.elements.push(el);
                            resize_text_box(&mut document.elements[i], &cam);
                            if let Some(body) =
                                render::element_body(&document.elements[i], i as i32 + 1, &cam)
                            {
                                frame_extra.push((
                                    Command::CreateElement(body),
                                    REQ_ID_NO_RESPONSE,
                                ));
                            }
                            selected = Some(i);
                            editing = Some(i);
                            push_element_update(
                                &mut frame_extra,
                                &document.elements[i],
                                i as i32 + 1,
                                &cam,
                                true,
                            );
                            selection_dirty = true;
                            status_dirty = true;
                        }
                    } else if button == input::Button::Left && state.tool == Tool::Select {
                        let p = cam.pointer_to_doc(col, row);
                        let tol = cam.cell_w.max(cam.cell_h) / cam.zoom * 0.6;
                        // A handle on the current selection wins over
                        // whatever element happens to sit under it.
                        let on_handle = selected.and_then(|i| {
                            hit::handle_at(&document.elements[i], p, tol).map(|h| (i, h))
                        });
                        if let Some((i, handle)) = on_handle {
                            history.checkpoint(&document);
                            dirty_doc = true;
                            interaction = Interaction::Resizing { index: i, handle };
                        } else if let Some(i) = hit::hit_test(&document, p, tol) {
                            history.checkpoint(&document);
                            dirty_doc = true;
                            if selected != Some(i) {
                                selected = Some(i);
                                selection_dirty = true;
                                status_dirty = true;
                            }
                            interaction = Interaction::Moving {
                                index: i,
                                last: drag::snap_screen(col, row, &cam),
                            };
                        } else {
                            if selected.take().is_some() {
                                selection_dirty = true;
                                status_dirty = true;
                            }
                            interaction = Interaction::Panning { from: (col, row) };
                        }
                    } else {
                        // Right button always pans, as does any button
                        // when the active tool neither draws nor selects.
                        interaction = Interaction::Panning { from: (col, row) };
                    }
                }
                Event::MouseUp { col, row, .. } => {
                    if let Interaction::Drawing(mut d) = interaction {
                        d.current = drag::snap_screen(col, row, &cam);
                        let (x, y, w, h) = d.extent();
                        if d.is_significant(&cam) {
                            if let Some(el) =
                                state.new_element(format!("el-{next_id}"), x, y, w, h)
                            {
                                dirty_doc = true;
                                next_id += 1;
                                let order = document.elements.len() as i32 + 1;
                                if let Some(body) = render::element_body(&el, order, &cam) {
                                    frame_extra.push((
                                        Command::CreateElement(body),
                                        REQ_ID_NO_RESPONSE,
                                    ));
                                }
                                document.elements.push(el);
                            }
                        }
                        preview_dirty = true;
                    }
                    interaction = Interaction::None;
                }
                Event::MouseMove { col, row, held } => {
                    if held.is_none() {
                        interaction = Interaction::None;
                    } else {
                        match &mut interaction {
                            Interaction::Drawing(d) => {
                                let next = drag::snap_screen(col, row, &cam);
                                if (next.x, next.y) != (d.current.x, d.current.y) {
                                    d.current = next;
                                    preview_dirty = true;
                                }
                            }
                            Interaction::Panning { from } => {
                                cam.pan_by(
                                    col as f32 - from.0 as f32,
                                    row as f32 - from.1 as f32,
                                );
                                *from = (col, row);
                                cam_dirty = true;
                            }
                            Interaction::Moving { index, last } => {
                                let next = drag::snap_screen(col, row, &cam);
                                let (dx, dy) = (next.x - last.x, next.y - last.y);
                                if dx != 0.0 || dy != 0.0 {
                                    *last = next;
                                    let i = *index;
                                    hit::translate(&mut document.elements[i], dx, dy);
                                    // Moving is the cheap path: the
                                    // element's geometry is unchanged, so
                                    // only its origin goes on the wire.
                                    frame_extra.push((
                                        Command::UpdateOrigin {
                                            id: document.elements[i].id.clone(),
                                            origin: render::element_origin(&document.elements[i], &cam),
                                        },
                                        REQ_ID_NO_RESPONSE,
                                    ));
                                    selection_dirty = true;
                                }
                            }
                            Interaction::Resizing { index, handle } => {
                                let to = drag::snap_screen(col, row, &cam);
                                let i = *index;
                                let h = *handle;
                                hit::resize(
                                    &mut document.elements[i],
                                    h,
                                    to,
                                    cam.cell_w,
                                    cam.cell_h,
                                );
                                // Geometry changed, so commands go too.
                                let el = &document.elements[i];
                                if let Some(body) =
                                    render::element_body(el, i as i32 + 1, &cam)
                                {
                                    frame_extra.push((
                                        Command::UpdateOrigin {
                                            id: el.id.clone(),
                                            origin: body.origin,
                                        },
                                        REQ_ID_NO_RESPONSE,
                                    ));
                                    frame_extra.push((
                                        Command::UpdateCommands(UpdateCommandsBody {
                                            id: el.id.clone(),
                                            commands: body.commands,
                                        }),
                                        REQ_ID_NO_RESPONSE,
                                    ));
                                }
                                selection_dirty = true;
                            }
                            Interaction::None => {}
                        }
                    }
                }

                Event::Key(c) => {
                    if let Some(t) = Tool::from_key(c) {
                        if t != state.tool {
                            state.tool = t;
                            chrome_dirty = true;
                            status_dirty = true;
                        }
                    }
                }
            }
        }

        if selected != selection_before {
            chrome_dirty = true;
        }

        if take_sigwinch(winch) {
            if let Ok((c, r)) = term_size() {
                if (c, r) != (cols, rows) {
                    (cols, rows) = (c, r);
                    send(&[(
                        Command::UpdateOrigin {
                            id: STATUS_ID.into(),
                            origin: status_origin(rows),
                        },
                        REQ_ID_NO_RESPONSE,
                    )])?;
                    // Chrome is laid out in absolute screen cells, so a
                    // resize re-centres it rather than moving an origin.
                    chrome_dirty = true;
                }
            }
        }

        let mut frame: Vec<(Command, u32)> = frame_extra;
        if selection_dirty {
            match selected.map(|i| &document.elements[i]) {
                Some(el) => {
                    frame.push((
                        Command::UpdateCommands(UpdateCommandsBody {
                            id: render::SELECTION_ID.into(),
                            commands: render::selection_commands(el, &cam),
                        }),
                        REQ_ID_NO_RESPONSE,
                    ));
                    frame.push((
                        Command::UpdateVisibility {
                            id: render::SELECTION_ID.into(),
                            is_visible: true,
                        },
                        REQ_ID_NO_RESPONSE,
                    ));
                }
                None => frame.push((
                    Command::UpdateVisibility {
                        id: render::SELECTION_ID.into(),
                        is_visible: false,
                    },
                    REQ_ID_NO_RESPONSE,
                )),
            }
            selection_dirty = false;
        }
        if preview_dirty {
            // One long-lived element re-pointed per frame — an origin
            // plus a commands update, rather than a create/delete cycle.
            let in_progress = match interaction {
                Interaction::Drawing(d) => Some(d),
                _ => None,
            };
            match in_progress.and_then(|d| preview_body(&state, &d, &cam)) {
                Some(body) => {
                    frame.push((
                        Command::UpdateOrigin {
                            id: PREVIEW_ID.into(),
                            origin: body.origin,
                        },
                        REQ_ID_NO_RESPONSE,
                    ));
                    frame.push((
                        Command::UpdateCommands(UpdateCommandsBody {
                            id: PREVIEW_ID.into(),
                            commands: body.commands,
                        }),
                        REQ_ID_NO_RESPONSE,
                    ));
                    frame.push((
                        Command::UpdateVisibility {
                            id: PREVIEW_ID.into(),
                            is_visible: true,
                        },
                        REQ_ID_NO_RESPONSE,
                    ));
                }
                None => frame.push((
                    Command::UpdateVisibility {
                        id: PREVIEW_ID.into(),
                        is_visible: false,
                    },
                    REQ_ID_NO_RESPONSE,
                )),
            }
            preview_dirty = false;
        }
        if chrome_dirty {
            // Switching tools can add or remove the options row, so the
            // whole bar is re-laid out, not just restyled.
            bar = chrome::layout(
                cols,
                rows,
                &state,
                selected.map(|i| &document.elements[i]),
            );
            frame.push((
                Command::UpdateCommands(UpdateCommandsBody {
                    id: CHROME_ID.into(),
                    commands: chrome::draw(&bar, cam.cell_w, cam.cell_h),
                }),
                REQ_ID_NO_RESPONSE,
            ));
            chrome_dirty = false;
        }
        if cam_dirty {
            frame.push((
                Command::UpdateTransform {
                    id: CANVAS_ID.into(),
                    transform: cam.transform(),
                },
                REQ_ID_NO_RESPONSE,
            ));
            cam_dirty = false;
        }
        if status_dirty {
            frame.push((
                Command::UpdateText(UpdateTextBody {
                    id: STATUS_ID.into(),
                    command_index: 0,
                    range: UpdateTextRange::Whole,
                    replacement: status_text(
                        &cam,
                        cursor,
                        &state,
                        selected.map(|i| &document.elements[i]),
                        editing.is_some(),
                        dirty_doc,
                        saved_note.as_deref(),
                    ),
                }),
                REQ_ID_NO_RESPONSE,
            ));
            status_dirty = false;
        }
        if !frame.is_empty() {
            send(&frame)?;
        }
    }
}

/// Screen cell a mouse event happened at, if it is a mouse event.
fn pointer_pos(ev: &Event) -> Option<(u16, u16)> {
    Some(match *ev {
        Event::MouseDown { col, row, .. }
        | Event::MouseUp { col, row, .. }
        | Event::MouseMove { col, row, .. }
        | Event::WheelUp { col, row }
        | Event::WheelDown { col, row } => (col, row),
        _ => return None,
    })
}

/// Rebuild every element from scratch. Used at startup and after
/// undo/redo, where the document changed wholesale — not on the drag
/// path, which updates individual elements in place.
fn full_render(
    document: &doc::Document,
    cam: &Camera,
    bar: &chrome::Chrome,
    cursor: (u16, u16),
    state: &ToolState,
    rows: u16,
) -> Vec<(Command, u32)> {
    let mut out = vec![
        (Command::ClearAll, REQ_ID_NO_RESPONSE),
        (render::canvas_element(cam), REQ_ID_NO_RESPONSE),
    ];
    out.extend(render::document_elements(document, cam));
    out.push((render::preview_element(), REQ_ID_NO_RESPONSE));
    out.push((render::selection_element(), REQ_ID_NO_RESPONSE));
    out.push((chrome_element(bar, cam), REQ_ID_NO_RESPONSE));
    out.push((
        status_element(cam, cursor, state, rows),
        REQ_ID_NO_RESPONSE,
    ));
    out
}

/// Keep a text element's box in step with its string, so hit testing
/// and the selection outline match what is drawn. The terminal font is
/// the primary font, so one ASCII character is one cell wide.
fn resize_text_box(e: &mut doc::Element, cam: &Camera) {
    // Record the font size a *web* renderer should use, for every
    // element that carries text — containers included, since their
    // caption is split out into a real text element on save. Derived
    // from the cell height so the saved document matches what vdraw
    // drew; vdraw's own rendering ignores it (VGE text is cell-sized).
    if !e.text.is_empty() {
        e.font_size = Some(cam.cell_h / doc::LINE_HEIGHT);
    }
    if e.shape() != Some(doc::Shape::Text) {
        return;
    }
    e.width = e.text.chars().count() as f32 * cam.cell_w;
    e.height = cam.cell_h;
}

/// Re-send an element's origin and geometry, optionally with the text
/// caret appended. Used by text editing and resize, where the geometry
/// itself changed — a plain move only needs the origin.
fn push_element_update(
    frame: &mut Vec<(Command, u32)>,
    e: &doc::Element,
    order: i32,
    cam: &Camera,
    caret: bool,
) {
    let Some(body) = render::element_body(e, order, cam) else {
        return;
    };
    let mut commands = body.commands;
    if caret {
        commands.push(render::caret_command(e, ACCENT));
    }
    frame.push((
        Command::UpdateOrigin {
            id: e.id.clone(),
            origin: body.origin,
        },
        REQ_ID_NO_RESPONSE,
    ));
    frame.push((
        Command::UpdateCommands(UpdateCommandsBody {
            id: e.id.clone(),
            commands,
        }),
        REQ_ID_NO_RESPONSE,
    ));
}

/// Geometry for the in-progress shape, or `None` while the drag is
/// still too small to be a real element.
fn preview_body(
    state: &ToolState,
    d: &Drag,
    cam: &Camera,
) -> Option<vge_protocol::command::CreateElementBody> {
    if !d.is_significant(cam) {
        return None;
    }
    let (x, y, w, h) = d.extent();
    let el = state.new_element("preview", x, y, w, h)?;
    render::element_body(&el, PREVIEW_ORDER, cam)
}

/// Apply a style option to an existing element, mirroring what
/// `ToolState::new_element` would have baked in at creation time.
///
/// Returns whether anything changed: switching tools is not a restyle,
/// and an option the shape can't express (a fill on an arrow, a line
/// type on text) is a no-op rather than a silent lie in the document.
fn restyle(e: &mut doc::Element, action: Action) -> bool {
    match action {
        Action::Tool(_) => false,
        Action::Thickness(w) => {
            e.stroke_width = w;
            true
        }
        Action::Color(c) => {
            e.stroke_color = c.into();
            true
        }
        Action::Fill(c) => match e.shape() {
            Some(s) if s.is_closed() => {
                e.background_color = c.into();
                true
            }
            _ => false,
        },
        Action::Line(lt) => match e.shape() {
            Some(doc::Shape::Text) | None => false,
            Some(_) => {
                e.stroke_style = lt.as_str().into();
                true
            }
        },
    }
}

fn apply(state: &mut ToolState, action: Action) {
    match action {
        Action::Tool(t) => state.tool = t,
        Action::Thickness(w) => state.thickness = w,
        Action::Color(c) => state.color = c,
        Action::Fill(c) => state.fill = c,
        Action::Line(lt) => state.line_type = lt,
    }
}

/// The palette element. Top-level (no `parent`) so the camera transform
/// never reaches it; its geometry is in absolute screen cells.
fn chrome_element(bar: &chrome::Chrome, cam: &Camera) -> Command {
    Command::CreateElement(CreateElementBody {
        id: CHROME_ID.into(),
        commands: chrome::draw(bar, cam.cell_w, cam.cell_h),
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: CHROME_ORDER,
        parent: None,
        size: None,
        transform: None,
    })
}

fn status_text(
    cam: &Camera,
    cursor: (u16, u16),
    state: &ToolState,
    selected: Option<&doc::Element>,
    editing: bool,
    dirty: bool,
    note: Option<&str>,
) -> String {
    // A save message displaces the hint line until the next keystroke.
    if let Some(n) = note {
        return format!("vdraw  {n}");
    }
    if editing {
        return "vdraw  [text]  typing — Enter or Esc commits".into();
    }
    let p = cam.pointer_to_doc(cursor.0, cursor.1);
    let sel = match selected {
        Some(e) => format!("  sel:{}", e.kind),
        None => String::new(),
    };
    format!(
        "vdraw{}  [{}]  {}%  ({:.0}, {:.0}){}   s/b/e/d/l/a/t · ^Z undo · ^Y redo · ^S save · q quit",
        if dirty { "*" } else { "" },
        state.tool.label(),
        cam.zoom_percent(),
        p.x,
        p.y,
        sel
    )
}

fn status_origin(rows: u16) -> Point {
    Point {
        x: 1.0,
        y: rows.saturating_sub(1) as f32,
    }
}

/// Chrome is a top-level element — no `parent`, so the camera transform
/// never reaches it and the readout stays put while the canvas moves.
fn status_element(cam: &Camera, cursor: (u16, u16), state: &ToolState, rows: u16) -> Command {
    Command::CreateElement(CreateElementBody {
        id: STATUS_ID.into(),
        commands: vec![DrawCmd::DrawText {
            origin: Point { x: 0.0, y: 0.0 },
            align: Align::Left,
            fill: Style::Flat(Color {
                r: 0.55,
                g: 0.58,
                b: 0.62,
                a: 1.0,
            }),
            font_style: FontStyle::default(),
            text: status_text(cam, cursor, state, None, false, false, None),
        }],
        origin: status_origin(rows),
        is_visible: true,
        draw_order: CHROME_ORDER,
        parent: None,
        size: None,
        transform: None,
    })
}

fn term_size() -> Result<(u16, u16)> {
    let ws = winsize().ok_or_else(|| anyhow!("could not query terminal size"))?;
    Ok((ws.ws_col.max(1), ws.ws_row.max(1)))
}

fn send(cmds: &[(Command, u32)]) -> Result<()> {
    let mut out = std::io::stdout();
    out.write_all(&build_envelope(cmds))?;
    out.flush()?;
    Ok(())
}

struct TermExit;

impl Drop for TermExit {
    fn drop(&mut self) {
        let mut o = std::io::stdout();
        let env = build_envelope(&[(Command::ClearAll, REQ_ID_NO_RESPONSE)]);
        let _ = o.write_all(&env);
        let _ = o.write_all(b"\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l");
        let _ = o.flush();
    }
}
