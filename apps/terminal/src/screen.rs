//! The terminal's text buffer.
//!
//! Output is kept as logical lines of styled [`Cell`]s. Lines are
//! soft-wrapped into visual rows when drawn, so resizing the window re-flows
//! the text. [`Screen::write`] understands newlines, tabs and the ANSI
//! "select graphic rendition" escape sequences (`ESC [ ... m`) that set
//! colours and attributes, and the carriage return, backspace and "erase in
//! line" (`ESC [ K`) with which programs redraw the line they are on
//! (progress bars); other escape sequences (window titles, hyperlinks,
//! cursor movement, ...) are recognised and left out, so that none of their
//! bytes show. [`Screen::write_styled`] writes plain text in a given style.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

/// Maximum number of lines kept in the scrollback.
pub const MAX_LINES: usize = 10_000;

/// Tab stops are every `TAB_WIDTH` columns.
const TAB_WIDTH: usize = 8;

/// Indices into the 16-colour palette (the classic ANSI order).
#[allow(dead_code)]
pub mod color {
    pub const BLACK: u8 = 0;
    pub const RED: u8 = 1;
    pub const GREEN: u8 = 2;
    pub const YELLOW: u8 = 3;
    pub const BLUE: u8 = 4;
    pub const MAGENTA: u8 = 5;
    pub const CYAN: u8 = 6;
    pub const WHITE: u8 = 7;
    pub const GREY: u8 = 8;
    pub const BRIGHT_RED: u8 = 9;
    pub const BRIGHT_GREEN: u8 = 10;
    pub const BRIGHT_YELLOW: u8 = 11;
    pub const BRIGHT_BLUE: u8 = 12;
    pub const BRIGHT_MAGENTA: u8 = 13;
    pub const BRIGHT_CYAN: u8 = 14;
    pub const BRIGHT_WHITE: u8 = 15;
    /// The terminal's default foreground or background.
    pub const DEFAULT: u8 = 255;
}

/// How a cell is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: u8,
    pub bg: u8,
    pub bold: bool,
    pub dim: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Style {
    /// Default colours, no attributes.
    pub const PLAIN: Style =
        Style { fg: color::DEFAULT, bg: color::DEFAULT, bold: false, dim: false, underline: false, inverse: false };

    /// Plain style with foreground colour `fg`.
    pub const fn fg(fg: u8) -> Style {
        Style { fg, ..Style::PLAIN }
    }

    /// The same style in bold.
    pub const fn bold(self) -> Style {
        Style { bold: true, ..self }
    }

    /// Bold text in the default colour.
    pub const BOLD: Style = Style::PLAIN.bold();
    /// Faint grey text (secondary information).
    pub const DIM: Style = Style::fg(color::GREY);
    /// Error messages.
    pub const ERROR: Style = Style::fg(color::BRIGHT_RED);
}

/// One character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

/// A position in the buffer: an absolute line number and a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub line: u64,
    pub col: usize,
}

/// State of the escape-sequence parser (sequences as ECMA-48 forms them).
enum Esc {
    None,
    /// Saw ESC.
    Escape,
    /// Inside `ESC [`; collected parameter bytes.
    Csi(String),
    /// After ESC and an intermediate byte (`ESC ( B` chooses a character
    /// set): ignored up to the final byte.
    Intermediate,
    /// Inside a control string, ignored up to its end (`ESC \`, or BEL as
    /// xterm allows): `ESC ]` (an operating system command, such as a
    /// window title or the hyperlinks GCC puts in its messages), `ESC P`,
    /// `ESC _`, `ESC ^` or `ESC X`.
    Control {
        /// The last character was ESC.
        escape: bool,
    },
}

impl Esc {
    /// The state after ESC and `c`.
    fn after_escape(c: char) -> Esc {
        match c {
            '[' => Esc::Csi(String::new()),
            ']' | 'P' | '_' | '^' | 'X' => Esc::Control { escape: false },
            '\x20'..='\x2f' => Esc::Intermediate,
            '\x1b' => Esc::Escape,
            // Anything else completes a sequence of its own (`ESC 7`,
            // `ESC c`, ...), none of which this terminal acts on.
            _ => Esc::None,
        }
    }
}

/// The scrollback buffer.
pub struct Screen {
    /// Completed lines.
    lines: VecDeque<Vec<Cell>>,
    /// Absolute number of `lines[0]` (grows as old lines are dropped).
    base: u64,
    /// The line currently being written (not yet terminated).
    partial: Vec<Cell>,
    /// Where the next character goes in `partial`.
    col: usize,
    /// Current style of [`Screen::write`].
    style: Style,
    esc: Esc,
    /// Incremented on every change (for redraw decisions and caches).
    pub generation: u64,
}

impl Default for Screen {
    fn default() -> Self {
        Screen::new()
    }
}

impl Screen {
    pub fn new() -> Screen {
        Screen {
            lines: VecDeque::new(),
            base: 0,
            partial: Vec::new(),
            col: 0,
            style: Style::PLAIN,
            esc: Esc::None,
            generation: 0,
        }
    }

    /// Absolute number of the first stored line.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Absolute number one past the last completed line, i.e. the number of
    /// the line that is being written (or the input line).
    pub fn end(&self) -> u64 {
        self.base + self.lines.len() as u64
    }

    /// The completed line with absolute number `abs`.
    pub fn line(&self, abs: u64) -> Option<&[Cell]> {
        let i = abs.checked_sub(self.base)?;
        self.lines.get(i as usize).map(|l| l.as_slice())
    }

    /// Forgets everything (the `clear` command).
    pub fn clear(&mut self) {
        self.base += self.lines.len() as u64;
        self.lines.clear();
        self.partial.clear();
        self.col = 0;
        self.style = Style::PLAIN;
        self.esc = Esc::None;
        self.generation += 1;
    }

    fn push_line(&mut self, line: Vec<Cell>) {
        self.lines.push_back(line);
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.base += 1;
        }
    }

    /// Ends the current line.
    pub fn newline(&mut self) {
        self.col = 0;
        let line = core::mem::take(&mut self.partial);
        self.push_line(line);
        self.generation += 1;
    }

    /// Ends the current line if anything was written to it.
    pub fn finish_line(&mut self) {
        if !self.partial.is_empty() {
            self.newline();
        }
        self.style = Style::PLAIN;
        self.esc = Esc::None;
    }

    /// Appends a completed line of cells (e.g. the echoed prompt).
    pub fn push_cells(&mut self, cells: Vec<Cell>) {
        self.finish_line();
        self.push_line(cells);
        self.generation += 1;
    }

    /// The line being written, not yet ended (a prompt waiting for input).
    pub fn partial(&self) -> &[Cell] {
        &self.partial
    }

    /// Writes `c` at the cursor, over what is there.
    fn place(&mut self, c: Cell) {
        if self.col < self.partial.len() {
            self.partial[self.col] = c;
        } else {
            self.partial.resize(self.col, Cell { ch: ' ', style: Style::PLAIN });
            self.partial.push(c);
        }
        self.col += 1;
    }

    fn put(&mut self, c: char, style: Style) {
        match c {
            '\n' => self.newline(),
            '\r' => self.col = 0,
            '\x08' => self.col = self.col.saturating_sub(1),
            '\t' => {
                let n = TAB_WIDTH - self.col % TAB_WIDTH;
                for _ in 0..n {
                    self.place(Cell { ch: ' ', style });
                }
            }
            c if c.is_control() => {}
            c => self.place(Cell { ch: c, style }),
        }
    }

    /// `ESC [ n K`: erases the line after the cursor (0), before it (1) or
    /// all of it (2).
    fn erase_in_line(&mut self, params: &str) {
        match params {
            "" | "0" => self.partial.truncate(self.col),
            "1" => {
                let end = (self.col + 1).min(self.partial.len());
                for c in &mut self.partial[..end] {
                    *c = Cell { ch: ' ', style: Style::PLAIN };
                }
            }
            "2" => self.partial.clear(),
            _ => {}
        }
    }

    /// Writes plain text in `style` (escape sequences are not interpreted).
    pub fn write_styled(&mut self, s: &str, style: Style) {
        for c in s.chars() {
            if c != '\x1b' {
                self.put(c, style);
            }
        }
        self.generation += 1;
    }

    /// Writes text, interpreting `ESC [ ... m` colour sequences and
    /// `ESC [ K`; other escape sequences are recognised and left out.
    pub fn write(&mut self, s: &str) {
        for c in s.chars() {
            match &mut self.esc {
                Esc::None => {
                    if c == '\x1b' {
                        self.esc = Esc::Escape;
                    } else {
                        let style = self.style;
                        self.put(c, style);
                    }
                }
                Esc::Escape => self.esc = Esc::after_escape(c),
                Esc::Intermediate => {
                    if !('\x20'..='\x2f').contains(&c) {
                        self.esc = Esc::None;
                    }
                }
                Esc::Control { escape } => {
                    if *escape {
                        // ESC \ ends the string; any other ESC starts a new
                        // sequence.
                        self.esc = if c == '\\' { Esc::None } else { Esc::after_escape(c) };
                    } else if c == '\x07' {
                        self.esc = Esc::None;
                    } else if c == '\x1b' {
                        *escape = true;
                    }
                }
                // ESC starts a new sequence; CAN and SUB cancel this one.
                Esc::Csi(_) if c == '\x1b' => self.esc = Esc::Escape,
                Esc::Csi(_) if c == '\x18' || c == '\x1a' => self.esc = Esc::None,
                Esc::Csi(params) => {
                    if ('\x40'..='\x7e').contains(&c) {
                        let params = core::mem::take(params);
                        self.esc = Esc::None;
                        match c {
                            'm' => self.style = apply_sgr(self.style, &params),
                            'K' => self.erase_in_line(&params),
                            _ => {}
                        }
                    } else if params.len() < 64 {
                        params.push(c);
                    } else {
                        self.esc = Esc::None;
                    }
                }
            }
        }
        self.generation += 1;
    }

    /// The text between two positions (inclusive start, exclusive end),
    /// including `live`, the cells of the input line at [`Screen::end`].
    /// Trailing blanks of each line are dropped; lines are joined with `\n`.
    pub fn text_between(&self, start: Pos, end: Pos, live: &[Cell]) -> String {
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        let mut out = String::new();
        let first = start.line.max(self.base);
        let mut abs = first;
        while abs <= end.line {
            let cells: &[Cell] = if abs == self.end() { live } else { self.line(abs).unwrap_or(&[]) };
            let c0 = if abs == start.line { start.col.min(cells.len()) } else { 0 };
            let c1 = if abs == end.line { end.col.min(cells.len()) } else { cells.len() };
            let mut s: String = cells[c0..c1.max(c0)].iter().map(|c| c.ch).collect();
            if abs != end.line {
                let trimmed = s.trim_end().len();
                s.truncate(trimmed);
                out.push_str(&s);
                out.push('\n');
            } else {
                out.push_str(&s);
            }
            if abs >= self.end() {
                break;
            }
            abs += 1;
        }
        out
    }
}

/// Applies an SGR parameter string (e.g. `"1;31"`) to a style.
fn apply_sgr(mut s: Style, params: &str) -> Style {
    let nums: Vec<u32> = if params.is_empty() {
        alloc::vec![0]
    } else {
        params.split(';').map(|p| p.parse::<u32>().unwrap_or(0)).collect()
    };
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => s = Style::PLAIN,
            1 => s.bold = true,
            2 => s.dim = true,
            4 => s.underline = true,
            7 => s.inverse = true,
            22 => {
                s.bold = false;
                s.dim = false;
            }
            24 => s.underline = false,
            27 => s.inverse = false,
            n @ 30..=37 => s.fg = (n - 30) as u8,
            39 => s.fg = color::DEFAULT,
            n @ 40..=47 => s.bg = (n - 40) as u8,
            49 => s.bg = color::DEFAULT,
            n @ 90..=97 => s.fg = (n - 90 + 8) as u8,
            n @ 100..=107 => s.bg = (n - 100 + 8) as u8,
            // 256-colour form: 38;5;N (only the first 16 map exactly).
            n @ (38 | 48) if nums.get(i + 1) == Some(&5) => {
                if let Some(&c) = nums.get(i + 2) {
                    let c = if c < 16 { c as u8 } else { approx_256(c) };
                    if n == 38 {
                        s.fg = c;
                    } else {
                        s.bg = c;
                    }
                }
                i += 2;
            }
            _ => {}
        }
        i += 1;
    }
    s
}

/// Maps a colour of the xterm 256-colour cube onto the 16-colour palette.
fn approx_256(c: u32) -> u8 {
    if c >= 232 {
        return if c < 244 { color::GREY } else { color::WHITE };
    }
    let c = c - 16;
    let (r, g, b) = (c / 36, (c / 6) % 6, c % 6);
    let bright = r.max(g).max(b) >= 4;
    let bit = |v: u32| (v >= 2) as u8;
    let idx = bit(r) | bit(g) << 1 | bit(b) << 2;
    if bright { idx + 8 } else { idx }
}

/// Characters that end a "word" for double-click selection.
pub fn is_word_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '|' | ';' | ',' | '`')
}

/// The word around `col` in `cells`: `(start, end)` column range.
pub fn word_at(cells: &[Cell], col: usize) -> (usize, usize) {
    if cells.is_empty() {
        return (0, 0);
    }
    let col = col.min(cells.len() - 1);
    if !is_word_char(cells[col].ch) {
        return (col, col + 1);
    }
    let mut a = col;
    while a > 0 && is_word_char(cells[a - 1].ch) {
        a -= 1;
    }
    let mut b = col;
    while b < cells.len() && is_word_char(cells[b].ch) {
        b += 1;
    }
    (a, b)
}

/// Converts plain text to cells in one style.
pub fn cells(text: &str, style: Style) -> Vec<Cell> {
    text.chars().filter(|c| !c.is_control()).map(|ch| Cell { ch, style }).collect()
}
