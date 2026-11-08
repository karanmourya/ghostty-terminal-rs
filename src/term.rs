//! `term` — the terminal state machine.
//!
//! This is the Rust equivalent of Ghostty's Zig `src/terminal` + `src/vt`:
//! it owns the screen grid and implements `vte::Perform` so the `vte` crate
//! can drive it with parsed escape sequences.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

/// Maximum number of scrolled-off lines kept in history.
pub const SCROLLBACK_LIMIT: usize = 10_000;

pub const DEFAULT_FG: [u8; 3] = [0xcc, 0xcc, 0xcc];
pub const DEFAULT_BG: [u8; 3] = [0x0c, 0x0c, 0x0c];

/// Standard 16-color ANSI palette (xterm-ish defaults).
pub const PALETTE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0xaa, 0x00, 0x00],
    [0x00, 0xaa, 0x00],
    [0xaa, 0x85, 0x00],
    [0x00, 0x00, 0xaa],
    [0xaa, 0x00, 0xaa],
    [0x00, 0xaa, 0xaa],
    [0xaa, 0xaa, 0xaa],
    [0x55, 0x55, 0x55],
    [0xff, 0x55, 0x55],
    [0x55, 0xff, 0x55],
    [0xff, 0xff, 0x55],
    [0x55, 0x55, 0xff],
    [0xff, 0x55, 0xff],
    [0x55, 0xff, 0xff],
    [0xff, 0xff, 0xff],
];

/// A single grid cell.
///
/// `ch == '\0'` marks the right half of a double-width character; the renderer
/// skips it so the wide glyph can advance across two columns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
    pub underline: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            underline: false,
        }
    }
}

impl Cell {
    fn blank(fg: [u8; 3], bg: [u8; 3]) -> Self {
        Self {
            ch: ' ',
            fg,
            bg,
            bold: false,
            underline: false,
        }
    }
}

/// Which mouse events the application asked to receive (DECSET 1000/1002/1003).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseMode {
    /// No mouse reporting; the terminal uses the mouse for itself.
    #[default]
    Off,
    /// Button presses and releases only (`?1000`, also X10 `?9`).
    Click,
    /// Presses, releases, and motion while a button is held (`?1002`).
    Drag,
    /// All motion, button or not (`?1003`).
    Move,
}

#[derive(Clone)]
struct SavedScreen {
    cells: Vec<Cell>,
    cx: usize,
    cy: usize,
}

pub struct Term {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<Cell>,
    /// Lines that scrolled off the top of the visible grid.
    scrollback: VecDeque<Vec<Cell>>,
    cx: usize,
    cy: usize,
    fg: [u8; 3],
    bg: [u8; 3],
    /// Colours used for SGR 39/49 and for new blank cells.
    default_fg: [u8; 3],
    default_bg: [u8; 3],
    /// 16-colour ANSI palette (configurable).
    palette: [[u8; 3]; 16],
    /// How many scrolled-off lines to keep.
    scrollback_limit: usize,
    bold: bool,
    underline: bool,
    inverse: bool,
    wrap_pending: bool,
    /// Alternate-screen buffer (DEC set 47/1047/1049).
    saved: Option<SavedScreen>,
    /// Cursor saved by DECSC (ESC 7) / restored by DECRC (ESC 8).
    saved_cursor: Option<(usize, usize)>,
    pub cursor_visible: bool,
    pub app_cursor_keys: bool,
    pub bracketed_paste: bool,
    /// Mouse tracking modes; the highest enabled one wins.
    click_mouse: bool,
    drag_mouse: bool,
    move_mouse: bool,
    /// SGR mouse encoding (`?1006`) instead of the legacy X10 bytes.
    pub mouse_sgr: bool,
    /// Focus in/out reporting (`?1004`).
    pub focus_reporting: bool,
    pub title: Option<String>,
    /// Texts queued by OSC 52 for the app to put on the clipboard.
    clipboard_writes: Vec<String>,
    /// Set when an app queries the clipboard with OSC 52 `?`.
    clipboard_query: bool,
    /// Whether OSC 52 writes / read-queries are honoured.
    pub osc52_write: bool,
    pub osc52_read: bool,
    responses: Vec<Vec<u8>>,
    dirty: bool,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![Cell::blank(DEFAULT_FG, DEFAULT_BG); cols * rows],
            scrollback: VecDeque::new(),
            cx: 0,
            cy: 0,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            default_fg: DEFAULT_FG,
            default_bg: DEFAULT_BG,
            palette: PALETTE,
            scrollback_limit: SCROLLBACK_LIMIT,
            bold: false,
            underline: false,
            inverse: false,
            wrap_pending: false,
            saved: None,
            saved_cursor: None,
            cursor_visible: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            click_mouse: false,
            drag_mouse: false,
            move_mouse: false,
            mouse_sgr: false,
            focus_reporting: false,
            title: None,
            clipboard_writes: Vec::new(),
            clipboard_query: false,
            osc52_write: true,
            osc52_read: true,
            responses: Vec::new(),
            dirty: true,
        }
    }

    /// A cell with the current default colours.
    fn blank(&self) -> Cell {
        Cell::blank(self.default_fg, self.default_bg)
    }

    /// Cell used by the renderer for lines that do not exist yet.
    pub fn blank_cell(&self) -> Cell {
        self.blank()
    }

    pub fn foreground(&self) -> [u8; 3] {
        self.default_fg
    }

    pub fn background(&self) -> [u8; 3] {
        self.default_bg
    }

    /// Apply a theme and repaint whatever was using the old defaults.
    pub fn set_theme(&mut self, fg: [u8; 3], bg: [u8; 3], palette: [[u8; 3]; 16]) {
        let (old_fg, old_bg) = (self.default_fg, self.default_bg);
        for cell in self
            .cells
            .iter_mut()
            .chain(self.scrollback.iter_mut().flatten())
        {
            if cell.fg == old_fg {
                cell.fg = fg;
            }
            if cell.bg == old_bg {
                cell.bg = bg;
            }
        }
        self.default_fg = fg;
        self.default_bg = bg;
        self.palette = palette;
        if self.fg == old_fg {
            self.fg = fg;
        }
        if self.bg == old_bg {
            self.bg = bg;
        }
        self.dirty = true;
    }

    /// Cap the history buffer, dropping the oldest lines immediately.
    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit.max(1);
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
    }

    /// Feed raw PTY bytes through the parser.
    pub fn feed(&mut self, parser: &mut vte::Parser, bytes: &[u8]) {
        for &b in bytes {
            parser.advance(self, b);
        }
        self.dirty = true;
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn take_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.responses)
    }

    /// Texts an application asked to place on the clipboard via OSC 52.
    pub fn take_clipboard_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clipboard_writes)
    }

    /// Whether an application queried the clipboard via OSC 52 `?`.
    pub fn take_clipboard_query(&mut self) -> bool {
        std::mem::take(&mut self.clipboard_query)
    }

    /// Grid accessor used by tests (rendering goes through [`Term::line`]).
    #[allow(dead_code)]
    pub fn cell(&self, x: usize, y: usize) -> &Cell {
        &self.cells[y * self.cols + x]
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cx, self.cy)
    }

    /// Effective mouse tracking mode: the most permissive enabled one wins.
    pub fn mouse_mode(&self) -> MouseMode {
        if self.move_mouse {
            MouseMode::Move
        } else if self.drag_mouse {
            MouseMode::Drag
        } else if self.click_mouse {
            MouseMode::Click
        } else {
            MouseMode::Off
        }
    }

    /// Number of lines currently held in history.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Total addressable lines: history followed by the visible grid.
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// Fetch a line by absolute index (`0` is the oldest scrollback line).
    /// Returns `None` when the index is out of range.
    pub fn line(&self, index: usize) -> Option<&[Cell]> {
        let history = self.scrollback.len();
        if index < history {
            return self.scrollback.get(index).map(|line| line.as_slice());
        }
        let row = index - history;
        if row >= self.rows {
            return None;
        }
        let start = row * self.cols;
        Some(&self.cells[start..start + self.cols])
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let old_cols = self.cols;
        let old_rows = self.rows;
        let blank = self.blank();
        let mut next = vec![blank; cols * rows];
        // Preserve top-left content; proper reflow is future work.
        for y in 0..rows.min(old_rows) {
            for x in 0..cols.min(old_cols) {
                next[y * cols + x] = self.cells[y * old_cols + x];
            }
        }
        // History lines are stored width-agnostic; pad or crop them so every
        // line matches `cols` after a resize.
        if cols != old_cols {
            for line in self.scrollback.iter_mut() {
                line.resize(cols, blank);
            }
        }
        self.cells = next;
        self.cols = cols;
        self.rows = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.wrap_pending = false;
        self.dirty = true;
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }

    fn scroll_up(&mut self, count: usize) {
        let n = count.min(self.rows);
        if n == 0 {
            return;
        }
        // Only the primary screen feeds scrollback; the alternate screen
        // (vim, less, htop) must not pollute history.
        if self.saved.is_none() {
            for row in 0..n {
                self.push_history_row(row);
            }
        }
        self.cells.copy_within(n * self.cols.., 0);
        let blank = self.blank();
        for c in self.cells[(self.rows - n) * self.cols..].iter_mut() {
            *c = blank;
        }
    }

    fn push_history_row(&mut self, row: usize) {
        if self.scrollback.len() >= self.scrollback_limit {
            self.scrollback.pop_front();
        }
        let start = row * self.cols;
        self.scrollback
            .push_back(self.cells[start..start + self.cols].to_vec());
    }

    fn scroll_down(&mut self, count: usize) {
        let n = count.min(self.rows);
        if n == 0 {
            return;
        }
        self.cells
            .copy_within(..(self.rows - n) * self.cols, n * self.cols);
        let blank = self.blank();
        for c in self.cells[..n * self.cols].iter_mut() {
            *c = blank;
        }
    }

    fn clear_row(&mut self, y: usize, from: usize, to: usize) {
        let to = to.min(self.cols);
        for x in from..to {
            let i = self.idx(x, y);
            self.cells[i] = Cell::blank(self.fg, self.bg);
        }
    }

    fn linefeed(&mut self) {
        if self.cy + 1 < self.rows {
            self.cy += 1;
        } else {
            self.scroll_up(1);
        }
        self.wrap_pending = false;
    }

    fn put_char(&mut self, c: char) {
        let width = c.width().unwrap_or(1).max(1);
        if width > self.cols {
            return;
        }
        if self.wrap_pending || self.cx + width > self.cols {
            self.cx = 0;
            self.linefeed();
        }
        // SGR 7 swaps the pen colors for the duration of the attribute.
        let (fg, bg) = if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        let i = self.idx(self.cx, self.cy);
        self.cells[i] = Cell {
            ch: c,
            fg,
            bg,
            bold: self.bold,
            underline: self.underline,
        };
        // Wide characters occupy a continuation cell that the renderer skips.
        if width == 2 && self.cx + 1 < self.cols {
            let j = self.idx(self.cx + 1, self.cy);
            self.cells[j] = Cell {
                ch: '\0',
                fg,
                bg,
                bold: self.bold,
                underline: self.underline,
            };
        }
        let next = self.cx + width;
        if next >= self.cols {
            self.cx = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cx = next;
        }
    }

    fn sgr(&mut self, p: &[u16]) {
        if p.is_empty() {
            self.reset_pen();
            return;
        }
        let mut i = 0;
        while i < p.len() {
            match p[i] {
                0 => self.reset_pen(),
                1 => self.bold = true,
                4 => self.underline = true,
                7 => self.inverse = true,
                22 => self.bold = false,
                24 => self.underline = false,
                27 => self.inverse = false,
                30..=37 => self.fg = self.palette[(p[i] - 30) as usize],
                40..=47 => self.bg = self.palette[(p[i] - 40) as usize],
                39 => self.fg = self.default_fg,
                49 => self.bg = self.default_bg,
                90..=97 => self.fg = self.palette[(p[i] - 90 + 8) as usize],
                100..=107 => self.bg = self.palette[(p[i] - 100 + 8) as usize],
                38 | 48 => {
                    let is_fg = p[i] == 38;
                    if p.get(i + 1) == Some(&5) && i + 2 < p.len() {
                        let color = xterm_256(p[i + 2], &self.palette);
                        if is_fg {
                            self.fg = color
                        } else {
                            self.bg = color
                        }
                        i += 2;
                    } else if p.get(i + 1) == Some(&2) && i + 4 < p.len() {
                        let color = [p[i + 2] as u8, p[i + 3] as u8, p[i + 4] as u8];
                        if is_fg {
                            self.fg = color
                        } else {
                            self.bg = color
                        }
                        i += 4;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn reset_pen(&mut self) {
        self.fg = self.default_fg;
        self.bg = self.default_bg;
        self.bold = false;
        self.underline = false;
        self.inverse = false;
    }

    fn set_private_mode(&mut self, mode: u16, enabled: bool) {
        match mode {
            1 => self.app_cursor_keys = enabled,
            9 | 1000 => self.click_mouse = enabled,
            1002 => self.drag_mouse = enabled,
            1003 => self.move_mouse = enabled,
            1004 => self.focus_reporting = enabled,
            1006 => self.mouse_sgr = enabled,
            25 => self.cursor_visible = enabled,
            2004 => self.bracketed_paste = enabled,
            47 | 1047 | 1049 => {
                if enabled {
                    if self.saved.is_none() {
                        self.saved = Some(SavedScreen {
                            cells: self.cells.clone(),
                            cx: self.cx,
                            cy: self.cy,
                        });
                    }
                    if mode == 1049 || mode == 1047 {
                        let blank = self.blank();
                        self.cells.iter_mut().for_each(|c| *c = blank);
                        self.cx = 0;
                        self.cy = 0;
                    }
                } else if let Some(s) = self.saved.take() {
                    self.cells = s.cells;
                    self.cx = s.cx.min(self.cols - 1);
                    self.cy = s.cy.min(self.rows - 1);
                }
                self.dirty = true;
            }
            _ => {}
        }
    }
}

fn xterm_256(n: u16, palette: &[[u8; 3]; 16]) -> [u8; 3] {
    let n = n as usize;
    if n < 16 {
        return palette[n];
    }
    if n < 232 {
        let n = n - 16;
        let r = (n / 36) as u8;
        let g = ((n % 36) / 6) as u8;
        let b = (n % 6) as u8;
        let scale = |v: u8| -> u8 {
            if v == 0 {
                0
            } else {
                55 + v * 40
            }
        };
        return [scale(r), scale(g), scale(b)];
    }
    let v = 8 + ((n - 232) as u8) * 10;
    [v, v, v]
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.linefeed(),
            b'\r' => {
                self.cx = 0;
                self.wrap_pending = false;
            }
            b'\t' => {
                let next = ((self.cx / 8) + 1) * 8;
                self.cx = next.min(self.cols - 1);
                self.wrap_pending = false;
            }
            0x08 => {
                if self.cx > 0 {
                    self.cx -= 1;
                }
                self.wrap_pending = false;
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first() == Some(&b'?');
        let p: Vec<u16> = params
            .iter()
            .map(|s| s.first().copied().unwrap_or(0))
            .collect();
        let arg = |i: usize, default: u16| -> usize {
            match p.get(i).copied() {
                Some(0) | None => default as usize,
                Some(v) => v as usize,
            }
        };

        match action {
            'm' => self.sgr(&p),
            'h' | 'l' if private => {
                let enabled = action == 'h';
                for mode in &p {
                    self.set_private_mode(*mode, enabled);
                }
            }
            'H' | 'f' => {
                self.cy = arg(0, 1).saturating_sub(1).min(self.rows - 1);
                self.cx = arg(1, 1).saturating_sub(1).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'A' => self.cy = self.cy.saturating_sub(arg(0, 1)),
            'B' => self.cy = (self.cy + arg(0, 1)).min(self.rows - 1),
            'C' => self.cx = (self.cx + arg(0, 1)).min(self.cols - 1),
            'D' => self.cx = self.cx.saturating_sub(arg(0, 1)),
            'E' => {
                self.cy = (self.cy + arg(0, 1)).min(self.rows - 1);
                self.cx = 0;
            }
            'F' => {
                self.cy = self.cy.saturating_sub(arg(0, 1));
                self.cx = 0;
            }
            'G' => self.cx = arg(0, 1).saturating_sub(1).min(self.cols - 1),
            'd' => self.cy = arg(0, 1).saturating_sub(1).min(self.rows - 1),
            'J' => match p.first().copied().unwrap_or(0) {
                0 => {
                    self.clear_row(self.cy, self.cx, self.cols);
                    for y in self.cy + 1..self.rows {
                        self.clear_row(y, 0, self.cols);
                    }
                }
                1 => {
                    for y in 0..self.cy {
                        self.clear_row(y, 0, self.cols);
                    }
                    self.clear_row(self.cy, 0, self.cx + 1);
                }
                _ => {
                    let blank = self.blank();
                    self.cells.iter_mut().for_each(|c| *c = blank);
                }
            },
            'K' => match p.first().copied().unwrap_or(0) {
                0 => self.clear_row(self.cy, self.cx, self.cols),
                1 => self.clear_row(self.cy, 0, self.cx + 1),
                _ => self.clear_row(self.cy, 0, self.cols),
            },
            'L' => {
                let n = arg(0, 1).min(self.rows - self.cy);
                let start = self.idx(0, self.cy);
                self.cells.copy_within(start.., start + n * self.cols);
                for y in self.cy..self.cy + n {
                    self.clear_row(y, 0, self.cols);
                }
            }
            'M' => {
                let n = arg(0, 1).min(self.rows - self.cy);
                let start = self.idx(0, self.cy);
                self.cells.copy_within(start + n * self.cols.., start);
                for y in self.rows - n..self.rows {
                    self.clear_row(y, 0, self.cols);
                }
            }
            'P' => {
                let n = arg(0, 1).min(self.cols - self.cx);
                let row_start = self.idx(0, self.cy);
                let from = self.idx(self.cx, self.cy);
                self.cells
                    .copy_within(from + n..row_start + self.cols, from);
                self.clear_row(self.cy, self.cols - n, self.cols);
            }
            '@' => {
                let n = arg(0, 1).min(self.cols - self.cx);
                let row_start = self.idx(0, self.cy);
                let from = self.idx(self.cx, self.cy);
                self.cells
                    .copy_within(from..row_start + self.cols - n, from + n);
                self.clear_row(self.cy, self.cx, self.cx + n);
            }
            'S' => self.scroll_up(arg(0, 1)),
            'T' => self.scroll_down(arg(0, 1)),
            'X' => self.clear_row(self.cy, self.cx, self.cx + arg(0, 1)),
            'n' => match p.first().copied().unwrap_or(0) {
                6 => {
                    let response = format!("\x1b[{};{}R", self.cy + 1, self.cx + 1);
                    self.responses.push(response.into_bytes());
                }
                5 => self.responses.push(b"\x1b[0n".to_vec()),
                _ => {}
            },
            'c' => {
                if p.first().copied().unwrap_or(0) == 0 {
                    // VT100 with advanced video: "I am a terminal".
                    let response = b"\x1b[?1;2c".to_vec();
                    self.responses.push(response);
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // Charset designation (ESC ( 0 etc.): accept and ignore in the MVP.
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'c' => {
                // RIS - full reset
                let blank = self.blank();
                self.cells.iter_mut().for_each(|c| *c = blank);
                self.cx = 0;
                self.cy = 0;
                self.saved = None;
                self.saved_cursor = None;
                self.wrap_pending = false;
                self.cursor_visible = true;
                self.app_cursor_keys = false;
                self.bracketed_paste = false;
                self.click_mouse = false;
                self.drag_mouse = false;
                self.move_mouse = false;
                self.mouse_sgr = false;
                self.focus_reporting = false;
                self.clipboard_writes.clear();
                self.clipboard_query = false;
                self.reset_pen();
            }
            b'D' => self.linefeed(),
            b'E' => {
                self.cx = 0;
                self.linefeed();
            }
            b'M' => {
                if self.cy > 0 {
                    self.cy -= 1;
                } else {
                    self.scroll_down(1);
                }
            }
            b'7' => {
                self.saved_cursor = Some((self.cx, self.cy));
            }
            b'8' => {
                if let Some((x, y)) = self.saved_cursor {
                    self.cx = x.min(self.cols - 1);
                    self.cy = y.min(self.rows - 1);
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 / 2 set the window title.
        if matches!(params.first(), Some(&b"0") | Some(&b"2")) {
            if let Some(title) = params.get(1) {
                self.title = Some(String::from_utf8_lossy(title).into_owned());
                self.dirty = true;
            }
        }
        // OSC 52: clipboard. `params` arrives split on ';', so a write looks
        // like ["52", "c", "<base64>"] (the selection field may hold several
        // ':'-separated targets, or be empty for the system clipboard).
        if params.first().is_some_and(|p| *p == b"52") {
            let selection = params.get(1).copied().unwrap_or(b"");
            let data = params.get(2).copied().unwrap_or(b"");
            let targets_system_clipboard = selection.is_empty()
                || selection.split(|b| *b == b':').any(|target| target == b"c");
            if targets_system_clipboard {
                if data == b"?" {
                    if self.osc52_read {
                        self.clipboard_query = true;
                    }
                } else if self.osc52_write {
                    if let Some(text) = decode_osc52(data) {
                        self.clipboard_writes.push(text);
                    }
                }
            }
        }
        // TODO: OSC 8 hyperlinks, OSC 7 cwd.
    }
}

/// Decode the base64 payload of an OSC 52 sequence into clipboard text.
fn decode_osc52(data: &[u8]) -> Option<String> {
    let bytes = base64_decode(data)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Standard base64 encoding (with padding), as used by OSC 52 replies.
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.len();
        let triple = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        out.push(ALPHABET[(triple[0] >> 2) as usize] as char);
        out.push(ALPHABET[(((triple[0] & 0x03) << 4) | (triple[1] >> 4)) as usize] as char);
        out.push(if n > 1 {
            ALPHABET[(((triple[1] & 0x0f) << 2) | (triple[2] >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if n > 2 {
            ALPHABET[(triple[2] & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Standard base64 decoding; `None` on malformed input.
fn base64_decode(data: &[u8]) -> Option<Vec<u8>> {
    fn value(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut quads = data.chunks_exact(4);
    for quad in &mut quads {
        let padding = quad.iter().filter(|b| **b == b'=').count();
        if padding > 0 && quad[..4 - padding].contains(&b'=') {
            return None;
        }
        let mut acc: u32 = 0;
        for b in quad.iter() {
            acc <<= 6;
            if *b != b'=' {
                acc |= u32::from(value(*b)?);
            }
        }
        out.push((acc >> 16) as u8);
        if padding < 2 {
            out.push((acc >> 8) as u8);
        }
        if padding < 1 {
            out.push(acc as u8);
        }
    }
    if !quads.remainder().is_empty() {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(t: &Term, y: usize) -> String {
        (0..t.cols)
            .map(|x| t.cell(x, y).ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// Text of an absolute line (scrollback followed by the visible grid).
    fn line_text(t: &Term, index: usize) -> String {
        t.line(index)
            .map(|line| {
                line.iter()
                    .map(|c| c.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .unwrap_or_default()
    }

    fn feed(t: &mut Term, s: &str) {
        let mut parser = vte::Parser::new();
        t.feed(&mut parser, s.as_bytes());
    }

    #[test]
    fn plain_text() {
        let mut t = Term::new(10, 3);
        feed(&mut t, "hello");
        assert_eq!(row_text(&t, 0), "hello");
        assert_eq!(t.cursor(), (5, 0));
    }

    #[test]
    fn crlf_and_wrap() {
        let mut t = Term::new(4, 3);
        feed(&mut t, "abcdef\r\nxy");
        assert_eq!(row_text(&t, 0), "abcd");
        assert_eq!(row_text(&t, 1), "ef");
        assert_eq!(row_text(&t, 2), "xy");
    }

    #[test]
    fn scrolling() {
        let mut t = Term::new(4, 2);
        feed(&mut t, "a\r\nb\r\nc");
        assert_eq!(row_text(&t, 0), "b");
        assert_eq!(row_text(&t, 1), "c");
    }

    #[test]
    fn scrollback_keeps_scrolled_off_lines() {
        let mut t = Term::new(6, 2);
        feed(&mut t, "one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.total_lines(), 4);
        assert_eq!(line_text(&t, 0), "one");
        assert_eq!(line_text(&t, 1), "two");
        assert_eq!(line_text(&t, 2), "three");
        assert_eq!(line_text(&t, 3), "four");
        assert!(t.line(4).is_none());
    }

    #[test]
    fn alternate_screen_does_not_pollute_history() {
        let mut t = Term::new(4, 2);
        feed(&mut t, "keep\r\n");
        assert_eq!(t.scrollback_len(), 0);
        feed(&mut t, "\x1b[?1049h");
        feed(&mut t, "a\r\nb\r\nc\r\nd");
        assert_eq!(t.scrollback_len(), 0);
        feed(&mut t, "\x1b[?1049l");
        assert_eq!(line_text(&t, 0), "keep");
    }

    #[test]
    fn scrollback_survives_resize() {
        let mut t = Term::new(4, 2);
        feed(&mut t, "abcdef\r\ngh\r\nij\r\nkl");
        let before = t.scrollback_len();
        assert!(before > 0);
        t.resize(8, 3);
        assert_eq!(t.scrollback_len(), before);
        // Every history line must match the new width.
        for i in 0..t.scrollback_len() {
            assert_eq!(t.line(i).unwrap().len(), 8);
        }
    }

    #[test]
    fn bracketed_paste_mode_tracks_private_mode() {
        let mut t = Term::new(10, 2);
        feed(&mut t, "\x1b[?2004h");
        assert!(t.bracketed_paste);
        feed(&mut t, "\x1b[?2004l");
        assert!(!t.bracketed_paste);
    }

    #[test]
    fn mouse_reporting_modes() {
        let mut t = Term::new(10, 3);
        assert_eq!(t.mouse_mode(), MouseMode::Off);
        assert!(!t.mouse_sgr);

        feed(&mut t, "\x1b[?1000h\x1b[?1006h");
        assert_eq!(t.mouse_mode(), MouseMode::Click);
        assert!(t.mouse_sgr);

        feed(&mut t, "\x1b[?1002h");
        assert_eq!(t.mouse_mode(), MouseMode::Drag);

        feed(&mut t, "\x1b[?1003h");
        assert_eq!(t.mouse_mode(), MouseMode::Move);

        // Disabling the most permissive mode falls back to the next one.
        feed(&mut t, "\x1b[?1003l");
        assert_eq!(t.mouse_mode(), MouseMode::Drag);

        // Turning off a different mode must not disturb the active one.
        feed(&mut t, "\x1b[?1000l");
        assert_eq!(t.mouse_mode(), MouseMode::Drag);

        feed(&mut t, "\x1b[?1002l");
        assert_eq!(t.mouse_mode(), MouseMode::Off);
    }

    #[test]
    fn focus_reporting_and_reset() {
        let mut t = Term::new(10, 3);
        feed(&mut t, "\x1b[?1004h");
        assert!(t.focus_reporting);
        feed(&mut t, "\x1bc");
        assert!(!t.focus_reporting);
        assert!(!t.mouse_sgr);
        assert_eq!(t.mouse_mode(), MouseMode::Off);
    }

    #[test]
    fn colors() {
        let mut t = Term::new(8, 2);
        feed(&mut t, "\x1b[31mR\x1b[0mN");
        assert_eq!(t.cell(0, 0).fg, PALETTE[1]);
        assert_eq!(t.cell(1, 0).fg, DEFAULT_FG);
    }

    #[test]
    fn cursor_position() {
        let mut t = Term::new(10, 5);
        feed(&mut t, "\x1b[3;4HX");
        assert_eq!(t.cell(3, 2).ch, 'X');
    }

    #[test]
    fn erase_line() {
        let mut t = Term::new(10, 2);
        feed(&mut t, "abcdef\x1b[3D\x1b[K");
        assert_eq!(row_text(&t, 0), "abc");
    }

    #[test]
    fn base64_round_trip() {
        for text in [
            "",
            "f",
            "fo",
            "foo",
            "hello world",
            "日本語",
            "a".repeat(100).as_str(),
        ] {
            let encoded = base64_encode(text.as_bytes());
            assert_eq!(base64_decode(encoded.as_bytes()).unwrap(), text.as_bytes());
        }
        // Known vector: "hello" -> "aGVsbG8="
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_decode(b"aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode(b"!!!"), None);
        assert_eq!(base64_decode(b"abc"), None);
    }

    #[test]
    fn osc52_write_queues_clipboard_text() {
        let mut t = Term::new(10, 3);
        feed(&mut t, "\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(t.take_clipboard_writes(), vec!["hello".to_string()]);
        // Empty selection field also means the system clipboard.
        feed(&mut t, "\x1b]52;;d29ybGQ=\x07");
        assert_eq!(t.take_clipboard_writes(), vec!["world".to_string()]);
    }

    #[test]
    fn osc52_query_and_gates() {
        let mut t = Term::new(10, 3);
        feed(&mut t, "\x1b]52;c;?\x1b\\");
        assert!(t.take_clipboard_query());
        assert!(!t.take_clipboard_query());

        // Writes disabled: ignored.
        t.osc52_write = false;
        feed(&mut t, "\x1b]52;c;aGVsbG8=\x1b\\");
        assert!(t.take_clipboard_writes().is_empty());

        // Reads disabled: query ignored.
        t.osc52_read = false;
        feed(&mut t, "\x1b]52;c;?\x1b\\");
        assert!(!t.take_clipboard_query());
    }

    #[test]
    fn osc52_ignores_primary_selection_and_garbage() {
        let mut t = Term::new(10, 3);
        // 'p' is the primary selection, not the clipboard.
        feed(&mut t, "\x1b]52;p;aGVsbG8=\x1b\\");
        assert!(t.take_clipboard_writes().is_empty());
        // Malformed base64 is dropped, not panicked on.
        feed(&mut t, "\x1b]52;c;%%%\x1b\\");
        assert!(t.take_clipboard_writes().is_empty());
    }

    #[test]
    fn theme_repaints_default_cells_only() {
        let mut t = Term::new(8, 2);
        feed(&mut t, "ab\x1b[41m \x1b[0m");
        let new_fg = [0x01, 0x02, 0x03];
        let new_bg = [0x04, 0x05, 0x06];
        let mut palette = PALETTE;
        palette[1] = [0xaa, 0xbb, 0xcc];
        t.set_theme(new_fg, new_bg, palette);
        assert_eq!(t.foreground(), new_fg);
        assert_eq!(t.background(), new_bg);
        // Default text picks up the new foreground...
        assert_eq!(t.cell(0, 0).fg, new_fg);
        // ...new output uses it...
        feed(&mut t, "c");
        assert_eq!(t.cell(3, 0).fg, new_fg);
        // ...but explicitly colored cells keep their color...
        assert_eq!(t.cell(2, 0).bg, PALETTE[1]);
        // ...and SGR 31 now resolves through the themed palette.
        feed(&mut t, "\x1b[31mZ");
        assert_eq!(t.cell(4, 0).fg, [0xaa, 0xbb, 0xcc]);
        // Blank cells created after the theme switch use the new background.
        feed(&mut t, "\x1b[2J");
        assert_eq!(t.cell(0, 0).bg, new_bg);
    }

    #[test]
    fn scrollback_limit_is_configurable() {
        let mut t = Term::new(6, 2);
        t.set_scrollback_limit(2);
        feed(&mut t, "one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(line_text(&t, 0), "one");
        assert_eq!(line_text(&t, 1), "two");
        // Shrinking the limit trims existing history.
        t.set_scrollback_limit(1);
        assert_eq!(t.scrollback_len(), 1);
        assert_eq!(line_text(&t, 0), "two");
    }

    #[test]
    fn dsr_response() {
        let mut t = Term::new(10, 5);
        feed(&mut t, "\x1b[2;3H\x1b[6n");
        assert_eq!(t.take_responses(), vec![b"\x1b[2;3R".to_vec()]);
    }
}
