mod prt;
mod pty;
mod renderer;
mod vge;

use std::io::Read;
use std::num::NonZeroU32;
use std::sync::{mpsc, Arc};

use femtovg::{renderer::OpenGl, Canvas, Color};
use glutin::config::ConfigTemplateBuilder;
use glutin::context::{ContextAttributesBuilder, PossiblyCurrentContext};
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin::surface::{SurfaceAttributesBuilder, WindowSurface};
use glutin_winit::DisplayBuilder;
use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

struct App {
    // Terminal state (dropped first — no GL dependency)
    parser: Option<vt100::Parser>,
    pty: Option<pty::Pty>,
    term_renderer: Option<renderer::TerminalRenderer>,
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    vge: Option<vge::VgeEngine>,
    prt: Option<prt::PrtEngine>,

    // GL state (dropped in reverse-creation order so the EGL surface
    // is destroyed while the Wayland window still exists)
    canvas: Option<Canvas<OpenGl>>,
    gl_surface: Option<glutin::surface::Surface<WindowSurface>>,
    gl_context: Option<PossiblyCurrentContext>,
    window: Option<Arc<Window>>,

    // Input
    proxy: EventLoopProxy<()>,
    modifiers: ModifiersState,
    /// Last seen pointer position in physical pixels. Set by
    /// `WindowEvent::CursorMoved`, read by the `MouseWheel` handler so
    /// it can convert to `(col, row)` cells when forwarding wheel
    /// events to the PTY.
    cursor_pos: Option<winit::dpi::PhysicalPosition<f64>>,
}

impl App {
    fn new(proxy: EventLoopProxy<()>) -> Self {
        Self {
            window: None,
            gl_surface: None,
            gl_context: None,
            canvas: None,
            parser: None,
            pty: None,
            term_renderer: None,
            rx: None,
            vge: None,
            prt: None,
            proxy,
            modifiers: ModifiersState::empty(),
            cursor_pos: None,
        }
    }

    fn handle_key_input(&mut self, event: &winit::event::KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }

        // Shift+PageUp/Down for scrollback
        if self.modifiers.shift_key() {
            match &event.logical_key {
                Key::Named(NamedKey::PageUp) => {
                    if let Some(parser) = &mut self.parser {
                        let rows = parser.screen().size().0 as usize;
                        let screen = parser.screen_mut();
                        screen.set_scrollback(screen.scrollback() + rows);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                Key::Named(NamedKey::PageDown) => {
                    if let Some(parser) = &mut self.parser {
                        let rows = parser.screen().size().0 as usize;
                        let screen = parser.screen_mut();
                        screen.set_scrollback(screen.scrollback().saturating_sub(rows));
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                _ => {}
            }
        }

        // Any non-scroll key resets scrollback to bottom
        if let Some(parser) = &mut self.parser {
            parser.screen_mut().set_scrollback(0);
        }

        let pty = match &self.pty {
            Some(p) => p,
            None => return,
        };

        // Ctrl+key
        if self.modifiers.control_key() {
            match &event.logical_key {
                Key::Character(c) => {
                    if let Some(ch) = c.chars().next()
                        && ch.is_ascii_alphabetic()
                    {
                        let ctrl = (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                        let _ = pty.write_all(&[ctrl]);
                        return;
                    }
                }
                Key::Named(NamedKey::Space) => {
                    let _ = pty.write_all(&[0x00]);
                    return;
                }
                _ => {}
            }
        }

        // Alt+key: send ESC prefix
        if self.modifiers.alt_key()
            && !self.modifiers.control_key()
            && let Some(text) = &event.text
        {
            let mut bytes = vec![0x1b];
            bytes.extend_from_slice(text.as_bytes());
            let _ = pty.write_all(&bytes);
            return;
        }

        // Named keys
        let bytes: Option<&[u8]> = match &event.logical_key {
            Key::Named(named) => match named {
                NamedKey::Enter => Some(b"\r"),
                NamedKey::Backspace => Some(b"\x7f"),
                NamedKey::Tab => Some(b"\t"),
                NamedKey::Escape => Some(b"\x1b"),
                NamedKey::ArrowUp => Some(b"\x1b[A"),
                NamedKey::ArrowDown => Some(b"\x1b[B"),
                NamedKey::ArrowRight => Some(b"\x1b[C"),
                NamedKey::ArrowLeft => Some(b"\x1b[D"),
                NamedKey::Home => Some(b"\x1b[H"),
                NamedKey::End => Some(b"\x1b[F"),
                NamedKey::Delete => Some(b"\x1b[3~"),
                NamedKey::PageUp => Some(b"\x1b[5~"),
                NamedKey::PageDown => Some(b"\x1b[6~"),
                NamedKey::Insert => Some(b"\x1b[2~"),
                NamedKey::F1 => Some(b"\x1bOP"),
                NamedKey::F2 => Some(b"\x1bOQ"),
                NamedKey::F3 => Some(b"\x1bOR"),
                NamedKey::F4 => Some(b"\x1bOS"),
                NamedKey::F5 => Some(b"\x1b[15~"),
                NamedKey::F6 => Some(b"\x1b[17~"),
                NamedKey::F7 => Some(b"\x1b[18~"),
                NamedKey::F8 => Some(b"\x1b[19~"),
                NamedKey::F9 => Some(b"\x1b[20~"),
                NamedKey::F10 => Some(b"\x1b[21~"),
                NamedKey::F11 => Some(b"\x1b[23~"),
                NamedKey::F12 => Some(b"\x1b[24~"),
                _ => None,
            },
            _ => None,
        };

        if let Some(b) = bytes {
            let _ = pty.write_all(b);
            return;
        }

        // Text input
        if let Some(text) = &event.text {
            let _ = pty.write_all(text.as_bytes());
        }
    }

    /// Process PTY output. Returns false if the child process has exited.
    fn process_pty_output(&mut self) -> bool {
        let rx = match &self.rx {
            Some(r) => r,
            None => return false,
        };
        let parser = match &mut self.parser {
            Some(p) => p,
            None => return false,
        };
        let engine = match &mut self.vge {
            Some(e) => e,
            None => return false,
        };
        let prt = match &mut self.prt {
            Some(p) => p,
            None => return false,
        };
        let pty = match &self.pty {
            Some(p) => p,
            None => return false,
        };

        loop {
            match rx.try_recv() {
                Ok(data) => {
                    // Pipeline: PRT extracts ESC_PRT envelopes and observes
                    // RIS/DECSTR/2J/3J events; VGE then extracts ESC_VGE
                    // envelopes from PRT's passthrough; the rest goes to
                    // the host vt100. Each engine's apc passes the other
                    // extension's marker through verbatim, so order is
                    // independent of correctness.
                    let prt_chunk = prt.process_pty_chunk_full(&data);
                    let vge_passthrough = engine.process_pty_chunk(&prt_chunk.passthrough);
                    if !vge_passthrough.is_empty() {
                        parser.process(&vge_passthrough);
                    }
                    // PRT host-screen reactions: scope_reset / cull on
                    // observed RIS/DECSTR/2J/3J, then alt-screen swap +
                    // line tracker + scrollback eviction.
                    prt.handle_terminal_events(&prt_chunk.terminal_events);
                    prt.after_vt100_process(parser);
                    prt.flush_pending_events();
                    engine.after_vt100_process(parser);

                    let prt_resp = prt.take_responses();
                    if !prt_resp.is_empty() {
                        let _ = pty.write_all(&prt_resp);
                    }
                    let resp = engine.take_responses();
                    if !resp.is_empty() {
                        let _ = pty.write_all(&resp);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let window_attrs = WindowAttributes::default()
            .with_title("vterm")
            .with_inner_size(winit::dpi::LogicalSize::new(800u32, 600u32));

        let template = ConfigTemplateBuilder::new().with_alpha_size(8);
        let display_builder = DisplayBuilder::new().with_window_attributes(Some(window_attrs));

        let (window, gl_config) = display_builder
            .build(event_loop, template, |mut configs| configs.next().unwrap())
            .unwrap();

        let window = Arc::new(window.unwrap());
        let gl_display = gl_config.display();
        let raw_handle = window.window_handle().unwrap().as_raw();

        let context_attrs = ContextAttributesBuilder::new().build(Some(raw_handle));
        let gl_context = unsafe { gl_display.create_context(&gl_config, &context_attrs).unwrap() };

        let size = window.inner_size();
        let surface_attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_handle,
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        );
        let gl_surface =
            unsafe { gl_display.create_window_surface(&gl_config, &surface_attrs).unwrap() };
        let gl_context = gl_context.make_current(&gl_surface).unwrap();

        let gl_renderer = unsafe {
            OpenGl::new_from_function_cstr(|s| gl_display.get_proc_address(s) as *const _)
        }
        .unwrap();

        let mut canvas = Canvas::new(gl_renderer).unwrap();
        canvas.set_size(size.width, size.height, 1.0);

        // Initialize terminal renderer and measure cell dimensions
        let font_size = 16.0 * window.scale_factor() as f32;
        let term_renderer = renderer::TerminalRenderer::new(&mut canvas, font_size);
        let (term_cols, term_rows) = term_renderer.terminal_size(size.width, size.height);

        // VGE engine: needs cell pixel dimensions and HiDPI scale factor.
        let cell_px = (
            term_renderer.cell_width.round() as u16,
            term_renderer.cell_height.round() as u16,
        );
        let scale = window.scale_factor() as f32;
        let vge_engine = vge::VgeEngine::new(cell_px, scale);
        // PRT engine: top-level scope (depth 0). Limits default to the
        // recommended caps from §12 (64 portals, 1024×512, 100k
        // scrollback, 1MiB writes, depth 8) and feature bits for every
        // event category Phase 3 wires (bell/title/icon/cwd/clipboard/
        // mouse mode + alt-screen-in-portal). Cell metrics are passed
        // through so per-portal VGE engines (§10) inherit them.
        let prt_engine = prt::PrtEngine::with_metrics(cell_px, scale);

        // Create PTY and parser
        let parser = vt100::Parser::new(term_rows, term_cols, 10000);
        let pty = pty::Pty::new(term_rows, term_cols).expect("Failed to create PTY");

        // Start PTY reader thread
        let (tx, rx) = mpsc::channel();
        let reader_fd = pty.dup_master().expect("Failed to dup master fd");
        let proxy = self.proxy.clone();

        std::thread::spawn(move || {
            let mut file = std::fs::File::from(reader_fd);
            let mut buf = [0u8; 4096];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        let _ = proxy.send_event(());
                    }
                    Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
                    Err(_) => break,
                }
            }
            // Drop sender so the main thread sees Disconnected, then wake it up
            drop(tx);
            let _ = proxy.send_event(());
        });

        self.window = Some(window);
        self.gl_surface = Some(gl_surface);
        self.gl_context = Some(gl_context);
        self.canvas = Some(canvas);
        self.parser = Some(parser);
        self.pty = Some(pty);
        self.term_renderer = Some(term_renderer);
        self.rx = Some(rx);
        self.vge = Some(vge_engine);
        self.prt = Some(prt_engine);

        self.window.as_ref().unwrap().request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let (Some(surface), Some(context)) = (&self.gl_surface, &self.gl_context) {
                    surface.resize(
                        context,
                        NonZeroU32::new(size.width.max(1)).unwrap(),
                        NonZeroU32::new(size.height.max(1)).unwrap(),
                    );
                }
                if let Some(tr) = &self.term_renderer {
                    let (cols, rows) = tr.terminal_size(size.width, size.height);
                    if let Some(parser) = &mut self.parser {
                        parser.screen_mut().set_size(rows, cols);
                    }
                    if let Some(pty) = &self.pty {
                        pty.resize(rows, cols);
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some(position);
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // Decide whether to forward the wheel as a mouse-button
                // event to the PTY (when the inner program has enabled
                // mouse reporting in SGR encoding) or use it for the
                // host's own scrollback.
                let (mode, encoding) = self
                    .parser
                    .as_ref()
                    .map(|p| {
                        let s = p.screen();
                        (s.mouse_protocol_mode(), s.mouse_protocol_encoding())
                    })
                    .unwrap_or((
                        vt100::MouseProtocolMode::None,
                        vt100::MouseProtocolEncoding::Default,
                    ));
                let forward = mode != vt100::MouseProtocolMode::None
                    && matches!(encoding, vt100::MouseProtocolEncoding::Sgr);

                if forward {
                    // Convert pointer position + delta into wheel ticks
                    // (1 line = 1 tick). LineDelta is platform-native;
                    // PixelDelta (touchpads) gets binned by cell height.
                    let cell_h = self
                        .term_renderer
                        .as_ref()
                        .map(|t| t.cell_height)
                        .unwrap_or(20.0);
                    let cell_w = self
                        .term_renderer
                        .as_ref()
                        .map(|t| t.cell_width)
                        .unwrap_or(9.0);
                    let ticks = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => y as i32,
                        winit::event::MouseScrollDelta::PixelDelta(pos) => {
                            (pos.y / cell_h as f64).round() as i32
                        }
                    };
                    if ticks != 0 {
                        let (col, row) = self
                            .cursor_pos
                            .map(|p| {
                                let c = (p.x / cell_w as f64).floor().max(0.0) as u32 + 1;
                                let r = (p.y / cell_h as f64).floor().max(0.0) as u32 + 1;
                                (c, r)
                            })
                            .unwrap_or((1, 1));
                        let button = if ticks > 0 { 64 } else { 65 };
                        let mut payload = Vec::with_capacity(16 * ticks.unsigned_abs() as usize);
                        for _ in 0..ticks.unsigned_abs() {
                            payload.extend_from_slice(
                                format!("\x1b[<{button};{col};{row}M").as_bytes(),
                            );
                        }
                        if let Some(pty) = &self.pty {
                            let _ = pty.write_all(&payload);
                        }
                    }
                } else {
                    let lines = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as isize,
                        winit::event::MouseScrollDelta::PixelDelta(pos) => {
                            let ch = self
                                .term_renderer
                                .as_ref()
                                .map(|t| t.cell_height)
                                .unwrap_or(20.0);
                            (pos.y as f32 / ch) as isize
                        }
                    };
                    if let Some(parser) = &mut self.parser {
                        let screen = parser.screen_mut();
                        let current = screen.scrollback() as isize;
                        screen.set_scrollback((current + lines).max(0) as usize);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_key_input(&event);
            }

            WindowEvent::RedrawRequested => {
                let size = self.window.as_ref().unwrap().inner_size();
                let canvas = self.canvas.as_mut().unwrap();
                canvas.set_size(size.width, size.height, 1.0);
                canvas.clear_rect(0, 0, size.width, size.height, Color::rgb(30, 30, 30));

                if let (Some(parser), Some(tr), Some(engine), Some(prt)) = (
                    &mut self.parser,
                    &mut self.term_renderer,
                    &mut self.vge,
                    &mut self.prt,
                ) {
                    // Drop GPU resources for any images that were
                    // dropped since the last frame — both the host
                    // VGE engine's queue and every per-portal VGE
                    // engine's queue, plus anything the PRT engine
                    // accumulated when portals were torn down (delete
                    // / clear / scope_reset / 2J / 3J / scrollback
                    // eviction / alt-swap leave).
                    for gpu_id in engine.take_pending_image_deletes() {
                        canvas.delete_image(gpu_id);
                    }
                    for gpu_id in prt.take_all_pending_image_deletes() {
                        canvas.delete_image(gpu_id);
                    }

                    // Probe actual scrollback buffer size (no public accessor)
                    let current = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(usize::MAX);
                    let max_scrollback = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(current);

                    tr.render(
                        canvas,
                        parser.screen(),
                        max_scrollback,
                        &engine.state,
                        engine.top_of_live_screen(),
                        &prt.state,
                    );
                }

                canvas.flush();
                self.gl_surface
                    .as_ref()
                    .unwrap()
                    .swap_buffers(self.gl_context.as_ref().unwrap())
                    .unwrap();
            }

            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: ()) {
        let alive = self.process_pty_output();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        if !alive {
            event_loop.exit();
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().unwrap();
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app).unwrap();
}
