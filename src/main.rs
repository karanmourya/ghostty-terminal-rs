//! ghostty-windows - a pure-Rust terminal emulator for Windows, inspired by
//! [Ghostty](https://ghostty.org).
//!
//! ## Layer mapping
//!
//! | Ghostty (Zig/C/Swift)            | this project (Rust)            |
//! |----------------------------------|--------------------------------|
//! | `src/terminal`, `src/vt`         | `term.rs` + the `vte` crate     |
//! | macOS `AppKit` app / Linux `GTK` | `winit`                        |
//! | `Metal` / `OpenGL` renderer      | `wgpu` + `quad.rs`             |
//! | `CoreText` font shaping          | `glyphon` (cosmic-text)        |
//! | macOS `PTY` / Linux `pty`        | `portable-pty` (ConPTY)        |
//!
//! This is a clean-room Rust implementation, not a line-by-line port of the Zig
//! source. See README.md for what is implemented and what still needs work
//! before it can be called a Ghostty replacement.

mod quad;
mod term;

use std::sync::mpsc::{channel, Receiver, Sender};

use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer as GlyphonRenderer, Viewport, Weight,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::sync::Arc;
use term::Term;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, Ime, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId},
};

use quad::{linear, Quad, QuadRenderer};

const DEFAULT_FONT_SIZE: f32 = 15.0;
const LINE_HEIGHT_RATIO: f32 = 1.32;
const PADDING: f32 = 6.0;
const CURSOR_THICKNESS: f32 = 2.0;

enum UserEvent {
    /// The PTY reader thread has output waiting (or has exited).
    Wake,
}

// ---------------------------------------------------------------------------
// GPU state
// ---------------------------------------------------------------------------

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

impl Gpu {
    fn new(window: &Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter");

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .expect("create device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        Self {
            device,
            queue,
            surface,
            config,
        }
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }
}

// ---------------------------------------------------------------------------
// Text state (glyphon)
// ---------------------------------------------------------------------------

struct Text {
    font_system: FontSystem,
    swash: SwashCache,
    // Kept alive because `Viewport` and `TextAtlas` reference its bind groups.
    _cache: Cache,
    viewport: Viewport,
    atlas: TextAtlas,
    renderer: GlyphonRenderer,
    buffer: Option<Buffer>,
    font_size: f32,
    line_height: f32,
    cell_width: f32,
    dirty: bool,
}

impl Text {
    fn new(gpu: &Gpu, scale: f32) -> Self {
        let mut font_system = FontSystem::new();
        let swash = SwashCache::new();
        let cache = Cache::new(&gpu.device);
        let viewport = Viewport::new(&gpu.device, &cache);
        let mut atlas = TextAtlas::new(&gpu.device, &gpu.queue, &cache, gpu.config.format);
        let renderer = GlyphonRenderer::new(
            &mut atlas,
            &gpu.device,
            wgpu::MultisampleState::default(),
            None,
        );

        let font_size = DEFAULT_FONT_SIZE * scale;
        let line_height = (font_size * LINE_HEIGHT_RATIO).round();
        let cell_width = measure_cell(&mut font_system, font_size, line_height);

        Self {
            font_system,
            swash,
            _cache: cache,
            viewport,
            atlas,
            renderer,
            buffer: None,
            font_size,
            line_height,
            cell_width,
            dirty: true,
        }
    }

    fn set_scale(&mut self, scale: f32) {
        let font_size = DEFAULT_FONT_SIZE * scale;
        if (font_size - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = font_size;
        self.line_height = (font_size * LINE_HEIGHT_RATIO).round();
        self.cell_width = measure_cell(&mut self.font_system, self.font_size, self.line_height);
        self.buffer = None;
        self.dirty = true;
    }

    /// Rebuild the shaped text buffer from the terminal grid.
    fn rebuild(&mut self, term: &Term, scroll: usize, width: f32, height: f32) {
        let mut buffer = Buffer::new(
            &mut self.font_system,
            Metrics::new(self.font_size, self.line_height),
        );
        // Give shaping far more room than the window: a terminal never soft-wraps
        // (the grid already decided where lines end), and clipping is handled by
        // the TextArea bounds. Too-narrow a width would shift rows.
        buffer.set_size(
            &mut self.font_system,
            Some((width * 8.0).max(1024.0)),
            Some(height.max(1.0)),
        );

        // Group horizontal runs of identical style into spans so colored output
        // shapes in one pass instead of one buffer per cell.
        let mut spans: Vec<(String, Attrs<'static>)> = Vec::new();
        let top = term.scrollback_len().saturating_sub(scroll);
        for y in 0..term.rows {
            let mut text = String::with_capacity(term.cols);
            let mut current: Option<([u8; 3], bool, bool)> = None;
            let line = term.line(top + y);
            for x in 0..term.cols {
                // Missing lines (only possible mid-resize) render as blanks.
                let cell = match line.and_then(|line| line.get(x)) {
                    Some(cell) => *cell,
                    None => term::Cell::default(),
                };
                if cell.ch == '\0' {
                    // Right half of a wide glyph: the glyph itself advances.
                    continue;
                }
                let style = (cell.fg, cell.bold, cell.underline);
                if current != Some(style) {
                    if let Some((fg, bold, underline)) = current {
                        spans.push((std::mem::take(&mut text), attrs(fg, bold, underline)));
                    }
                    current = Some(style);
                }
                text.push(cell.ch);
            }
            if let Some((fg, bold, underline)) = current {
                spans.push((std::mem::take(&mut text), attrs(fg, bold, underline)));
            }
            // Terminate the row; cosmic-text creates one layout line per '\n'.
            match spans.last_mut() {
                Some((last, _)) => last.push('\n'),
                None => spans.push(("\n".to_owned(), Attrs::new())),
            }
        }

        buffer.set_rich_text(
            &mut self.font_system,
            spans.iter().map(|(text, attrs)| (text.as_str(), *attrs)),
            attrs(term::DEFAULT_FG, false, false),
            Shaping::Advanced,
        );
        buffer.shape_until_scroll(&mut self.font_system, false);

        self.buffer = Some(buffer);
        self.dirty = false;
    }
}

fn attrs(fg: [u8; 3], bold: bool, _underline: bool) -> Attrs<'static> {
    let mut a = Attrs::new()
        .family(Family::Monospace)
        .color(Color::rgb(fg[0], fg[1], fg[2]));
    if bold {
        a = a.weight(Weight::BOLD);
    }
    a
}

/// Measure the advance width of the monospace font so the grid, cursor, and
/// background rectangles line up with the shaped glyphs.
fn measure_cell(font_system: &mut FontSystem, font_size: f32, line_height: f32) -> f32 {
    let mut probe = Buffer::new(font_system, Metrics::new(font_size, line_height));
    probe.set_size(font_system, Some(10_000.0), Some(line_height));
    probe.set_text(
        font_system,
        "MMMMMMMMMM",
        Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
    );
    probe.shape_until_scroll(font_system, false);
    if let Some(run) = probe.layout_runs().next() {
        if run.glyphs.len() >= 2 {
            return run.glyphs[1].x - run.glyphs[0].x;
        }
        if let Some(glyph) = run.glyphs.first() {
            return glyph.w;
        }
    }
    font_size * 0.6
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct App {
    gpu: Option<Gpu>,
    text: Option<Text>,
    quads: Option<QuadRenderer>,
    window: Option<Arc<Window>>,
    proxy: Option<EventLoopProxy<UserEvent>>,

    term: Term,
    parser: vte::Parser,
    pty_writer: Option<Box<dyn std::io::Write + Send>>,
    pty_master: Option<Box<dyn MasterPty + Send>>,
    rx: Option<Receiver<Vec<u8>>>,

    /// Lines scrolled up from the live screen (0 = bottom).
    scroll: usize,

    modifiers: ModifiersState,
    width: u32,
    height: u32,
    scale: f32,
}

impl App {
    fn new() -> Self {
        Self {
            gpu: None,
            text: None,
            quads: None,
            window: None,
            proxy: None,
            term: Term::new(80, 24),
            parser: vte::Parser::new(),
            pty_writer: None,
            pty_master: None,
            rx: None,
            scroll: 0,
            modifiers: ModifiersState::empty(),
            width: 1000,
            height: 640,
            scale: 1.0,
        }
    }

    // -- terminal geometry --------------------------------------------------

    fn dimensions(&self) -> (usize, usize) {
        let Some(text) = self.text.as_ref() else {
            return (80, 24);
        };
        let usable_w = (self.width as f32 - PADDING * 2.0).max(text.cell_width);
        let usable_h = (self.height as f32 - PADDING * 2.0).max(text.line_height);
        (
            (usable_w / text.cell_width).floor() as usize,
            (usable_h / text.line_height).floor() as usize,
        )
    }

    fn sync_size(&mut self) {
        let (cols, rows) = self.dimensions();
        if cols == self.term.cols && rows == self.term.rows {
            return;
        }
        self.term.resize(cols, rows);
        if let Some(master) = self.pty_master.as_ref() {
            let _ = master.resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: self.width as u16,
                pixel_height: self.height as u16,
            });
        }
        if let Some(text) = self.text.as_mut() {
            text.buffer = None;
        }
        self.scroll = self.scroll.min(self.term.scrollback_len());
    }

    // -- scrolling ---------------------------------------------------------

    fn scroll_by(&mut self, lines: i32) {
        let max = self.term.scrollback_len() as i32;
        let next = (self.scroll as i32 + lines).clamp(0, max) as usize;
        self.set_scroll(next);
    }

    fn set_scroll(&mut self, next: usize) {
        let next = next.min(self.term.scrollback_len());
        if next == self.scroll {
            return;
        }
        self.scroll = next;
        if let Some(text) = self.text.as_mut() {
            text.buffer = None;
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn jump_to_bottom(&mut self) {
        self.set_scroll(0);
    }

    // -- PTY ---------------------------------------------------------------

    fn spawn_pty(&mut self) {
        // `GHOSTTY_WIN_SHELL` may include arguments, e.g. "pwsh.exe -NoLogo".
        let shell_line =
            std::env::var("GHOSTTY_WIN_SHELL").unwrap_or_else(|_| "powershell.exe".into());
        let mut parts = shell_line.split_whitespace();
        let shell = parts.next().unwrap_or("powershell.exe").to_string();
        let mut cmd = CommandBuilder::new(&shell);
        for arg in parts {
            cmd.arg(arg);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.cwd(std::env::current_dir().unwrap_or_default());

        let pty_system = portable_pty::native_pty_system();
        let pair = match pty_system.openpty(PtySize {
            rows: self.term.rows as u16,
            cols: self.term.cols as u16,
            pixel_width: self.width as u16,
            pixel_height: self.height as u16,
        }) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("failed to open ConPTY: {err}");
                return;
            }
        };

        let mut reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(err) => {
                eprintln!("failed to clone ConPTY reader: {err}");
                return;
            }
        };
        self.pty_writer = pair.master.take_writer().ok();
        self.pty_master = Some(pair.master);

        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
        self.rx = Some(rx);
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        if let Some(proxy) = proxy.as_ref() {
                            let _ = proxy.send_event(UserEvent::Wake);
                        }
                    }
                }
            }
            if let Some(proxy) = proxy.as_ref() {
                let _ = proxy.send_event(UserEvent::Wake);
            }
        });

        if let Err(err) = pair.slave.spawn_command(cmd) {
            eprintln!("failed to spawn {shell}: {err}; falling back to cmd.exe");
            let mut fallback = CommandBuilder::new("cmd.exe");
            fallback.env("TERM", "xterm-256color");
            let _ = pair.slave.spawn_command(fallback);
        }
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        if let Some(writer) = self.pty_writer.as_mut() {
            use std::io::Write;
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    fn drain_pty(&mut self) {
        let mut chunks = Vec::new();
        if let Some(rx) = self.rx.as_ref() {
            while let Ok(chunk) = rx.try_recv() {
                chunks.push(chunk);
            }
        }
        let history_before = self.term.scrollback_len();
        let mut any = false;
        for chunk in chunks {
            self.term.feed(&mut self.parser, &chunk);
            any = true;
        }
        // Keep a scrolled-back viewport anchored to the same lines while new
        // output pushes lines into history.
        if self.scroll > 0 {
            let added = self.term.scrollback_len().saturating_sub(history_before);
            if added > 0 {
                self.scroll = (self.scroll + added).min(self.term.scrollback_len());
            }
        }
        let dirty = any && self.term.take_dirty();
        if dirty {
            for response in self.term.take_responses() {
                self.write_pty(&response);
            }
            if let (Some(text), Some(window)) = (self.text.as_mut(), self.window.as_ref()) {
                text.buffer = None;
                window.request_redraw();
            }
            if let Some(title) = self.term.title.clone() {
                if let Some(window) = self.window.as_ref() {
                    window.set_title(&format!("{title} - Ghostty (Rust)"));
                }
            }
        }
    }

    // -- input -------------------------------------------------------------

    fn encode_key(&self, key: &winit::event::KeyEvent) -> Option<Vec<u8>> {
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let esc = if alt {
            b"\x1b".as_slice()
        } else {
            b"".as_slice()
        };

        let cursor_prefix: &[u8] = if self.term.app_cursor_keys {
            b"\x1bO"
        } else {
            b"\x1b["
        };

        let plain: Vec<u8> = match &key.logical_key {
            Key::Named(NamedKey::Enter) => b"\r".to_vec(),
            Key::Named(NamedKey::Backspace) => b"\x7f".to_vec(),
            Key::Named(NamedKey::Tab) => b"\t".to_vec(),
            Key::Named(NamedKey::Escape) => b"\x1b".to_vec(),
            Key::Named(NamedKey::ArrowUp) => [cursor_prefix, b"A"].concat(),
            Key::Named(NamedKey::ArrowDown) => [cursor_prefix, b"B"].concat(),
            Key::Named(NamedKey::ArrowRight) => [cursor_prefix, b"C"].concat(),
            Key::Named(NamedKey::ArrowLeft) => [cursor_prefix, b"D"].concat(),
            Key::Named(NamedKey::Home) => [cursor_prefix, b"H"].concat(),
            Key::Named(NamedKey::End) => [cursor_prefix, b"F"].concat(),
            Key::Named(NamedKey::Insert) => b"\x1b[2~".to_vec(),
            Key::Named(NamedKey::Delete) => b"\x1b[3~".to_vec(),
            Key::Named(NamedKey::PageUp) => b"\x1b[5~".to_vec(),
            Key::Named(NamedKey::PageDown) => b"\x1b[6~".to_vec(),
            Key::Named(NamedKey::F1) => b"\x1bOP".to_vec(),
            Key::Named(NamedKey::F2) => b"\x1bOQ".to_vec(),
            Key::Named(NamedKey::F3) => b"\x1bOR".to_vec(),
            Key::Named(NamedKey::F4) => b"\x1bOS".to_vec(),
            Key::Named(NamedKey::F5) => b"\x1b[15~".to_vec(),
            Key::Named(NamedKey::F6) => b"\x1b[17~".to_vec(),
            Key::Named(NamedKey::F7) => b"\x1b[18~".to_vec(),
            Key::Named(NamedKey::F8) => b"\x1b[19~".to_vec(),
            Key::Named(NamedKey::F9) => b"\x1b[20~".to_vec(),
            Key::Named(NamedKey::F10) => b"\x1b[21~".to_vec(),
            Key::Named(NamedKey::F11) => b"\x1b[23~".to_vec(),
            Key::Named(NamedKey::F12) => b"\x1b[24~".to_vec(),
            Key::Named(NamedKey::Space) => b" ".to_vec(),
            Key::Character(text) => {
                if ctrl {
                    // Ctrl+<letter> => C0 control code; Ctrl+@ etc. per xterm.
                    match text.chars().next() {
                        Some(c) if c.is_ascii_alphabetic() => {
                            vec![(c.to_ascii_lowercase() as u8) - 0x60]
                        }
                        Some('[') => vec![0x1b],
                        Some('\\') => vec![0x1c],
                        Some(']') => vec![0x1d],
                        Some('^') => vec![0x1e],
                        Some('_') => vec![0x1f],
                        _ => text.as_bytes().to_vec(),
                    }
                } else {
                    text.as_bytes().to_vec()
                }
            }
            _ => return None,
        };

        if plain.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(plain.len() + 1);
        out.extend_from_slice(esc);
        out.extend_from_slice(&plain);
        Some(out)
    }

    fn bracket_paste(&mut self) {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            if let Ok(text) = clipboard.get_text() {
                let normalized = text.replace("\r\n", "\r");
                if self.term.bracketed_paste {
                    self.write_pty(b"\x1b[200~");
                    self.write_pty(normalized.as_bytes());
                    self.write_pty(b"\x1b[201~");
                } else {
                    self.write_pty(normalized.as_bytes());
                }
            }
        }
    }

    // -- rendering ---------------------------------------------------------

    fn render(&mut self) {
        let (Some(gpu), Some(text), Some(quads), Some(window)) = (
            self.gpu.as_ref(),
            self.text.as_mut(),
            self.quads.as_mut(),
            self.window.as_ref(),
        ) else {
            return;
        };
        if gpu.config.width == 0 || gpu.config.height == 0 {
            return;
        }

        let surface_width = gpu.config.width as f32;
        let surface_height = gpu.config.height as f32;
        let cols = self.term.cols;
        let rows = self.term.rows;
        let cell_w = text.cell_width;
        let line_h = text.line_height;

        if text.dirty || text.buffer.is_none() {
            text.rebuild(&self.term, self.scroll, surface_width, surface_height);
        }

        // --- background quads + cursor quads ---
        let top = self.term.scrollback_len().saturating_sub(self.scroll);
        let mut instances: Vec<Quad> = Vec::new();
        for y in 0..rows {
            let line = self.term.line(top + y);
            for x in 0..cols {
                let Some(cell) = line.and_then(|line| line.get(x)) else {
                    continue;
                };
                if cell.bg != term::DEFAULT_BG {
                    instances.push(Quad {
                        rect: [
                            PADDING + x as f32 * cell_w,
                            PADDING + y as f32 * line_h,
                            cell_w,
                            line_h,
                        ],
                        color: linear(cell.bg, 1.0),
                    });
                }
                // glyphon has no underline support, so draw it as a thin quad.
                if cell.underline {
                    instances.push(Quad {
                        rect: [
                            PADDING + x as f32 * cell_w,
                            PADDING + y as f32 * line_h + line_h - 2.5,
                            cell_w,
                            1.5,
                        ],
                        color: linear(cell.fg, 1.0),
                    });
                }
            }
        }
        let bg_count = instances.len() as u32;

        // The scrollbar only appears when there is history and only while
        // scrolled away from the live screen (like Ghostty's overlay).
        let history = self.term.scrollback_len();
        if history > 0 && self.scroll > 0 {
            let track_x = surface_width - PADDING - 4.0;
            let track_h = rows as f32 * line_h;
            let track_y = PADDING;
            instances.push(Quad {
                rect: [track_x, track_y, 4.0, track_h],
                color: linear(term::DEFAULT_FG, 0.10),
            });
            let visible_ratio = rows as f32 / self.term.total_lines() as f32;
            let thumb_h = (track_h * visible_ratio).max(24.0).min(track_h);
            let travel = track_h - thumb_h;
            let pos = 1.0 - (self.scroll as f32 / history as f32);
            instances.push(Quad {
                rect: [track_x, track_y + travel * pos, 4.0, thumb_h],
                color: linear(term::DEFAULT_FG, 0.45),
            });
        }

        let cursor_visible = self.term.cursor_visible && self.scroll == 0 && window.has_focus();
        if cursor_visible {
            let (cx, cy) = self.term.cursor();
            let x = PADDING + cx as f32 * cell_w;
            let y = PADDING + cy as f32 * line_h;
            let color = linear(term::DEFAULT_FG, 0.9);
            let t = CURSOR_THICKNESS;
            instances.extend_from_slice(&[
                Quad {
                    rect: [x, y, cell_w, t],
                    color,
                },
                Quad {
                    rect: [x, y + line_h - t, cell_w, t],
                    color,
                },
                Quad {
                    rect: [x, y, t, line_h],
                    color,
                },
                Quad {
                    rect: [x + cell_w - t, y, t, line_h],
                    color,
                },
            ]);
        }
        let total = instances.len() as u32;

        quads.upload(
            &gpu.device,
            &gpu.queue,
            [surface_width, surface_height],
            &instances,
        );

        text.viewport.update(
            &gpu.queue,
            Resolution {
                width: gpu.config.width,
                height: gpu.config.height,
            },
        );

        let buffer = text.buffer.as_ref().expect("text buffer");
        let prepared = text.renderer.prepare(
            &gpu.device,
            &gpu.queue,
            &mut text.font_system,
            &mut text.atlas,
            &text.viewport,
            [TextArea {
                buffer,
                left: PADDING,
                top: PADDING,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: gpu.config.width as i32,
                    bottom: gpu.config.height as i32,
                },
                default_color: Color::rgb(
                    term::DEFAULT_FG[0],
                    term::DEFAULT_FG[1],
                    term::DEFAULT_FG[2],
                ),
                custom_glyphs: &[],
            }],
            &mut text.swash,
        );
        if let Err(err) = prepared {
            eprintln!("glyphon prepare failed: {err:?}");
            return;
        }

        let frame = match gpu.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
            Err(err) => {
                eprintln!("surface error: {err:?}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        {
            let bg = linear(term::DEFAULT_BG, 1.0);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terminal"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            quads.draw_range(&mut pass, 0..bg_count);
            if let Err(err) = text.renderer.render(&text.atlas, &text.viewport, &mut pass) {
                eprintln!("glyphon render failed: {err:?}");
            }
            quads.draw_range(&mut pass, bg_count..total);
        }

        gpu.queue.submit(Some(encoder.finish()));
        frame.present();
        text.atlas.trim();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("Ghostty (Rust) - Windows")
            .with_inner_size(PhysicalSize::new(self.width, self.height));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(err) => {
                eprintln!("failed to create window: {err}");
                event_loop.exit();
                return;
            }
        };

        let gpu = Gpu::new(&window);
        let scale = window.scale_factor() as f32;
        let text = Text::new(&gpu, scale);
        let quads = QuadRenderer::new(&gpu.device, gpu.config.format);

        self.width = gpu.config.width;
        self.height = gpu.config.height;
        self.scale = scale;
        self.gpu = Some(gpu);
        self.text = Some(text);
        self.quads = Some(quads);
        self.window = Some(window);

        self.sync_size();
        self.spawn_pty();
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => self.drain_pty(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.width = size.width.max(1);
                self.height = size.height.max(1);
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size);
                }
                self.sync_size();
                if let Some(text) = self.text.as_mut() {
                    text.buffer = None;
                }
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
                if let Some(text) = self.text.as_mut() {
                    text.set_scale(self.scale);
                }
                self.sync_size();
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::Focused(_) => {
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }

                // Shift+PageUp/PageDown/Home/End drive the scrollback viewport
                // instead of being forwarded to the application.
                if self.modifiers.shift_key() && !self.modifiers.control_key() {
                    let page = self.term.rows.saturating_sub(1).max(1) as i32;
                    match event.logical_key {
                        Key::Named(NamedKey::PageUp) => {
                            self.scroll_by(page);
                            return;
                        }
                        Key::Named(NamedKey::PageDown) => {
                            self.scroll_by(-page);
                            return;
                        }
                        Key::Named(NamedKey::Home) => {
                            let max = self.term.scrollback_len();
                            self.set_scroll(max);
                            return;
                        }
                        Key::Named(NamedKey::End) => {
                            self.jump_to_bottom();
                            return;
                        }
                        _ => {}
                    }
                }

                // Ctrl+V pastes; Ctrl+Shift+V is the same thing for muscle memory.
                if self.modifiers.control_key() {
                    if let Key::Character(text) = &event.logical_key {
                        if text.eq_ignore_ascii_case("v") {
                            self.jump_to_bottom();
                            self.bracket_paste();
                            return;
                        }
                    }
                }
                if let Some(bytes) = self.encode_key(&event) {
                    // Typing always returns to the live screen, but modifier-only
                    // presses must not disturb a scrolled-back viewport.
                    self.jump_to_bottom();
                    self.write_pty(&bytes);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0).round() as i32,
                    MouseScrollDelta::PixelDelta(pos) => {
                        let line_h = self
                            .text
                            .as_ref()
                            .map(|t| t.line_height)
                            .unwrap_or(20.0)
                            .max(1.0);
                        (pos.y as f32 / line_h).round() as i32
                    }
                };
                if lines != 0 {
                    self.scroll_by(lines);
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.jump_to_bottom();
                self.write_pty(text.as_bytes());
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Belt-and-braces: also drain here in case a wake event coalesced.
        self.drain_pty();
    }
}

fn main() {
    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(err) => {
            eprintln!("failed to create event loop: {err}");
            return;
        }
    };
    let proxy = event_loop.create_proxy();

    let mut app = App::new();
    app.proxy = Some(proxy);

    if let Err(err) = event_loop.run_app(&mut app) {
        eprintln!("event loop failed: {err}");
    }
}
