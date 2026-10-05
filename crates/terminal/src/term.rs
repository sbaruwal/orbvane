//! Terminal emulator state: the screen grid, scrollback, cursor and modes, driven by the
//! escape sequences `vte` tokenizes. Implements the xterm subset shells and TUIs rely on.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    /// 0–15 are the ANSI colors, 16–255 the xterm 256-color palette.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub mod flags {
    pub const BOLD: u16 = 1;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const HIDDEN: u16 = 1 << 5;
    pub const STRIKE: u16 = 1 << 6;
    /// First half of a double-width character.
    pub const WIDE: u16 = 1 << 7;
    /// Second half of a double-width character (draws nothing).
    pub const SPACER: u16 = 1 << 8;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Self { ch: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// The line continues on the next row (soft wrap), so copying joins them.
    pub wrapped: bool,
}

impl Row {
    fn blank(cols: usize, bg: Color) -> Self {
        Self { cells: vec![Cell { bg, ..Cell::default() }; cols], wrapped: false }
    }

    pub fn text(&self) -> String {
        let s: String = self.cells.iter().filter(|c| c.flags & flags::SPACER == 0).map(|c| c.ch).collect();
        s.trim_end().to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    x: usize,
    y: usize,
    /// Attributes applied to printed characters.
    pen: Cell,
    /// DEC special graphics (line drawing) selected for G0.
    line_drawing: bool,
}

struct SavedScreen {
    lines: Vec<Row>,
    cursor: Cursor,
}

pub struct Term {
    cols: usize,
    rows: usize,
    lines: Vec<Row>,
    scrollback: VecDeque<Row>,
    max_scrollback: usize,
    /// The main screen while the alternate screen is active.
    main_screen: Option<SavedScreen>,
    cursor: Cursor,
    saved_cursor: Option<Cursor>,
    /// The cursor sits past the last column; the next printed char wraps first.
    wrap_pending: bool,
    scroll_top: usize,
    scroll_bottom: usize,
    tabs: Vec<bool>,
    last_char: char,
    pub autowrap: bool,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub app_cursor_keys: bool,
    pub bracketed_paste: bool,
    insert_mode: bool,
    origin_mode: bool,
    pub title: Option<String>,
    /// Bytes to send back to the program (replies to status queries).
    pub responses: Vec<u8>,
    /// Incremented on every change, so the UI knows when to redraw.
    pub version: u64,
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i > 0).collect()
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(2), rows.max(1));
        Self {
            cols,
            rows,
            lines: (0..rows).map(|_| Row::blank(cols, Color::Default)).collect(),
            scrollback: VecDeque::new(),
            max_scrollback: 10_000,
            main_screen: None,
            cursor: Cursor::default(),
            saved_cursor: None,
            wrap_pending: false,
            scroll_top: 0,
            scroll_bottom: rows,
            tabs: default_tabs(cols),
            last_char: ' ',
            autowrap: true,
            cursor_visible: true,
            cursor_shape: CursorShape::Block,
            app_cursor_keys: false,
            bracketed_paste: false,
            insert_mode: false,
            origin_mode: false,
            title: None,
            responses: Vec::new(),
            version: 0,
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor.x, self.cursor.y)
    }

    pub fn is_alt_screen(&self) -> bool {
        self.main_screen.is_some()
    }

    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.version += 1;
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// A line by absolute index: scrollback first, then the screen.
    pub fn line(&self, index: usize) -> Option<&Row> {
        if index < self.scrollback.len() {
            self.scrollback.get(index)
        } else {
            self.lines.get(index - self.scrollback.len())
        }
    }

    /// Screen row `row` when scrolled back `offset` lines.
    pub fn visible_row(&self, row: usize, offset: usize) -> Option<&Row> {
        let first = self.scrollback.len().saturating_sub(offset);
        self.line(first + row)
    }

    /// Text between two absolute (line, col) positions, joining soft-wrapped rows.
    pub fn text_between(&self, start: (usize, usize), end: (usize, usize)) -> String {
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        let mut out = String::new();
        for li in start.0..=end.0 {
            let Some(row) = self.line(li) else { break };
            let from = if li == start.0 { start.1 } else { 0 };
            let to = if li == end.0 { (end.1 + 1).min(row.cells.len()) } else { row.cells.len() };
            let part: String = row.cells.get(from..to.max(from)).unwrap_or(&[]).iter()
                .filter(|c| c.flags & flags::SPACER == 0)
                .map(|c| c.ch)
                .collect();
            if li < end.0 && !row.wrapped {
                out.push_str(part.trim_end());
                out.push('\n');
            } else {
                out.push_str(&part);
            }
        }
        out.trim_end_matches(' ').to_string()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        for row in self.lines.iter_mut().chain(self.scrollback.iter_mut()) {
            row.cells.resize(cols, Cell::default());
        }
        if let Some(main) = &mut self.main_screen {
            for row in &mut main.lines {
                row.cells.resize(cols, Cell::default());
            }
            main.lines.resize(rows, Row::blank(cols, Color::Default));
        }
        if rows < self.rows {
            // Keep the cursor on screen by pushing lines above it into scrollback.
            let overflow = (self.cursor.y + 1).saturating_sub(rows);
            for _ in 0..overflow {
                let row = self.lines.remove(0);
                if self.main_screen.is_none() {
                    self.push_scrollback(row);
                }
            }
            self.lines.truncate(rows);
            self.cursor.y -= overflow;
        } else {
            // Grow by pulling lines back from scrollback first (like xterm).
            let mut grow = rows - self.rows;
            while grow > 0 && self.main_screen.is_none() {
                let Some(row) = self.scrollback.pop_back() else { break };
                self.lines.insert(0, row);
                self.cursor.y += 1;
                grow -= 1;
            }
            self.lines.resize(rows, Row::blank(cols, Color::Default));
        }
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows;
        self.tabs = default_tabs(cols);
        self.cursor.x = self.cursor.x.min(cols - 1);
        self.cursor.y = self.cursor.y.min(rows - 1);
        self.wrap_pending = false;
        self.version += 1;
    }

    fn push_scrollback(&mut self, row: Row) {
        self.scrollback.push_back(row);
        if self.scrollback.len() > self.max_scrollback {
            self.scrollback.pop_front();
        }
    }

    fn blank(&self) -> Cell {
        Cell { bg: self.cursor.pen.bg, ..Cell::default() }
    }

    fn scroll_up(&mut self, n: usize) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        for _ in 0..n.min(bottom - top) {
            let row = self.lines.remove(top);
            if top == 0 && self.main_screen.is_none() {
                self.push_scrollback(row);
            }
            self.lines.insert(bottom - 1, Row::blank(self.cols, self.cursor.pen.bg));
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        for _ in 0..n.min(bottom - top) {
            self.lines.remove(bottom - 1);
            self.lines.insert(top, Row::blank(self.cols, self.cursor.pen.bg));
        }
    }

    fn linefeed(&mut self) {
        self.wrap_pending = false;
        if self.cursor.y + 1 == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.y + 1 < self.rows {
            self.cursor.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.y == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
        }
    }

    fn goto(&mut self, x: usize, y: usize) {
        let (min_y, max_y) = if self.origin_mode { (self.scroll_top, self.scroll_bottom - 1) } else { (0, self.rows - 1) };
        let y = if self.origin_mode { y + self.scroll_top } else { y };
        self.cursor.x = x.min(self.cols - 1);
        self.cursor.y = y.clamp(min_y, max_y);
        self.wrap_pending = false;
    }

    fn erase_cells(&mut self, y: usize, from: usize, to: usize) {
        let blank = self.blank();
        if let Some(row) = self.lines.get_mut(y) {
            for cell in row.cells.iter_mut().take(to.min(self.cols)).skip(from) {
                *cell = blank;
            }
        }
    }

    fn put_char(&mut self, c: char) {
        let c = if self.cursor.line_drawing { dec_line_drawing(c) } else { c };
        let width = c.width().unwrap_or(0);
        if width == 0 {
            return; // combining marks and controls: not rendered separately
        }
        if self.wrap_pending && self.autowrap {
            self.lines[self.cursor.y].wrapped = true;
            self.cursor.x = 0;
            self.linefeed();
        }
        self.wrap_pending = false;
        if width == 2 && self.cursor.x + 1 >= self.cols {
            // No room for a wide char at the end of the line.
            if self.autowrap {
                self.lines[self.cursor.y].wrapped = true;
                self.cursor.x = 0;
                self.linefeed();
            } else {
                return;
            }
        }
        let (x, y) = (self.cursor.x, self.cursor.y);
        if self.insert_mode {
            let row = &mut self.lines[y].cells;
            for _ in 0..width {
                row.insert(x, Cell::default());
                row.pop();
            }
        }
        let mut cell = self.cursor.pen;
        cell.ch = c;
        cell.flags &= !(flags::WIDE | flags::SPACER);
        if width == 2 {
            cell.flags |= flags::WIDE;
            self.lines[y].cells[x] = cell;
            self.lines[y].cells[x + 1] = Cell { ch: ' ', flags: flags::SPACER, ..cell };
        } else {
            self.lines[y].cells[x] = cell;
        }
        self.last_char = c;
        if x + width >= self.cols {
            self.cursor.x = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cursor.x = x + width;
        }
    }

    fn set_alt_screen(&mut self, on: bool, save_cursor: bool) {
        match (on, self.main_screen.is_some()) {
            (true, false) => {
                if save_cursor {
                    self.saved_cursor = Some(self.cursor);
                }
                let blank: Vec<Row> = (0..self.rows).map(|_| Row::blank(self.cols, Color::Default)).collect();
                let lines = std::mem::replace(&mut self.lines, blank);
                self.main_screen = Some(SavedScreen { lines, cursor: self.cursor });
            }
            (false, true) => {
                let main = self.main_screen.take().unwrap();
                self.lines = main.lines;
                self.cursor = main.cursor;
                if save_cursor {
                    if let Some(c) = self.saved_cursor {
                        self.cursor = c;
                    }
                }
            }
            _ => {}
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
        self.wrap_pending = false;
    }

    fn reset(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        let scrollback = std::mem::take(&mut self.scrollback);
        *self = Term::new(cols, rows);
        self.scrollback = scrollback;
    }

    fn sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        if groups.is_empty() {
            self.cursor.pen = Cell::default();
            return;
        }
        let pen = &mut self.cursor.pen;
        let mut i = 0;
        while i < groups.len() {
            let g = groups[i];
            match g[0] {
                0 => *pen = Cell::default(),
                1 => pen.flags |= flags::BOLD,
                2 => pen.flags |= flags::DIM,
                3 => pen.flags |= flags::ITALIC,
                4 => {
                    // 4:0 turns underline off; other styles render as a plain underline.
                    if g.get(1) == Some(&0) {
                        pen.flags &= !flags::UNDERLINE;
                    } else {
                        pen.flags |= flags::UNDERLINE;
                    }
                }
                7 => pen.flags |= flags::INVERSE,
                8 => pen.flags |= flags::HIDDEN,
                9 => pen.flags |= flags::STRIKE,
                21 | 22 => pen.flags &= !(flags::BOLD | flags::DIM),
                23 => pen.flags &= !flags::ITALIC,
                24 => pen.flags &= !flags::UNDERLINE,
                27 => pen.flags &= !flags::INVERSE,
                28 => pen.flags &= !flags::HIDDEN,
                29 => pen.flags &= !flags::STRIKE,
                n @ 30..=37 => pen.fg = Color::Indexed((n - 30) as u8),
                39 => pen.fg = Color::Default,
                n @ 40..=47 => pen.bg = Color::Indexed((n - 40) as u8),
                49 => pen.bg = Color::Default,
                n @ 90..=97 => pen.fg = Color::Indexed((n - 90 + 8) as u8),
                n @ 100..=107 => pen.bg = Color::Indexed((n - 100 + 8) as u8),
                38 | 48 => {
                    // Extended color, either as subparameters (38:2::r:g:b) or as
                    // separate parameters (38;2;r;g;b / 38;5;n).
                    let (color, used) = if g.len() > 1 {
                        (extended_color(&g[1..]), 0)
                    } else {
                        let rest: Vec<u16> = groups[i + 1..].iter().map(|p| p[0]).collect();
                        let used = match rest.first() {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => 0,
                        };
                        (extended_color(&rest), used)
                    };
                    if let Some(color) = color {
                        if g[0] == 38 {
                            pen.fg = color;
                        } else {
                            pen.bg = color;
                        }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn extended_color(p: &[u16]) -> Option<Color> {
    match p {
        [5, n, ..] => Some(Color::Indexed(*n as u8)),
        // Colon form may include an empty color-space id: 2::r:g:b parses as [2, 0, r, g, b].
        [2, _, r, g, b] => Some(Color::Rgb(*r as u8, *g as u8, *b as u8)),
        [2, r, g, b, ..] => Some(Color::Rgb(*r as u8, *g as u8, *b as u8)),
        _ => None,
    }
}

/// Maps ASCII to DEC special graphics (box drawing) for `ESC ( 0`.
fn dec_line_drawing(c: char) -> char {
    match c {
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'q' => '─',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'a' => '▒',
        '`' => '◆',
        'f' => '°',
        'g' => '±',
        '~' => '·',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        _ => c,
    }
}

/// First value of each parameter, with 0 (or missing) replaced by `default`.
fn arg(params: &Params, i: usize, default: usize) -> usize {
    match params.iter().nth(i).map(|p| p[0] as usize) {
        Some(0) | None => default,
        Some(n) => n,
    }
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.put_char(c);
        self.version += 1;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.cursor.x = self.cursor.x.saturating_sub(1);
                self.wrap_pending = false;
            }
            0x09 => {
                let next = (self.cursor.x + 1..self.cols).find(|&x| self.tabs[x]).unwrap_or(self.cols - 1);
                self.cursor.x = next;
            }
            0x0A..=0x0C => self.linefeed(),
            0x0D => {
                self.cursor.x = 0;
                self.wrap_pending = false;
            }
            _ => {}
        }
        self.version += 1;
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.version += 1;
        let private = intermediates.first() == Some(&b'?');
        let (x, y) = (self.cursor.x, self.cursor.y);
        match (action, intermediates) {
            ('A', []) => self.goto_raw(x, y.saturating_sub(arg(params, 0, 1)).max(if y >= self.scroll_top { self.scroll_top } else { 0 })),
            ('B', []) | ('e', []) => {
                let limit = if y < self.scroll_bottom { self.scroll_bottom - 1 } else { self.rows - 1 };
                self.goto_raw(x, (y + arg(params, 0, 1)).min(limit))
            }
            ('C', []) | ('a', []) => self.goto_raw((x + arg(params, 0, 1)).min(self.cols - 1), y),
            ('D', []) => self.goto_raw(x.saturating_sub(arg(params, 0, 1)), y),
            ('E', []) => self.goto_raw(0, (y + arg(params, 0, 1)).min(self.rows - 1)),
            ('F', []) => self.goto_raw(0, y.saturating_sub(arg(params, 0, 1))),
            ('G', []) | ('`', []) => self.goto_raw(arg(params, 0, 1) - 1, y),
            ('H', []) | ('f', []) => self.goto(arg(params, 1, 1) - 1, arg(params, 0, 1) - 1),
            ('d', []) => self.goto(x, arg(params, 0, 1) - 1),
            ('J', []) => {
                match arg(params, 0, 0) {
                    0 => {
                        self.erase_cells(y, x, self.cols);
                        for row in y + 1..self.rows {
                            self.erase_cells(row, 0, self.cols);
                        }
                    }
                    1 => {
                        for row in 0..y {
                            self.erase_cells(row, 0, self.cols);
                        }
                        self.erase_cells(y, 0, x + 1);
                    }
                    2 => {
                        for row in 0..self.rows {
                            self.erase_cells(row, 0, self.cols);
                        }
                    }
                    3 => self.scrollback.clear(),
                    _ => {}
                }
                if arg(params, 0, 0) != 3 {
                    self.wrap_pending = false;
                }
            }
            ('K', []) => {
                match arg(params, 0, 0) {
                    0 => self.erase_cells(y, x, self.cols),
                    1 => self.erase_cells(y, 0, x + 1),
                    2 => self.erase_cells(y, 0, self.cols),
                    _ => {}
                }
                self.lines[y].wrapped = false;
            }
            ('X', []) => self.erase_cells(y, x, x + arg(params, 0, 1)),
            ('@', []) => {
                let blank = self.blank();
                let row = &mut self.lines[y].cells;
                for _ in 0..arg(params, 0, 1).min(self.cols - x) {
                    row.insert(x, blank);
                    row.pop();
                }
            }
            ('P', []) => {
                let blank = self.blank();
                let row = &mut self.lines[y].cells;
                for _ in 0..arg(params, 0, 1).min(self.cols - x) {
                    row.remove(x);
                    row.push(blank);
                }
            }
            ('L', []) | ('M', []) if (self.scroll_top..self.scroll_bottom).contains(&y) => {
                let saved_top = self.scroll_top;
                self.scroll_top = y;
                if action == 'L' {
                    self.scroll_down(arg(params, 0, 1));
                } else {
                    self.scroll_up_no_scrollback(arg(params, 0, 1));
                }
                self.scroll_top = saved_top;
                self.cursor.x = 0;
            }
            ('S', []) => self.scroll_up_no_scrollback(arg(params, 0, 1)),
            ('T', []) => self.scroll_down(arg(params, 0, 1)),
            ('b', []) => {
                for _ in 0..arg(params, 0, 1).min(65535) {
                    self.put_char(self.last_char);
                }
            }
            ('m', []) => self.sgr(params),
            ('r', []) => {
                let top = arg(params, 0, 1) - 1;
                let bottom = arg(params, 1, self.rows).min(self.rows);
                if top + 1 < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.goto(0, 0);
                }
            }
            ('s', []) => self.saved_cursor = Some(self.cursor),
            ('u', []) => {
                if let Some(c) = self.saved_cursor {
                    self.cursor = c;
                    self.wrap_pending = false;
                }
            }
            ('n', []) => match arg(params, 0, 0) {
                5 => self.responses.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let reply = format!("\x1b[{};{}R", self.cursor.y + 1, self.cursor.x + 1);
                    self.responses.extend_from_slice(reply.as_bytes());
                }
                _ => {}
            },
            ('c', []) => self.responses.extend_from_slice(b"\x1b[?62;22c"),
            ('c', [b'>']) => self.responses.extend_from_slice(b"\x1b[>0;0;0c"),
            ('g', []) => match arg(params, 0, 0) {
                0 => {
                    if let Some(t) = self.tabs.get_mut(x) {
                        *t = false;
                    }
                }
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            ('Z', []) => {
                let prev = (0..x).rev().find(|&i| self.tabs[i]).unwrap_or(0);
                self.cursor.x = prev;
            }
            ('q', [b' ']) => {
                self.cursor_shape = match arg(params, 0, 1) {
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => CursorShape::Block,
                };
            }
            ('h', _) | ('l', _) => {
                let on = action == 'h';
                for p in params.iter() {
                    match (private, p[0]) {
                        (true, 1) => self.app_cursor_keys = on,
                        (true, 6) => {
                            self.origin_mode = on;
                            self.goto(0, 0);
                        }
                        (true, 7) => self.autowrap = on,
                        (true, 25) => self.cursor_visible = on,
                        (true, 47) | (true, 1047) => self.set_alt_screen(on, false),
                        (true, 1049) => {
                            if on {
                                self.set_alt_screen(true, true);
                            } else {
                                self.set_alt_screen(false, true);
                            }
                        }
                        (true, 2004) => self.bracketed_paste = on,
                        (false, 4) => self.insert_mode = on,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.version += 1;
        match (intermediates, byte) {
            ([], b'7') => self.saved_cursor = Some(self.cursor),
            ([], b'8') => {
                if let Some(c) = self.saved_cursor {
                    self.cursor = c;
                    self.wrap_pending = false;
                }
            }
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.cursor.x = 0;
                self.linefeed();
            }
            ([], b'M') => self.reverse_index(),
            ([], b'H') => {
                if let Some(t) = self.tabs.get_mut(self.cursor.x) {
                    *t = true;
                }
            }
            ([], b'c') => self.reset(),
            ([b'('], b'0') => self.cursor.line_drawing = true,
            ([b'('], _) => self.cursor.line_drawing = false,
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let [kind, title, ..] = params {
            if *kind == b"0" || *kind == b"2" {
                self.title = Some(String::from_utf8_lossy(title).to_string());
                self.version += 1;
            }
        }
    }
}

impl Term {
    /// Moves without origin-mode translation (relative cursor movement).
    fn goto_raw(&mut self, x: usize, y: usize) {
        self.cursor.x = x.min(self.cols - 1);
        self.cursor.y = y.min(self.rows - 1);
        self.wrap_pending = false;
    }

    /// Scrolls the region up without saving lines to scrollback (CSI S, delete line).
    fn scroll_up_no_scrollback(&mut self, n: usize) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        for _ in 0..n.min(bottom - top) {
            self.lines.remove(top);
            self.lines.insert(bottom - 1, Row::blank(self.cols, self.cursor.pen.bg));
        }
    }
}

/// A terminal state plus the parser feeding it.
pub struct Emulator {
    parser: vte::Parser,
    pub term: Term,
}

impl Emulator {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self { parser: vte::Parser::new(), term: Term::new(cols, rows) }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(e: &Emulator) -> Vec<String> {
        (0..e.term.rows()).map(|r| e.term.visible_row(r, 0).unwrap().text()).collect()
    }

    #[test]
    fn prints_and_wraps() {
        let mut e = Emulator::new(5, 3);
        e.feed(b"hello world");
        assert_eq!(screen(&e), vec!["hello", " worl", "d"]);
        assert!(e.term.visible_row(0, 0).unwrap().wrapped);
        assert_eq!(e.term.text_between((0, 0), (2, 4)), "hello world");
    }

    #[test]
    fn newline_scrolls_into_scrollback() {
        let mut e = Emulator::new(10, 2);
        e.feed(b"a\r\nb\r\nc");
        assert_eq!(screen(&e), vec!["b", "c"]);
        assert_eq!(e.term.scrollback_len(), 1);
        assert_eq!(e.term.visible_row(0, 1).unwrap().text(), "a");
    }

    #[test]
    fn cursor_movement_and_erase() {
        let mut e = Emulator::new(10, 3);
        e.feed(b"abcdef\x1b[1;3H\x1b[KX\x1b[2;1Hyz\x1b[1D\x1b[P");
        assert_eq!(screen(&e), vec!["abX", "y", ""]);
    }

    #[test]
    fn sgr_colors() {
        let mut e = Emulator::new(10, 1);
        e.feed(b"\x1b[1;31ma\x1b[38;5;208mb\x1b[38;2;1;2;3mc\x1b[48:2::9:8:7md\x1b[0me");
        let row = e.term.visible_row(0, 0).unwrap();
        assert_eq!(row.cells[0].fg, Color::Indexed(1));
        assert!(row.cells[0].flags & flags::BOLD != 0);
        assert_eq!(row.cells[1].fg, Color::Indexed(208));
        assert_eq!(row.cells[2].fg, Color::Rgb(1, 2, 3));
        assert_eq!(row.cells[3].bg, Color::Rgb(9, 8, 7));
        assert_eq!(row.cells[4].fg, Color::Default);
        assert_eq!(row.cells[4].flags, 0);
    }

    #[test]
    fn alternate_screen_restores_main() {
        let mut e = Emulator::new(10, 2);
        e.feed(b"shell$ \x1b[?1049h\x1b[2J\x1b[Hvim\x1b[?1049l");
        assert_eq!(screen(&e), vec!["shell$", ""]);
        assert_eq!(e.term.cursor(), (7, 0));
    }

    #[test]
    fn scroll_region_and_reverse_index() {
        let mut e = Emulator::new(10, 4);
        e.feed(b"1\r\n2\r\n3\r\n4\x1b[2;3r\x1b[3;1H\n");
        assert_eq!(screen(&e), vec!["1", "3", "", "4"]);
        e.feed(b"\x1b[2;1H\x1bM");
        assert_eq!(screen(&e), vec!["1", "", "3", "4"]);
    }

    #[test]
    fn wide_chars_and_line_drawing() {
        let mut e = Emulator::new(6, 2);
        e.feed("日本x".as_bytes());
        let row = e.term.visible_row(0, 0).unwrap();
        assert!(row.cells[0].flags & flags::WIDE != 0);
        assert!(row.cells[1].flags & flags::SPACER != 0);
        assert_eq!(row.text(), "日本x");
        e.feed(b"\r\n\x1b(0lqk\x1b(B");
        assert_eq!(e.term.visible_row(1, 0).unwrap().text(), "┌─┐");
    }

    #[test]
    fn replies_to_cursor_position_query() {
        let mut e = Emulator::new(10, 5);
        e.feed(b"\x1b[3;4H\x1b[6n");
        assert_eq!(e.term.responses, b"\x1b[3;4R");
    }

    #[test]
    fn resize_keeps_cursor_line_visible() {
        let mut e = Emulator::new(10, 4);
        e.feed(b"a\r\nb\r\nc\r\nd");
        e.term.resize(10, 2);
        assert_eq!(screen(&e), vec!["c", "d"]);
        e.term.resize(10, 4);
        assert_eq!(screen(&e), vec!["a", "b", "c", "d"]);
    }
}
