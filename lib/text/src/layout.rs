//! [`Layout`]: buffer lines broken into visual rows on a monospace grid
//! (soft wrap), with hit testing and vertical movement.
//!
//! Columns are display columns: every character is one cell wide except
//! tabs, which advance to the next tab stop.

use alloc::vec::Vec;

use crate::buffer::{Buffer, Pos};

/// Width in cells of `c` when it starts at display column `x`.
pub fn char_width(c: char, x: usize, tab: usize) -> usize {
    if c == '\t' { tab - x % tab } else { 1 }
}

/// Display column of byte offset `col` in `line`.
pub fn display_col(line: &str, col: usize, tab: usize) -> usize {
    let mut x = 0;
    for c in line[..col.min(line.len())].chars() {
        x += char_width(c, x, tab.max(1));
    }
    x
}

/// One visual row: the bytes `start..end` of buffer line `line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

/// The rows of a buffer for a given wrap width.
#[derive(Debug, Clone)]
pub struct Layout {
    rows: Vec<Row>,
    /// Index of the first row of each line.
    first_row: Vec<usize>,
    tab: usize,
    /// Widest row in cells.
    max_width: usize,
}

/// Splits one line into rows of at most `cols` cells, breaking after spaces
/// where possible. Spaces may hang past the edge rather than start a row.
fn wrap_line(index: usize, line: &str, cols: usize, tab: usize, rows: &mut Vec<Row>, max_width: &mut usize) {
    let mut start = 0;
    loop {
        let mut x = 0;
        let mut brk = None;
        let mut end = line.len();
        let mut width_at_brk = 0;
        for (off, c) in line[start..].char_indices() {
            let i = start + off;
            let w = char_width(c, x, tab);
            if x + w > cols && i > start && c != ' ' {
                end = brk.unwrap_or(i);
                x = if brk.is_some() { width_at_brk } else { x };
                break;
            }
            x += w;
            if c == ' ' || c == '\t' {
                brk = Some(i + c.len_utf8());
                width_at_brk = x;
            }
        }
        *max_width = (*max_width).max(x);
        rows.push(Row { line: index, start, end });
        if end >= line.len() {
            break;
        }
        start = end;
    }
}

impl Layout {
    /// Lays out `buf`, wrapping rows at `wrap` cells (`None` = no wrapping).
    pub fn new(buf: &Buffer, wrap: Option<usize>, tab: usize) -> Layout {
        let tab = tab.max(1);
        let mut rows = Vec::with_capacity(buf.line_count());
        let mut first_row = Vec::with_capacity(buf.line_count());
        let mut max_width = 0;
        for (i, line) in buf.lines().enumerate() {
            first_row.push(rows.len());
            match wrap {
                Some(cols) => wrap_line(i, line, cols.max(4), tab, &mut rows, &mut max_width),
                None => {
                    max_width = max_width.max(display_col(line, line.len(), tab));
                    rows.push(Row { line: i, start: 0, end: line.len() });
                }
            }
        }
        Layout { rows, first_row, tab, max_width }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn row(&self, i: usize) -> Row {
        self.rows[i.min(self.rows.len() - 1)]
    }

    /// Width of the widest row in cells.
    pub fn max_width(&self) -> usize {
        self.max_width
    }

    /// The first row of `line`.
    pub fn first_row_of_line(&self, line: usize) -> usize {
        self.first_row[line.min(self.first_row.len() - 1)]
    }

    /// The last row of `line`.
    fn last_row_of_line(&self, line: usize) -> usize {
        self.first_row.get(line + 1).copied().unwrap_or(self.rows.len()) - 1
    }

    fn is_last_row_of_line(&self, r: usize) -> bool {
        r + 1 >= self.rows.len() || self.rows[r + 1].line != self.rows[r].line
    }

    /// The row containing `p`. A position at a wrap point belongs to the
    /// row that starts there.
    pub fn row_of(&self, p: Pos) -> usize {
        let line = p.line.min(self.first_row.len() - 1);
        let (first, last) = (self.first_row[line], self.last_row_of_line(line));
        let mut r = first;
        while r < last && p.col >= self.rows[r].end {
            r += 1;
        }
        r
    }

    /// Display column of `p` within its row.
    pub fn x_of(&self, buf: &Buffer, p: Pos) -> usize {
        let row = self.rows[self.row_of(p)];
        let line = buf.line(row.line);
        let mut x = 0;
        for c in line[row.start..p.col.clamp(row.start, row.end)].chars() {
            x += char_width(c, x, self.tab);
        }
        x
    }

    /// The position closest to display column `x` in row `r`.
    pub fn pos_at(&self, buf: &Buffer, r: usize, x: usize) -> Pos {
        let r = r.min(self.rows.len() - 1);
        let row = self.rows[r];
        let line = buf.line(row.line);
        let mut cx = 0;
        for (off, c) in line[row.start..row.end].char_indices() {
            let w = char_width(c, cx, self.tab);
            if x < cx + w.div_ceil(2) {
                return Pos::new(row.line, row.start + off);
            }
            cx += w;
        }
        Pos::new(row.line, self.row_end_col(buf, r))
    }

    /// Where the cursor goes for "end of row": the line end on the last row
    /// of a line, otherwise before the row's final character (so the cursor
    /// stays on that row).
    fn row_end_col(&self, buf: &Buffer, r: usize) -> usize {
        let row = self.rows[r];
        if self.is_last_row_of_line(r) {
            row.end
        } else {
            let line = buf.line(row.line);
            line[row.start..row.end].char_indices().next_back().map_or(row.start, |(off, _)| row.start + off)
        }
    }

    /// Moves `delta` rows up (negative) or down from `p`, aiming for display
    /// column `goal`. Beyond the first/last row it goes to the start/end of
    /// the buffer.
    pub fn vertical(&self, buf: &Buffer, p: Pos, goal: usize, delta: isize) -> Pos {
        let r = self.row_of(p) as isize + delta;
        if r < 0 {
            Pos::new(0, 0)
        } else if r >= self.rows.len() as isize {
            buf.end()
        } else {
            self.pos_at(buf, r as usize, goal)
        }
    }

    /// Start of the row containing `p`.
    pub fn row_start(&self, p: Pos) -> Pos {
        let row = self.rows[self.row_of(p)];
        Pos::new(row.line, row.start)
    }

    /// End of the row containing `p`.
    pub fn row_end(&self, buf: &Buffer, p: Pos) -> Pos {
        let r = self.row_of(p);
        Pos::new(self.rows[r].line, self.row_end_col(buf, r))
    }

    /// Whether row `r` continues a wrapped line.
    pub fn is_continuation(&self, r: usize) -> bool {
        self.rows[r].start > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_wrap_rows_are_lines() {
        let b = Buffer::from_text("a\tb\n\nlonger line");
        let l = Layout::new(&b, None, 4);
        assert_eq!(l.row_count(), 3);
        assert_eq!(l.max_width(), 11);
        assert_eq!(l.x_of(&b, Pos::new(0, 2)), 4);
        assert_eq!(l.pos_at(&b, 0, 3), Pos::new(0, 2));
        assert_eq!(l.pos_at(&b, 0, 1), Pos::new(0, 1));
        assert_eq!(l.pos_at(&b, 2, 99), Pos::new(2, 11));
        assert_eq!(l.vertical(&b, Pos::new(0, 2), 4, 2), Pos::new(2, 4));
        assert_eq!(l.vertical(&b, Pos::new(0, 2), 4, -1), Pos::new(0, 0));
        assert_eq!(l.vertical(&b, Pos::new(2, 2), 4, 1), Pos::new(2, 11));
    }

    #[test]
    fn wraps_at_spaces() {
        let b = Buffer::from_text("the quick brown fox");
        let l = Layout::new(&b, Some(10), 4);
        let rows: Vec<&str> = (0..l.row_count()).map(|i| l.row(i)).map(|r| &b.line(0)[r.start..r.end]).collect();
        assert_eq!(rows, ["the quick ", "brown fox"]);
        // The wrap point belongs to the second row.
        assert_eq!(l.row_of(Pos::new(0, 10)), 1);
        assert_eq!(l.row_of(Pos::new(0, 9)), 0);
        assert_eq!(l.row_end(&b, Pos::new(0, 2)), Pos::new(0, 9));
        assert_eq!(l.row_start(Pos::new(0, 12)), Pos::new(0, 10));
        assert_eq!(l.vertical(&b, Pos::new(0, 2), 2, 1), Pos::new(0, 12));
        assert!(l.is_continuation(1));
    }

    #[test]
    fn hard_breaks_long_words() {
        let b = Buffer::from_text("abcdefghij");
        let l = Layout::new(&b, Some(4), 4);
        let rows: Vec<(usize, usize)> = (0..l.row_count()).map(|i| (l.row(i).start, l.row(i).end)).collect();
        assert_eq!(rows, [(0, 4), (4, 8), (8, 10)]);
        assert_eq!(l.pos_at(&b, 2, 9), Pos::new(0, 10));
        // Spaces hang instead of starting a row.
        let b = Buffer::from_text("abcd    efgh");
        let l = Layout::new(&b, Some(4), 4);
        assert_eq!(l.row(1).start, 8);
    }

    #[test]
    fn display_columns() {
        assert_eq!(display_col("\tx", 1, 4), 4);
        assert_eq!(display_col("ab\tx", 3, 4), 4);
        assert_eq!(display_col("é", 2, 4), 1);
    }
}
