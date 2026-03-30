mod pty;
mod renderer;

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

    // GL state (dropped in reverse-creation order so the EGL surface
    // is destroyed while the Wayland window still exists)
    canvas: Option<Canvas<OpenGl>>,
    gl_surface: Option<glutin::surface::Surface<WindowSurface>>,
    gl_context: Option<PossiblyCurrentContext>,
    window: Option<Arc<Window>>,

    // Input
    proxy: EventLoopProxy<()>,
    modifiers: ModifiersState,
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
            proxy,
            modifiers: ModifiersState::empty(),
        }
    }

    fn handle_key_input(&mut self, event: &winit::event::KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let pty = match &self.pty {
            Some(p) => p,
            None => return,
        };

        // Ctrl+key
        if self.modifiers.control_key() {
            match &event.logical_key {
                Key::Character(c) => {
                    if let Some(ch) = c.chars().next() {
                        if ch.is_ascii_alphabetic() {
                            let ctrl = (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                            let _ = pty.write_all(&[ctrl]);
                            return;
                        }
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
        if self.modifiers.alt_key() && !self.modifiers.control_key() {
            if let Some(text) = &event.text {
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(text.as_bytes());
                let _ = pty.write_all(&bytes);
                return;
            }
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

        loop {
            match rx.try_recv() {
                Ok(data) => parser.process(&data),
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

        // Create PTY and parser
        let parser = vt100::Parser::new(term_rows, term_cols, 0);
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

                if let (Some(parser), Some(tr)) = (&self.parser, &mut self.term_renderer) {
                    tr.render(canvas, parser.screen());
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
