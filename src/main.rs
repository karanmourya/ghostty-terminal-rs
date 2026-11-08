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

mod config;
mod quad;
mod term;

use config::{Config, CursorStyle};

use std::sync::mpsc::{channel, Receiver, Sender};

use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer as GlyphonRenderer, Viewport, Weight,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use term::Term;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId},
};

use quad::{linear, Quad, QuadRenderer};

const LINE_HEIGHT_RATIO: f32 = 1.32;
const CURSOR_THICKNESS: f32 = 2.0;
/// Highlight colour painted behind selected cells.
const SELECTION_BG: [u8; 3] = [0x58, 0x5e, 0x7a];

enum UserEvent {
    /// The PTY reader thread has output waiting (or has exited).
    Wake,
}

/// A text selection, stored in absolute line coordinates (scrollback included)
/// so it stays attached to the text while the viewport moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl Selection {
    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Top-left and bottom-right corners, in reading order.
    fn bounds(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
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
    /// Configured font size before DPI scaling.
    base_font_size: f32,
    font_size: f32,
    family: Family<'static>,
    line_height: f32,
    cell_width: f32,
    dirty: bool,
}

impl Text {
    fn new(gpu: &Gpu, scale: f32, config: &Config) -> Self {
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

        // One small leak so the family name can be 'static (the app lives for
        // the whole process, so this is never worth freeing).
        let family = match &config.font_family {
            Some(name) => Family::Name(Box::leak(name.clone().into_boxed_str())),
            None => Family::Monospace,
        };
        let font_size = config.font_size * scale;
        let line_height = (font_size * LINE_HEIGHT_RATIO).round();
        let cell_width = measure_cell(&mut font_system, &family, font_size, line_height);

        Self {
            font_system,
            swash,
            _cache: cache,
            viewport,
            atlas,
            renderer,
            buffer: None,
            base_font_size: config.font_size,
            font_size,
            family,
            line_height,
            cell_width,
            dirty: true,
        }
    }

    fn set_scale(&mut self, scale: f32) {
        let font_size = self.base_font_size * scale;
        if (font_size - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = font_size;
        self.line_height = (font_size * LINE_HEIGHT_RATIO).round();
        self.cell_width = measure_cell(
            &mut self.font_system,
            &self.family,
            self.font_size,
            self.line_height,
        );
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
                    None => term.blank_cell(),
                };
                if cell.ch == '\0' {
                    // Right half of a wide glyph: the glyph itself advances.
                    continue;
                }
                let style = (cell.fg, cell.bold, cell.underline);
                if current != Some(style) {
                    if let Some((fg, bold, underline)) = current {
                        spans.push((
                            std::mem::take(&mut text),
                            attrs(fg, bold, underline, &self.family),
                        ));
                    }
                    current = Some(style);
                }
                text.push(cell.ch);
            }
            if let Some((fg, bold, underline)) = current {
                spans.push((
                    std::mem::take(&mut text),
                    attrs(fg, bold, underline, &self.family),
                ));
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
            attrs(term.foreground(), false, false, &self.family),
            Shaping::Advanced,
        );
        buffer.shape_until_scroll(&mut self.font_system, false);

        self.buffer = Some(buffer);
        self.dirty = false;
    }
}

fn attrs(fg: [u8; 3], bold: bool, _underline: bool, family: &Family<'static>) -> Attrs<'static> {
    let mut a = Attrs::new()
        .family(*family)
        .color(Color::rgb(fg[0], fg[1], fg[2]));
    if bold {
        a = a.weight(Weight::BOLD);
    }
    a
}

/// Measure the advance width of the monospace font so the grid, cursor, and
/// background rectangles line up with the shaped glyphs.
fn measure_cell(
    font_system: &mut FontSystem,
    family: &Family<'static>,
    font_size: f32,
    line_height: f32,
) -> f32 {
    let mut probe = Buffer::new(font_system, Metrics::new(font_size, line_height));
    probe.set_size(font_system, Some(10_000.0), Some(line_height));
    probe.set_text(
        font_system,
        "MMMMMMMMMM",
        Attrs::new().family(*family),
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
    config: Config,
    parser: vte::Parser,
    pty_writer: Option<Box<dyn std::io::Write + Send>>,
    pty_master: Option<Box<dyn MasterPty + Send>>,
    rx: Option<Receiver<Vec<u8>>>,

    /// Lines scrolled up from the live screen (0 = bottom).
    scroll: usize,

    /// Last known pointer position, in grid cells.
    pointer_cell: (usize, usize),
    /// Button currently held (xterm button code) while reporting motion.
    mouse_down: Option<u8>,
    /// Active text selection and whether the pointer is dragging it out.
    selection: Option<Selection>,
    selecting: bool,
    /// Time and cell of the previous primary click, for double-click words.
    last_click: Option<(std::time::Instant, (usize, usize))>,

    modifiers: ModifiersState,
    width: u32,
    height: u32,
    scale: f32,
}

impl App {
    fn new() -> Self {
        let (config, warnings, path) = config::load();
        for warning in &warnings {
            eprintln!("config warning: {warning}");
        }
        if let Some(path) = &path {
            if warnings.is_empty() {
                eprintln!("loaded config from {}", path.display());
            }
        }
        let mut term = Term::new(80, 24);
        term.set_theme(config.foreground, config.background, config.palette);
        term.set_scrollback_limit(config.scrollback);
        Self {
            gpu: None,
            text: None,
            quads: None,
            window: None,
            proxy: None,
            term,
            config,
            parser: vte::Parser::new(),
            pty_writer: None,
            pty_master: None,
            rx: None,
            scroll: 0,
            pointer_cell: (0, 0),
            mouse_down: None,
            selection: None,
            selecting: false,
            last_click: None,
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
        let usable_w = (self.width as f32 - self.config.padding * 2.0).max(text.cell_width);
        let usable_h = (self.height as f32 - self.config.padding * 2.0).max(text.line_height);
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

    // -- mouse reporting ---------------------------------------------------

    fn cell_at(&self, x: f64, y: f64) -> (usize, usize) {
        let Some(text) = self.text.as_ref() else {
            return (0, 0);
        };
        let col = ((x as f32 - self.config.padding) / text.cell_width)
            .floor()
            .max(0.0) as usize;
        let row = ((y as f32 - self.config.padding) / text.line_height)
            .floor()
            .max(0.0) as usize;
        (
            col.min(self.term.cols.saturating_sub(1)),
            row.min(self.term.rows.saturating_sub(1)),
        )
    }

    /// Encode an xterm mouse report, or `None` when the application did not
    /// ask for mouse events. Shift is reserved for local selection/scrollback.
    fn mouse_report(&self, button: u8, release: bool) -> Option<Vec<u8>> {
        if self.term.mouse_mode() == term::MouseMode::Off || self.modifiers.shift_key() {
            return None;
        }
        let (col, row) = self.pointer_cell;
        Some(encode_mouse_report(
            button,
            release,
            self.term.mouse_sgr,
            col,
            row,
            self.modifiers.alt_key(),
            self.modifiers.control_key(),
        ))
    }

    fn report_mouse(&mut self, button: u8, release: bool) {
        if let Some(bytes) = self.mouse_report(button, release) {
            self.write_pty(&bytes);
        }
    }

    /// Motion report: `?1002` only streams while a button is held, `?1003`
    /// streams all motion. The xterm motion bit is 32.
    fn report_motion(&mut self) {
        if self.scroll > 0 {
            return;
        }
        match self.term.mouse_mode() {
            term::MouseMode::Move => {}
            term::MouseMode::Drag if self.mouse_down.is_some() => {}
            _ => return,
        }
        let code = self.mouse_down.unwrap_or(3) + 32;
        self.report_mouse(code, false);
    }

    // -- selection ---------------------------------------------------------

    /// Convert a viewport cell into absolute line coordinates.
    fn absolute_cell(&self, cell: (usize, usize)) -> (usize, usize) {
        let top = self.term.scrollback_len().saturating_sub(self.scroll);
        (top + cell.1, cell.0)
    }

    fn set_selection(&mut self, selection: Option<Selection>) {
        self.selection = selection;
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn begin_selection(&mut self, cell: (usize, usize)) {
        let at = self.absolute_cell(cell);
        let now = Instant::now();
        let double_click = self
            .last_click
            .map(|(when, previous)| {
                previous == at && now.duration_since(when) < Duration::from_millis(400)
            })
            .unwrap_or(false);
        self.last_click = Some((now, at));
        self.selecting = true;
        if double_click {
            if let Some(word) = self.word_selection(at) {
                self.set_selection(Some(word));
                return;
            }
        }
        self.set_selection(Some(Selection {
            anchor: at,
            head: at,
        }));
    }

    /// Select the word (path-ish characters included) around `at`.
    fn word_selection(&self, at: (usize, usize)) -> Option<Selection> {
        let line = self.term.line(at.0)?;
        if line.is_empty() {
            return None;
        }
        let is_word = |c: char| {
            c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\' | ':' | '~' | '@')
        };
        let mut idx = at.1.min(line.len() - 1);
        while idx > 0 && !is_word(line[idx].ch) {
            idx -= 1;
        }
        if !is_word(line[idx].ch) {
            return None;
        }
        let mut start = idx;
        while start > 0 && is_word(line[start - 1].ch) {
            start -= 1;
        }
        let mut end = idx;
        while end + 1 < line.len() && is_word(line[end + 1].ch) {
            end += 1;
        }
        Some(Selection {
            anchor: (at.0, start),
            head: (at.0, end),
        })
    }

    fn update_selection(&mut self, cell: (usize, usize)) {
        if !self.selecting {
            return;
        }
        let Some(sel) = self.selection else {
            return;
        };
        let head = self.absolute_cell(cell);
        if head != sel.head {
            self.set_selection(Some(Selection {
                anchor: sel.anchor,
                head,
            }));
        }
    }

    /// Plain text of the current selection, newline-joined, trailing blanks cut.
    fn selection_text(&self) -> Option<String> {
        self.selection
            .and_then(|sel| selection_text(&self.term, sel))
    }

    fn copy_selection(&mut self) {
        if let Some(text) = self.selection_text() {
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                let _ = clipboard.set_text(text);
            }
        }
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
        let padding = self.config.padding;

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
                if cell.bg != self.term.background() {
                    instances.push(Quad {
                        rect: [
                            padding + x as f32 * cell_w,
                            padding + y as f32 * line_h,
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
                            padding + x as f32 * cell_w,
                            padding + y as f32 * line_h + line_h - 2.5,
                            cell_w,
                            1.5,
                        ],
                        color: linear(cell.fg, 1.0),
                    });
                }
            }
        }
        // --- selection highlight (painted over cell backgrounds, under text) ---
        if let Some(sel) = self.selection.filter(|sel| !sel.is_empty()) {
            let ((start_line, start_col), (end_line, end_col)) = sel.bounds();
            let color = linear(SELECTION_BG, 0.85);
            for y in 0..rows {
                let line_index = top + y;
                if line_index < start_line || line_index > end_line {
                    continue;
                }
                let first = if line_index == start_line {
                    start_col.min(cols - 1)
                } else {
                    0
                };
                let last = if line_index == end_line {
                    end_col.min(cols - 1)
                } else {
                    cols - 1
                };
                if first > last {
                    continue;
                }
                instances.push(Quad {
                    rect: [
                        padding + first as f32 * cell_w,
                        padding + y as f32 * line_h,
                        (last - first + 1) as f32 * cell_w,
                        line_h,
                    ],
                    color,
                });
            }
        }
        let bg_count = instances.len() as u32;

        // The scrollbar only appears when there is history and only while
        // scrolled away from the live screen (like Ghostty's overlay).
        let history = self.term.scrollback_len();
        if history > 0 && self.scroll > 0 {
            let track_x = surface_width - padding - 4.0;
            let track_h = rows as f32 * line_h;
            let track_y = padding;
            instances.push(Quad {
                rect: [track_x, track_y, 4.0, track_h],
                color: linear(self.term.foreground(), 0.10),
            });
            let visible_ratio = rows as f32 / self.term.total_lines() as f32;
            let thumb_h = (track_h * visible_ratio).max(24.0).min(track_h);
            let travel = track_h - thumb_h;
            let pos = 1.0 - (self.scroll as f32 / history as f32);
            instances.push(Quad {
                rect: [track_x, track_y + travel * pos, 4.0, thumb_h],
                color: linear(self.term.foreground(), 0.45),
            });
        }

        let cursor_visible = self.term.cursor_visible && self.scroll == 0 && window.has_focus();
        if cursor_visible {
            let (cx, cy) = self.term.cursor();
            let x = padding + cx as f32 * cell_w;
            let y = padding + cy as f32 * line_h;
            let color = linear(self.term.foreground(), 0.9);
            let t = CURSOR_THICKNESS;
            match self.config.cursor_style {
                CursorStyle::Block => instances.extend_from_slice(&[
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
                ]),
                CursorStyle::Bar => instances.push(Quad {
                    rect: [x, y, t, line_h],
                    color,
                }),
                CursorStyle::Underline => instances.push(Quad {
                    rect: [x, y + line_h - t, cell_w, t],
                    color,
                }),
            }
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
                left: padding,
                top: padding,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: gpu.config.width as i32,
                    bottom: gpu.config.height as i32,
                },
                default_color: Color::rgb(
                    self.term.foreground()[0],
                    self.term.foreground()[1],
                    self.term.foreground()[2],
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
            let bg = linear(self.term.background(), 1.0);
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
        let config = self.config.clone();
        let text = Text::new(&gpu, scale, &config);
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
            WindowEvent::Focused(focused) => {
                if self.term.focus_reporting {
                    self.write_pty(if focused { b"\x1b[I" } else { b"\x1b[O" });
                }
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

                // Ctrl+Shift+C copies the selection (plain Ctrl+C stays SIGINT).
                if self.modifiers.control_key() && self.modifiers.shift_key() {
                    if let Key::Character(text) = &event.logical_key {
                        if text.eq_ignore_ascii_case("c") {
                            self.copy_selection();
                            return;
                        }
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
                    if self.selection.is_some() {
                        self.set_selection(None);
                    }
                    self.write_pty(&bytes);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Wheel events belong to the application when it enabled mouse
                // reporting; Shift keeps the local scrollback override.
                let wheel = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        if y > 0.0 {
                            Some(64u8)
                        } else if y < 0.0 {
                            Some(65)
                        } else if x > 0.0 {
                            Some(66)
                        } else if x < 0.0 {
                            Some(67)
                        } else {
                            None
                        }
                    }
                    MouseScrollDelta::PixelDelta(pos) => {
                        if pos.y > 0.0 {
                            Some(64u8)
                        } else if pos.y < 0.0 {
                            Some(65)
                        } else if pos.x > 0.0 {
                            Some(66)
                        } else if pos.x < 0.0 {
                            Some(67)
                        } else {
                            None
                        }
                    }
                };
                if let Some(button) = wheel {
                    if let Some(bytes) = self.mouse_report(button, false) {
                        self.write_pty(&bytes);
                        return;
                    }
                }
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
            WindowEvent::CursorMoved { position, .. } => {
                let cell = self.cell_at(position.x, position.y);
                if cell == self.pointer_cell {
                    return;
                }
                self.pointer_cell = cell;
                if self.selecting {
                    self.update_selection(cell);
                } else {
                    self.report_motion();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(code) = mouse_button_code(button) else {
                    return;
                };
                // The application gets the mouse unless Shift is held, which is
                // the local override for selecting text.
                let reports =
                    self.term.mouse_mode() != term::MouseMode::Off && !self.modifiers.shift_key();
                match state {
                    ElementState::Pressed => {
                        self.mouse_down = Some(code);
                        if reports {
                            // A click while scrolled back means "back to live".
                            self.jump_to_bottom();
                            self.report_mouse(code, false);
                        } else if code == 0 {
                            self.begin_selection(self.pointer_cell);
                        }
                    }
                    ElementState::Released => {
                        self.mouse_down = None;
                        if reports {
                            self.report_mouse(code, true);
                        } else if self.selecting && code == 0 {
                            self.selecting = false;
                            if self.selection.is_none_or(|sel| sel.is_empty()) {
                                self.set_selection(None);
                            } else {
                                self.copy_selection();
                            }
                        }
                    }
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

/// Encode one xterm mouse report. `alt`/`ctrl` add the xterm modifier bits
/// (shift is the caller's local override and never reaches the application).
fn encode_mouse_report(
    button: u8,
    release: bool,
    sgr: bool,
    col: usize,
    row: usize,
    alt: bool,
    ctrl: bool,
) -> Vec<u8> {
    let mut code = button + u8::from(alt) * 8 + u8::from(ctrl) * 16;
    let (x, y) = (col + 1, row + 1);
    if sgr {
        let end = if release { 'm' } else { 'M' };
        format!("\x1b[<{code};{x};{y}{end}").into_bytes()
    } else {
        // Legacy X10 encoding cannot name the released button.
        if release {
            code = 3;
        }
        let x = (32 + x).min(255) as u8;
        let y = (32 + y).min(255) as u8;
        vec![0x1b, b'[', b'M', 32 + code, x, y]
    }
}

/// Plain text of `sel`, newline-joined, trailing blanks trimmed per line.
fn selection_text(term: &Term, sel: Selection) -> Option<String> {
    if sel.is_empty() {
        return None;
    }
    let ((start_line, start_col), (end_line, end_col)) = sel.bounds();
    let mut out = String::new();
    for line_index in start_line..=end_line {
        let Some(cells) = term.line(line_index) else {
            continue;
        };
        if cells.is_empty() {
            continue;
        }
        let first = if line_index == start_line {
            start_col.min(cells.len() - 1)
        } else {
            0
        };
        let last = if line_index == end_line {
            end_col.min(cells.len() - 1)
        } else {
            cells.len() - 1
        };
        if first > last {
            continue;
        }
        let mut row = String::with_capacity(last - first + 1);
        for cell in &cells[first..=last] {
            // Skip the right half of wide glyphs.
            if cell.ch != '\0' {
                row.push(cell.ch);
            }
        }
        out.push_str(row.trim_end());
        if line_index != end_line {
            out.push_str("\r\n");
        }
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn mouse_button_code(button: MouseButton) -> Option<u8> {
    // xterm button codes: left/middle/right = 0/1/2, extra buttons 8/9.
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        MouseButton::Back => Some(8),
        MouseButton::Forward => Some(9),
        MouseButton::Other(_) => None,
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

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn term_with(text: &str) -> Term {
        let mut term = Term::new(12, 4);
        let mut parser = vte::Parser::new();
        term.feed(&mut parser, text.as_bytes());
        term
    }

    #[test]
    fn single_line_is_trimmed() {
        let term = term_with("hello world");
        let sel = Selection {
            anchor: (0, 6),
            head: (0, 10),
        };
        assert_eq!(selection_text(&term, sel).as_deref(), Some("world"));
    }

    #[test]
    fn multi_line_join_uses_crlf() {
        let term = term_with("one\r\ntwo\r\nthree");
        let sel = Selection {
            anchor: (0, 1),
            head: (2, 1),
        };
        assert_eq!(
            selection_text(&term, sel).as_deref(),
            Some("ne\r\ntwo\r\nth")
        );
    }

    #[test]
    fn reversed_selection_is_normalised() {
        let term = term_with("abcd");
        let sel = Selection {
            anchor: (0, 2),
            head: (0, 1),
        };
        assert_eq!(selection_text(&term, sel).as_deref(), Some("bc"));
    }

    #[test]
    fn empty_selection_is_none() {
        let term = term_with("abcd");
        let sel = Selection {
            anchor: (0, 1),
            head: (0, 1),
        };
        assert_eq!(selection_text(&term, sel), None);
    }

    #[test]
    fn wide_glyphs_copy_once() {
        let term = term_with("日本");
        let sel = Selection {
            anchor: (0, 0),
            head: (0, 3),
        };
        assert_eq!(selection_text(&term, sel).as_deref(), Some("日本"));
    }

    #[test]
    fn selection_reaches_into_scrollback() {
        let mut term = Term::new(8, 2);
        let mut parser = vte::Parser::new();
        term.feed(&mut parser, b"one\r\ntwo\r\nthree");
        let sel = Selection {
            anchor: (0, 0),
            head: (0, 2),
        };
        assert_eq!(selection_text(&term, sel).as_deref(), Some("one"));
    }
}

#[cfg(test)]
mod mouse_tests {
    use super::*;

    #[test]
    fn sgr_press_and_release() {
        assert_eq!(
            encode_mouse_report(0, false, true, 0, 0, false, false),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            encode_mouse_report(2, true, true, 4, 2, false, false),
            b"\x1b[<2;5;3m"
        );
    }

    #[test]
    fn sgr_modifiers() {
        // Ctrl+wheel up (64) at the top-left corner.
        assert_eq!(
            encode_mouse_report(64, false, true, 0, 0, false, true),
            b"\x1b[<80;1;1M"
        );
        // Alt+left press.
        assert_eq!(
            encode_mouse_report(0, false, true, 10, 4, true, false),
            b"\x1b[<8;11;5M"
        );
    }

    #[test]
    fn legacy_x10_encoding() {
        assert_eq!(
            encode_mouse_report(0, false, false, 0, 0, false, false),
            [0x1b, b'[', b'M', 32, 33, 33]
        );
        // Release loses the button number in X10 mode.
        assert_eq!(
            encode_mouse_report(2, true, false, 1, 1, false, false),
            [0x1b, b'[', b'M', 35, 34, 34]
        );
        // Motion: button 3 (none) plus the motion bit.
        assert_eq!(
            encode_mouse_report(35, false, false, 0, 0, false, false),
            [0x1b, b'[', b'M', 67, 33, 33]
        );
    }

    #[test]
    fn coordinates_clamp_to_the_legacy_byte_range() {
        let bytes = encode_mouse_report(0, false, false, 300, 300, false, false);
        assert_eq!(bytes[4], 255);
        assert_eq!(bytes[5], 255);
    }
}
