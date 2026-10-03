//! The anti-aliasing scan converter.
//!
//! [`Rasterizer`] computes the exact area of every pixel covered by a path, in the style of
//! FreeType's "gray" rasterizer:
//!
//! 1. The path is transformed to device space and its curves are flattened into lines (curves
//!    entirely outside the target are not flattened at all).
//! 2. Lines are clipped: parts above or below the target and parts right of it are dropped (they
//!    cannot influence visible pixels); parts left of it are projected onto `x = 0`, where they
//!    still contribute their winding to the whole row.
//! 3. Each line is walked through the pixel grid in 24.8 fixed point. For every pixel ("cell") it
//!    touches, `cover` accumulates the signed height of the line inside the cell and `area` twice
//!    the signed area between the line and the cell's left edge.
//! 4. Cells are bucketed by row (counting sort) and sorted by `x` within each row. Sweeping a row
//!    from left to right, the running sum of `cover` is the winding-weighted coverage of the pixels
//!    between cells, and a cell's own coverage is that sum minus the part left of its edges.
//! 5. Coverage is mapped through the fill rule and handed to the caller as [`Span`]s: runs of
//!    constant coverage (e.g. shape interiors) as [`Coverage::Solid`], everything else as
//!    per-pixel [`Coverage::Mask`] slices.
//!
//! The result is exact-area anti-aliasing (up to 1/256 pixel quantization) for any number of
//! overlapping contours with the non-zero or even-odd rule. All per-pixel work is integer
//! arithmetic; all buffers are kept between calls.

use alloc::vec::Vec;

use crate::flatten;
use crate::geom::Point;
use crate::path::{Path, PathEl};
use crate::stroke::{StrokeStyle, Stroker};
use crate::transform::Transform;

const SHIFT: i32 = 8;
const ONE: i32 = 1 << SHIFT;

/// Largest supported clip width or height in pixels; larger clips are clamped to it.
pub const MAX_DIMENSION: u32 = 32_000;

/// Device coordinates are clamped to `±COORD_LIMIT` (NaN becomes 0) before processing.
const COORD_LIMIT: f32 = 4.0e6;

/// Runs of constant non-zero coverage at least this long are reported as [`Coverage::Solid`].
const MIN_SOLID_RUN: i32 = 8;

/// Gaps of zero coverage up to this length inside a per-pixel run are filled with zeros instead of
/// splitting the run.
const MAX_ZERO_GAP: i32 = 4;

/// How overlapping contours combine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FillRule {
    /// A pixel is inside if the winding number is non-zero (fonts, most shapes).
    #[default]
    NonZero,
    /// A pixel is inside if the winding number is odd (holes from overlapping contours).
    EvenOdd,
}

/// Coverage values of a [`Span`]; 0 = outside, 255 = fully inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage<'a> {
    /// Every pixel of the span has this coverage (1..=255). `Solid(255)` is an opaque interior run.
    Solid(u8),
    /// One coverage value per pixel (`len` values; may contain zeros).
    Mask(&'a [u8]),
}

/// A horizontal run of pixels produced by the rasterizer.
///
/// Spans are emitted row by row from top to bottom and from left to right within a row; they never
/// overlap and always lie inside the clip rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span<'a> {
    /// Row.
    pub y: u32,
    /// First column.
    pub x: u32,
    /// Number of pixels.
    pub len: u32,
    /// Coverage of the pixels.
    pub coverage: Coverage<'a>,
}

impl Span<'_> {
    /// The coverage of pixel `x + i` (0 outside the span).
    #[inline]
    pub fn coverage_at(&self, i: u32) -> u8 {
        match self.coverage {
            Coverage::Solid(v) => {
                if i < self.len {
                    v
                } else {
                    0
                }
            }
            Coverage::Mask(m) => m.get(i as usize).copied().unwrap_or(0),
        }
    }
}

/// An 8-bit coverage mask (row-major, `width * height` bytes, 0..=255).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mask {
    /// Width in pixels (= row stride).
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Coverage values, `width * height` bytes.
    pub data: Vec<u8>,
}

impl Mask {
    /// Creates a zeroed mask.
    pub fn new(width: u32, height: u32) -> Self {
        let mut m = Mask { width, height, data: Vec::new() };
        m.clear();
        m
    }

    /// Zeroes the mask (and fixes up `data` to be exactly `width * height` bytes).
    pub fn clear(&mut self) {
        let len = self.width as usize * self.height as usize;
        self.data.clear();
        self.data.resize(len, 0);
    }

    /// Changes the size and zeroes the mask, reusing the allocation.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.clear();
    }

    /// The coverage at `(x, y)` (0 outside the mask).
    #[inline]
    pub fn get(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.data.get(y as usize * self.width as usize + x as usize).copied().unwrap_or(0)
    }

    /// One row of coverage values (empty if `y` is out of range).
    pub fn row(&self, y: u32) -> &[u8] {
        let w = self.width as usize;
        let start = y as usize * w;
        if y >= self.height {
            return &[];
        }
        self.data.get(start..start + w).unwrap_or(&[])
    }

    /// Writes a span into the mask (replacing the previous values; clipped to the mask).
    pub fn blit_span(&mut self, span: &Span<'_>) {
        if span.y >= self.height || span.x >= self.width {
            return;
        }
        let w = self.width as usize;
        let row = span.y as usize * w;
        let x0 = span.x as usize;
        match span.coverage {
            Coverage::Solid(v) => {
                let end = (x0 + span.len as usize).min(w);
                if let Some(s) = self.data.get_mut(row + x0..row + end) {
                    s.fill(v);
                }
            }
            Coverage::Mask(m) => {
                let len = m.len().min(w - x0);
                if let Some(s) = self.data.get_mut(row + x0..row + x0 + len) {
                    s.copy_from_slice(&m[..len]);
                }
            }
        }
    }
}

/// An accumulated pixel: `cover` is the summed signed height of edges inside it (256 = one pixel),
/// `area` twice the summed signed area between those edges and the pixel's left side.
#[derive(Clone, Copy, Default)]
struct Cell {
    x: i32,
    y: i32,
    cover: i32,
    area: i32,
}

/// A reusable anti-aliasing path rasterizer. See the [module documentation](self).
pub struct Rasterizer {
    cells: Vec<Cell>,
    sorted: Vec<Cell>,
    rows: Vec<u32>,
    batch: Vec<u8>,
    stroker: Stroker,
    cur_x: i32,
    cur_y: i32,
    cur_cover: i32,
    cur_area: i32,
    min_y: i32,
    max_y: i32,
    width: i32,
    height: i32,
    wf: f32,
    hf: f32,
    tolerance: f32,
}

impl Default for Rasterizer {
    fn default() -> Self {
        Rasterizer::new()
    }
}

impl Rasterizer {
    /// Creates a rasterizer. Its buffers grow as needed and are kept for later calls.
    pub fn new() -> Self {
        Rasterizer {
            cells: Vec::new(),
            sorted: Vec::new(),
            rows: Vec::new(),
            batch: Vec::new(),
            stroker: Stroker::new(),
            cur_x: i32::MIN,
            cur_y: i32::MIN,
            cur_cover: 0,
            cur_area: 0,
            min_y: i32::MAX,
            max_y: i32::MIN,
            width: 0,
            height: 0,
            wf: 0.0,
            hf: 0.0,
            tolerance: flatten::DEFAULT_TOLERANCE,
        }
    }

    /// Sets the curve flattening tolerance in device pixels (default 0.2). Ignored unless positive.
    pub fn set_tolerance(&mut self, tolerance: f32) {
        if tolerance > 0.0 && tolerance.is_finite() {
            self.tolerance = tolerance.max(1e-3);
        }
    }

    /// The curve flattening tolerance in device pixels.
    pub fn tolerance(&self) -> f32 {
        self.tolerance
    }

    /// Fills `path` (transformed by `transform`) with anti-aliasing, clipped to the rectangle
    /// `(0, 0)..clip`, and passes the resulting spans to `sink`. Contours are closed implicitly.
    pub fn fill<F: FnMut(Span<'_>)>(
        &mut self,
        path: &Path,
        transform: &Transform,
        rule: FillRule,
        clip: (u32, u32),
        sink: F,
    ) {
        self.reset(clip.0, clip.1);
        self.add_path(path, transform);
        self.sweep(rule, sink);
    }

    /// Fills `path` into `mask` (the mask is cleared first; its size is the clip rectangle).
    pub fn fill_mask(&mut self, path: &Path, transform: &Transform, rule: FillRule, mask: &mut Mask) {
        mask.clear();
        let clip = (mask.width, mask.height);
        self.fill(path, transform, rule, clip, |span| mask.blit_span(&span));
    }

    /// Fills `path` into a new `width x height` mask.
    pub fn render_mask(&mut self, path: &Path, transform: &Transform, rule: FillRule, width: u32, height: u32) -> Mask {
        let mut mask = Mask::new(width, height);
        self.fill_mask(path, transform, rule, &mut mask);
        mask
    }

    /// Strokes `path` with `style` (in path units, i.e. before `transform`) and fills the outline
    /// with the non-zero rule. The flattening tolerance is adapted to the transform's scale.
    pub fn stroke<F: FnMut(Span<'_>)>(
        &mut self,
        path: &Path,
        style: &StrokeStyle,
        transform: &Transform,
        clip: (u32, u32),
        sink: F,
    ) {
        let mut stroker = core::mem::take(&mut self.stroker);
        let scale = transform.max_scale();
        let tol = if scale > 1e-6 && scale.is_finite() { self.tolerance / scale } else { self.tolerance };
        let outline = stroker.stroke(path, style, tol);
        self.fill(outline, transform, FillRule::NonZero, clip, sink);
        self.stroker = stroker;
    }

    /// Strokes `path` into `mask` (cleared first); see [`Rasterizer::stroke`].
    pub fn stroke_mask(&mut self, path: &Path, style: &StrokeStyle, transform: &Transform, mask: &mut Mask) {
        mask.clear();
        let clip = (mask.width, mask.height);
        self.stroke(path, style, transform, clip, |span| mask.blit_span(&span));
    }

    // ----- Low-level API ---------------------------------------------------------------------

    /// Starts a new rendering: discards accumulated cells and sets the clip rectangle
    /// `(0, 0)..(width, height)` (each clamped to [`MAX_DIMENSION`]).
    pub fn reset(&mut self, width: u32, height: u32) {
        self.width = width.min(MAX_DIMENSION) as i32;
        self.height = height.min(MAX_DIMENSION) as i32;
        self.wf = self.width as f32;
        self.hf = self.height as f32;
        self.cells.clear();
        self.cur_x = i32::MIN;
        self.cur_y = i32::MIN;
        self.cur_cover = 0;
        self.cur_area = 0;
        self.min_y = i32::MAX;
        self.max_y = i32::MIN;
    }

    /// Accumulates the (implicitly closed) contours of `path`, transformed by `transform`.
    pub fn add_path(&mut self, path: &Path, transform: &Transform) {
        let t = *transform;
        let mut start = Point::ZERO;
        let mut last = Point::ZERO;
        let mut open = false;
        for el in path.iter() {
            match el {
                PathEl::MoveTo(p) => {
                    if open {
                        self.line(last, start);
                    }
                    let p = map(&t, p);
                    start = p;
                    last = p;
                    open = true;
                }
                PathEl::LineTo(p) => {
                    let p = map(&t, p);
                    self.line(last, p);
                    last = p;
                }
                PathEl::QuadTo(c, p) => {
                    let (c, p) = (map(&t, c), map(&t, p));
                    self.quad(last, c, p);
                    last = p;
                }
                PathEl::CubicTo(c1, c2, p) => {
                    let (c1, c2, p) = (map(&t, c1), map(&t, c2), map(&t, p));
                    self.cubic(last, c1, c2, p);
                    last = p;
                }
                PathEl::Close => {
                    if open {
                        self.line(last, start);
                        last = start;
                        open = false;
                    }
                }
            }
        }
        if open {
            self.line(last, start);
        }
    }

    /// Accumulates a single line in device coordinates. The caller is responsible for adding
    /// closed loops (unbalanced edges produce coverage up to the right clip edge).
    pub fn add_line(&mut self, p0: Point, p1: Point) {
        let id = Transform::IDENTITY;
        self.line(map(&id, p0), map(&id, p1));
    }

    /// Converts the accumulated cells into spans (passed to `sink`) and clears them.
    pub fn sweep<F: FnMut(Span<'_>)>(&mut self, rule: FillRule, mut sink: F) {
        self.flush_cell();
        self.cur_x = i32::MIN;
        self.cur_y = i32::MIN;
        let n = self.cells.len();
        if n == 0 {
            return;
        }
        let y0 = self.min_y;
        let rows = (self.max_y - y0 + 1) as usize;
        // Counting sort of the cells by row.
        self.rows.clear();
        self.rows.resize(rows, 0);
        for c in &self.cells {
            self.rows[(c.y - y0) as usize] += 1;
        }
        let mut sum = 0u32;
        for r in self.rows.iter_mut() {
            let count = *r;
            *r = sum;
            sum += count;
        }
        self.sorted.clear();
        self.sorted.resize(n, Cell::default());
        for c in &self.cells {
            let slot = &mut self.rows[(c.y - y0) as usize];
            self.sorted[*slot as usize] = *c;
            *slot += 1;
        }
        // `rows[r]` is now the end of row r.
        let width = self.width;
        let mut start = 0usize;
        for r in 0..rows {
            let end = self.rows[r] as usize;
            if end > start {
                let row = &mut self.sorted[start..end];
                sort_cells_by_x(row);
                sweep_row((y0 + r as i32) as u32, row, &mut self.batch, width, rule, &mut sink);
            }
            start = end;
        }
        self.cells.clear();
        self.min_y = i32::MAX;
        self.max_y = i32::MIN;
    }

    // ----- Edge processing -------------------------------------------------------------------

    fn quad(&mut self, p0: Point, p1: Point, p2: Point) {
        if p0.y.max(p1.y).max(p2.y) <= 0.0 || p0.y.min(p1.y).min(p2.y) >= self.hf {
            return;
        }
        if p0.x.min(p1.x).min(p2.x) >= self.wf {
            return;
        }
        if p0.x.max(p1.x).max(p2.x) <= 0.0 {
            // Entirely left of the clip: only the vertical extent matters.
            self.line(p0, p2);
            return;
        }
        let tol = self.tolerance;
        let mut prev = p0;
        flatten::flatten_quad(p0, p1, p2, tol, &mut |p| {
            self.line(prev, p);
            prev = p;
        });
    }

    fn cubic(&mut self, p0: Point, p1: Point, p2: Point, p3: Point) {
        if p0.y.max(p1.y).max(p2.y).max(p3.y) <= 0.0 || p0.y.min(p1.y).min(p2.y).min(p3.y) >= self.hf {
            return;
        }
        if p0.x.min(p1.x).min(p2.x).min(p3.x) >= self.wf {
            return;
        }
        if p0.x.max(p1.x).max(p2.x).max(p3.x) <= 0.0 {
            self.line(p0, p3);
            return;
        }
        let tol = self.tolerance;
        let mut prev = p0;
        flatten::flatten_cubic(p0, p1, p2, p3, tol, &mut |p| {
            self.line(prev, p);
            prev = p;
        });
    }

    /// Clips a device-space line to the target and accumulates it.
    fn line(&mut self, a: Point, b: Point) {
        let (mut x0, mut y0, mut x1, mut y1) = (a.x, a.y, b.x, b.y);
        if y0 == y1 {
            return; // horizontal lines never contribute
        }
        let (w, h) = (self.wf, self.hf);
        if x0 >= 0.0 && x1 >= 0.0 && x0 <= w && x1 <= w && y0 >= 0.0 && y1 >= 0.0 && y0 <= h && y1 <= h {
            self.line_fixed(fix(x0, w), fix(y0, h), fix(x1, w), fix(y1, h));
            return;
        }
        if (y0 <= 0.0 && y1 <= 0.0) || (y0 >= h && y1 >= h) {
            return;
        }
        // Vertical clipping.
        let dxdy = (x1 - x0) / (y1 - y0);
        if y0 < 0.0 {
            x0 -= y0 * dxdy;
            y0 = 0.0;
        } else if y0 > h {
            x0 += (h - y0) * dxdy;
            y0 = h;
        }
        if y1 < 0.0 {
            x1 -= y1 * dxdy;
            y1 = 0.0;
        } else if y1 > h {
            x1 += (h - y1) * dxdy;
            y1 = h;
        }
        if x0 >= w && x1 >= w {
            return;
        }
        if x0 <= 0.0 && x1 <= 0.0 {
            self.line_fixed(0, fix(y0, h), 0, fix(y1, h));
            return;
        }
        // Split at x = 0 and x = w: parts left of 0 are projected onto x = 0, parts right of w
        // are dropped.
        let mut cuts = [(0.0f32, 0.0f32); 2];
        let mut nc = 0;
        let dx = x1 - x0;
        if (x0 < 0.0) != (x1 < 0.0) {
            cuts[nc] = (-x0 / dx, 0.0);
            nc += 1;
        }
        if (x0 > w) != (x1 > w) {
            cuts[nc] = ((w - x0) / dx, w);
            nc += 1;
        }
        if nc == 2 && cuts[1].0 < cuts[0].0 {
            cuts.swap(0, 1);
        }
        let mut pts = [(0.0f32, 0.0f32); 4];
        pts[0] = (x0, y0);
        for (i, &(t, x)) in cuts[..nc].iter().enumerate() {
            pts[i + 1] = (x, y0 + (y1 - y0) * t);
        }
        pts[nc + 1] = (x1, y1);
        for i in 0..=nc {
            let (pa, pb) = (pts[i], pts[i + 1]);
            let mx = (pa.0 + pb.0) * 0.5;
            if mx < 0.0 {
                self.line_fixed(0, fix(pa.1, h), 0, fix(pb.1, h));
            } else if mx <= w {
                self.line_fixed(fix(pa.0, w), fix(pa.1, h), fix(pb.0, w), fix(pb.1, h));
            }
        }
    }

    /// Accumulates a line given in 24.8 fixed point, already clipped to the target.
    fn line_fixed(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        if y0 == y1 {
            return;
        }
        let (xa, ya, xb, yb, sign) = if y0 < y1 { (x0, y0, x1, y1, 1) } else { (x1, y1, x0, y0, -1) };
        let row0 = ya >> SHIFT;
        let row1 = (yb - 1) >> SHIFT;
        if row0 == row1 {
            let top = row0 << SHIFT;
            self.row_piece(row0, xa, ya - top, xb, yb - top, sign);
            return;
        }
        if xa == xb {
            // Vertical line: one cell per row.
            let col = (xa >> SHIFT).min(self.width);
            let fx2 = 2 * (xa - (col << SHIFT));
            let mut fy = ya - (row0 << SHIFT);
            for row in row0..=row1 {
                let fy_end = if row == row1 { yb - (row1 << SHIFT) } else { ONE };
                let c = sign * (fy_end - fy);
                self.add(col, row, c, c * fx2);
                fy = 0;
            }
            return;
        }
        let dx = (xb - xa) as i64;
        let dy = (yb - ya) as i64;
        let mut x_prev = xa;
        let mut fy_prev = ya - (row0 << SHIFT);
        for row in row0..row1 {
            let boundary = (row + 1) << SHIFT;
            let x_next = xa + div_round(dx * (boundary - ya) as i64, dy) as i32;
            self.row_piece(row, x_prev, fy_prev, x_next, ONE, sign);
            x_prev = x_next;
            fy_prev = 0;
        }
        self.row_piece(row1, x_prev, fy_prev, xb, yb - (row1 << SHIFT), sign);
    }

    /// Accumulates the part of a line inside one pixel row. `fys < fye` are offsets from the row's
    /// top edge in `0..=ONE`.
    #[inline]
    fn row_piece(&mut self, row: i32, xs: i32, fys: i32, xe: i32, fye: i32, sign: i32) {
        let dyp = fye - fys;
        if dyp == 0 {
            return;
        }
        let (xl, xr) = if xs <= xe { (xs, xe) } else { (xe, xs) };
        let col = xl >> SHIFT;
        let left = col << SHIFT;
        if xr <= left + ONE {
            // Inside a single column (possibly touching its right edge).
            let c = sign * dyp;
            self.add(col, row, c, c * ((xs - left) + (xe - left)));
            return;
        }
        let dyp64 = dyp as i64;
        if xs < xe {
            let dxp = (xe - xs) as i64;
            let mut c = xs >> SHIFT;
            let mut x_cur = xs;
            let mut fy_cur = fys;
            loop {
                let cl = c << SHIFT;
                let cr = cl + ONE;
                if cr >= xe {
                    let d = sign * (fye - fy_cur);
                    self.add(c, row, d, d * ((x_cur - cl) + (xe - cl)));
                    break;
                }
                let fy_b = fys + ((cr - xs) as i64 * dyp64 / dxp) as i32;
                let d = sign * (fy_b - fy_cur);
                self.add(c, row, d, d * ((x_cur - cl) + ONE));
                x_cur = cr;
                fy_cur = fy_b;
                c += 1;
            }
        } else {
            let dxp = (xs - xe) as i64;
            let mut c = (xs - 1) >> SHIFT;
            let mut x_cur = xs;
            let mut fy_cur = fys;
            loop {
                let cl = c << SHIFT;
                if cl <= xe {
                    let d = sign * (fye - fy_cur);
                    self.add(c, row, d, d * ((x_cur - cl) + (xe - cl)));
                    break;
                }
                let fy_b = fys + ((xs - cl) as i64 * dyp64 / dxp) as i32;
                let d = sign * (fy_b - fy_cur);
                self.add(c, row, d, d * (x_cur - cl));
                x_cur = cl;
                fy_cur = fy_b;
                c -= 1;
            }
        }
    }

    #[inline]
    fn add(&mut self, x: i32, y: i32, cover: i32, area: i32) {
        if x != self.cur_x || y != self.cur_y {
            self.flush_cell();
            self.cur_x = x;
            self.cur_y = y;
        }
        self.cur_cover = self.cur_cover.wrapping_add(cover);
        self.cur_area = self.cur_area.wrapping_add(area);
    }

    #[inline]
    fn flush_cell(&mut self) {
        if self.cur_cover != 0 || self.cur_area != 0 {
            self.cells.push(Cell { x: self.cur_x, y: self.cur_y, cover: self.cur_cover, area: self.cur_area });
            self.min_y = self.min_y.min(self.cur_y);
            self.max_y = self.max_y.max(self.cur_y);
            self.cur_cover = 0;
            self.cur_area = 0;
        }
    }
}

/// Transforms a point and clamps it to the supported coordinate range.
#[inline]
fn map(t: &Transform, p: Point) -> Point {
    let q = t.apply(p);
    Point::new(clamp_coord(q.x), clamp_coord(q.y))
}

#[inline]
fn clamp_coord(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(-COORD_LIMIT, COORD_LIMIT) }
}

/// Converts a clipped coordinate to 24.8 fixed point (rounded).
#[inline]
fn fix(v: f32, max: f32) -> i32 {
    (v.clamp(0.0, max) * ONE as f32 + 0.5) as i32
}

/// `num / den` rounded to nearest (`den > 0`).
#[inline]
fn div_round(num: i64, den: i64) -> i64 {
    if num >= 0 { (num + den / 2) / den } else { -((-num + den / 2) / den) }
}

fn sort_cells_by_x(cells: &mut [Cell]) {
    if cells.len() <= 24 {
        for i in 1..cells.len() {
            let c = cells[i];
            let mut j = i;
            while j > 0 && cells[j - 1].x > c.x {
                cells[j] = cells[j - 1];
                j -= 1;
            }
            cells[j] = c;
        }
    } else {
        cells.sort_unstable_by_key(|c| c.x);
    }
}

/// Maps coverage in 1/256 units (256 = one full winding) to 0..=255 through the fill rule.
#[inline]
fn alpha(c: u64, rule: FillRule) -> u8 {
    let c = match rule {
        FillRule::NonZero => c.min(256),
        FillRule::EvenOdd => {
            let m = c & 511;
            if m > 256 { 512 - m } else { m }
        }
    };
    ((c * 255 + 128) >> 8) as u8
}

#[inline]
fn span_alpha(acc: i32, rule: FillRule) -> u8 {
    alpha((acc as i64).unsigned_abs(), rule)
}

#[inline]
fn cell_alpha(acc: i32, area: i32, rule: FillRule) -> u8 {
    let v = ((acc as i64) << (SHIFT + 1)) - area as i64;
    alpha((v.unsigned_abs() + (1 << SHIFT)) >> (SHIFT + 1), rule)
}

fn sweep_row<F: FnMut(Span<'_>)>(y: u32, cells: &[Cell], batch: &mut Vec<u8>, width: i32, rule: FillRule, sink: &mut F) {
    batch.clear();
    let mut out = Batcher { y, buf: batch, start: 0, sink };
    let mut acc: i32 = 0;
    let mut x: i32 = 0;
    let n = cells.len();
    let mut i = 0;
    while i < n {
        let cx = cells[i].x;
        let mut cover = cells[i].cover;
        let mut area = cells[i].area;
        i += 1;
        while i < n && cells[i].x == cx {
            cover = cover.wrapping_add(cells[i].cover);
            area = area.wrapping_add(cells[i].area);
            i += 1;
        }
        if cx >= width {
            break;
        }
        if cx > x {
            out.run(x, cx - x, span_alpha(acc, rule));
        }
        acc = acc.wrapping_add(cover);
        out.pixel(cx, cell_alpha(acc, area, rule));
        x = cx + 1;
    }
    if x < width && acc != 0 {
        out.run(x, width - x, span_alpha(acc, rule));
    }
    out.flush();
}

/// Collects per-pixel values into runs and forwards spans to the sink.
struct Batcher<'b, F> {
    y: u32,
    buf: &'b mut Vec<u8>,
    start: i32,
    sink: &'b mut F,
}

impl<F: FnMut(Span<'_>)> Batcher<'_, F> {
    #[inline]
    fn run(&mut self, x: i32, len: i32, v: u8) {
        if v == 0 {
            if !self.buf.is_empty() {
                if len <= MAX_ZERO_GAP {
                    self.buf.resize(self.buf.len() + len as usize, 0);
                } else {
                    self.flush();
                }
            }
            return;
        }
        if len >= MIN_SOLID_RUN {
            self.flush();
            (self.sink)(Span { y: self.y, x: x as u32, len: len as u32, coverage: Coverage::Solid(v) });
            return;
        }
        if self.buf.is_empty() {
            self.start = x;
        }
        self.buf.resize(self.buf.len() + len as usize, v);
    }

    #[inline]
    fn pixel(&mut self, x: i32, v: u8) {
        if self.buf.is_empty() {
            if v == 0 {
                return;
            }
            self.start = x;
        }
        self.buf.push(v);
    }

    fn flush(&mut self) {
        while self.buf.last() == Some(&0) {
            self.buf.pop();
        }
        if !self.buf.is_empty() {
            (self.sink)(Span {
                y: self.y,
                x: self.start as u32,
                len: self.buf.len() as u32,
                coverage: Coverage::Mask(self.buf),
            });
            self.buf.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math;
    use alloc::vec;

    fn total(mask: &Mask) -> f64 {
        mask.data.iter().map(|&v| v as f64 / 255.0).sum()
    }

    fn rel_err(a: f64, b: f64) -> f64 {
        ((a - b) / b).abs()
    }

    fn fill(path: &Path, t: &Transform, rule: FillRule, w: u32, h: u32) -> Mask {
        Rasterizer::new().render_mask(path, t, rule, w, h)
    }

    #[test]
    fn axis_aligned_rect_exact() {
        let mut p = Path::new();
        p.rect(10.25, 20.5, 30.0, 15.75);
        let m = fill(&p, &Transform::IDENTITY, FillRule::NonZero, 64, 64);
        assert!(rel_err(total(&m), 30.0 * 15.75) < 0.002, "area {}", total(&m));
        assert_eq!(m.get(20, 30), 255);
        assert_eq!(m.get(5, 5), 0);
        assert_eq!(m.get(41, 30), 0);
        // Left column is 75% covered, top row 50%, bottom row (y=36) 25%.
        assert!((m.get(10, 30) as i32 - 191).abs() <= 1, "{}", m.get(10, 30));
        assert!((m.get(20, 20) as i32 - 128).abs() <= 1, "{}", m.get(20, 20));
        assert!((m.get(20, 36) as i32 - 64).abs() <= 1, "{}", m.get(20, 36));
        // Corner pixel: 0.75 * 0.5.
        assert!((m.get(10, 20) as i32 - 96).abs() <= 1, "{}", m.get(10, 20));
        // Half-pixel square.
        let mut q = Path::new();
        q.rect(0.5, 0.0, 1.0, 1.0);
        let m = fill(&q, &Transform::IDENTITY, FillRule::NonZero, 4, 1);
        assert_eq!(&m.data[..], &[128, 128, 0, 0]);
    }

    #[test]
    fn rotated_squares() {
        for i in 0..24 {
            let angle = i as f32 * 0.27;
            let mut p = Path::new();
            p.rect(-10.0, -10.0, 20.0, 20.0);
            let t = Transform::rotate(angle).then_translate(32.3, 31.7);
            let m = fill(&p, &t, FillRule::NonZero, 64, 64);
            assert!(rel_err(total(&m), 400.0) < 0.003, "angle {angle}: {}", total(&m));
            assert_eq!(m.get(32, 32), 255);
        }
        // Thin rotated sliver: the area must still be exact.
        let mut p = Path::new();
        p.rect(0.0, 0.0, 50.0, 0.3);
        let t = Transform::rotate(0.4).then_translate(5.0, 5.0);
        let m = fill(&p, &t, FillRule::NonZero, 64, 64);
        assert!(rel_err(total(&m), 15.0) < 0.01, "{}", total(&m));
    }

    #[test]
    fn circles() {
        for &r in &[1.5f32, 4.0, 10.25, 30.0] {
            let mut p = Path::new();
            p.circle(32.4, 32.2, r);
            let expected = core::f64::consts::PI * (r as f64) * (r as f64);
            let perimeter = 2.0 * core::f64::consts::PI * r as f64;
            // Flattening places the polyline inside the curve, at most `tolerance` away: the area
            // may be smaller by up to ~2/3 * perimeter * tolerance, never noticeably larger.
            for &tol in &[0.2f32, 0.05, 0.01] {
                let mut ras = Rasterizer::new();
                ras.set_tolerance(tol);
                let a = total(&ras.render_mask(&p, &Transform::IDENTITY, FillRule::NonZero, 64, 64));
                let max_loss = perimeter * tol as f64 * 0.67 + expected * 0.003;
                assert!(a <= expected * 1.003 && expected - a <= max_loss, "r {r} tol {tol}: {a} vs {expected}");
            }
        }
    }

    #[test]
    fn fill_rules_on_overlaps() {
        let mut p = Path::new();
        p.rect(10.0, 10.0, 30.0, 30.0);
        p.rect(25.0, 25.0, 30.0, 30.0);
        let nz = fill(&p, &Transform::IDENTITY, FillRule::NonZero, 64, 64);
        let eo = fill(&p, &Transform::IDENTITY, FillRule::EvenOdd, 64, 64);
        assert!(rel_err(total(&nz), 1575.0) < 0.001);
        assert!(rel_err(total(&eo), 1350.0) < 0.001);
        assert_eq!(nz.get(30, 30), 255);
        assert_eq!(eo.get(30, 30), 0);
        // Opposite orientation: the overlap cancels under the non-zero rule too.
        let mut q = Path::new();
        q.rect(10.0, 10.0, 30.0, 30.0);
        q.polygon(&[Point::new(25.0, 25.0), Point::new(25.0, 55.0), Point::new(55.0, 55.0), Point::new(55.0, 25.0)]);
        let nz = fill(&q, &Transform::IDENTITY, FillRule::NonZero, 64, 64);
        assert_eq!(nz.get(30, 30), 0);
        // Overlapping glyph-like contours (a plus sign from two bars, a stem through a bowl):
        // the union is filled without holes or seams under the non-zero rule.
        let mut plus = Path::new();
        plus.rect(5.3, 17.6, 30.0, 6.2);
        plus.rect(17.4, 4.9, 5.8, 30.0);
        let m = fill(&plus, &Transform::IDENTITY, FillRule::NonZero, 40, 40);
        let union = 30.0 * 6.2 + 5.8 * 30.0 - 6.2 * 5.8;
        assert!(rel_err(total(&m), union) < 0.003, "{} vs {union}", total(&m));
        assert_eq!(m.get(20, 20), 255);
        assert_eq!(m.get(18, 20), 255);
        let mut bowl = Path::new();
        bowl.circle(20.0, 20.0, 10.0);
        bowl.rect(8.0, 2.0, 4.0, 36.0); // stem overlapping the bowl's left side
        let m = fill(&bowl, &Transform::IDENTITY, FillRule::NonZero, 40, 40);
        for y in 3..37 {
            assert_eq!(m.get(9, y), 255, "stem pixel at y {y}");
        }
        assert_eq!(m.get(14, 20), 255);
        // An identical contour drawn twice keeps a solid interior (edge pixels where coincident
        // edges overlap can become slightly heavier, as with FreeType's area accumulation).
        let mut a = Path::new();
        a.circle(20.0, 20.0, 9.3);
        let once = fill(&a, &Transform::IDENTITY, FillRule::NonZero, 40, 40);
        let a2 = {
            let mut b = a.clone();
            b.append(&a);
            b
        };
        let twice = fill(&a2, &Transform::IDENTITY, FillRule::NonZero, 40, 40);
        for (o, t) in once.data.iter().zip(&twice.data) {
            assert!(*t >= *o && (*o != 255 || *t == 255));
        }
    }

    #[test]
    fn star_even_odd_has_hole() {
        let mut p = Path::new();
        let pts: Vec<Point> = (0..5)
            .map(|i| {
                let a = -core::f32::consts::FRAC_PI_2 + i as f32 * 4.0 * core::f32::consts::PI / 5.0;
                let (s, c) = math::sin_cos(a);
                Point::new(50.0 + 40.0 * c, 50.0 + 40.0 * s)
            })
            .collect();
        p.polygon(&pts);
        let nz = fill(&p, &Transform::IDENTITY, FillRule::NonZero, 100, 100);
        let eo = fill(&p, &Transform::IDENTITY, FillRule::EvenOdd, 100, 100);
        assert_eq!(nz.get(50, 50), 255);
        assert_eq!(eo.get(50, 50), 0);
        assert_eq!(eo.get(50, 20), 255);
        assert!(total(&nz) > total(&eo));
    }

    /// Renders `path` into a large reference mask and into a small mask shifted so that the shape
    /// crosses every border; the overlapping pixels must agree.
    #[test]
    fn clipping_matches_unclipped_reference() {
        let mut p = Path::new();
        p.circle(0.0, 0.0, 30.0);
        p.rect(-35.0, -5.0, 70.0, 10.0);
        p.cubic_to(80.0, -60.0, -70.0, 90.0, 10.0, 40.0);
        let reference = fill(&p, &Transform::translate(100.0, 100.0), FillRule::EvenOdd, 200, 200);
        let mut r = Rasterizer::new();
        // Integral offsets so that small-mask pixels map exactly onto reference pixels.
        for &(ox, oy) in &[(-5i32, -7i32), (40, 3), (10, 45), (-20, 30), (12, 12), (31, 31), (-29, -29)] {
            let m = r.render_mask(&p, &Transform::translate(ox as f32, oy as f32), FillRule::EvenOdd, 32, 32);
            for y in 0..32i32 {
                for x in 0..32i32 {
                    let (rx, ry) = (x + 100 - ox, y + 100 - oy);
                    let expect = reference.get(rx as u32, ry as u32);
                    let got = m.get(x as u32, y as u32);
                    assert!((expect as i32 - got as i32).abs() <= 1, "offset ({ox},{oy}) pixel ({x},{y}): {got} vs {expect}");
                }
            }
        }
    }

    #[test]
    fn clipped_rect_areas() {
        let mut p = Path::new();
        p.rect(-50.0, -50.0, 60.5, 60.5); // up to (10.5, 10.5)
        let m = fill(&p, &Transform::IDENTITY, FillRule::NonZero, 32, 32);
        assert!(rel_err(total(&m), 110.25) < 0.001);
        let mut q = Path::new();
        q.rect(25.5, 20.25, 100.0, 100.0);
        let m = fill(&q, &Transform::IDENTITY, FillRule::NonZero, 32, 32);
        assert!(rel_err(total(&m), 6.5 * 11.75) < 0.001);
        // Entirely outside on each side.
        for (x, y) in [(-200.0, 0.0), (200.0, 0.0), (0.0, -200.0), (0.0, 200.0)] {
            let mut s = Path::new();
            s.rect(x, y, 30.0, 30.0);
            let m = fill(&s, &Transform::IDENTITY, FillRule::NonZero, 32, 32);
            assert!(m.data.iter().all(|&v| v == 0));
        }
        // A shape covering the whole target.
        let mut s = Path::new();
        s.rect(-1000.0, -1000.0, 3000.0, 3000.0);
        let m = fill(&s, &Transform::IDENTITY, FillRule::NonZero, 32, 32);
        assert!(m.data.iter().all(|&v| v == 255));
    }

    #[test]
    fn spans_are_ordered_and_solid_interiors() {
        let mut p = Path::new();
        p.rounded_rect(3.3, 2.7, 90.0, 60.0, [12.0; 4]);
        let mut r = Rasterizer::new();
        let mut last = (0u32, 0u32);
        let mut solid = 0;
        let mut first = true;
        r.fill(&p, &Transform::IDENTITY, FillRule::NonZero, (100, 70), |s| {
            assert!(s.len > 0 && s.x + s.len <= 100 && s.y < 70);
            if let Coverage::Mask(m) = s.coverage {
                assert_eq!(m.len() as u32, s.len);
            }
            if let Coverage::Solid(v) = s.coverage {
                assert!(v > 0);
                if v == 255 {
                    solid += s.len;
                }
            }
            if !first {
                assert!(s.y > last.0 || (s.y == last.0 && s.x >= last.1), "spans out of order");
            }
            first = false;
            last = (s.y, s.x + s.len);
        });
        assert!(solid > 80 * 50, "interior should be solid spans: {solid}");
    }

    #[test]
    fn robustness() {
        let mut r = Rasterizer::new();
        let mut p = Path::new();
        p.move_to(f32::NAN, 3.0);
        p.line_to(1e30, -1e30);
        p.cubic_to(f32::INFINITY, 0.0, 5.0, f32::NEG_INFINITY, 3.0, 3.0);
        p.quad_to(1e20, 1e20, -1e20, 7.0);
        let _ = r.render_mask(&p, &Transform::IDENTITY, FillRule::NonZero, 16, 16);
        let _ = r.render_mask(&p, &Transform::scale(1e30, 1e30), FillRule::EvenOdd, 16, 16);
        let _ = r.render_mask(&p, &Transform::IDENTITY, FillRule::NonZero, 0, 0);
        let mut big = Path::new();
        big.rect(0.0, 0.0, 10.0, 10.0);
        let _ = r.render_mask(&big, &Transform::IDENTITY, FillRule::NonZero, 1, 100_000);
        // Many overlapping contours.
        let mut many = Path::new();
        for _ in 0..2000 {
            many.rect(1.0, 1.0, 5.0, 5.0);
        }
        let m = r.render_mask(&many, &Transform::IDENTITY, FillRule::NonZero, 8, 8);
        assert_eq!(m.get(3, 3), 255);
        // An unclosed contour is closed implicitly.
        let mut open = Path::new();
        open.move_to(0.0, 0.0);
        open.line_to(8.0, 0.0);
        open.line_to(8.0, 8.0);
        let m = r.render_mask(&open, &Transform::IDENTITY, FillRule::NonZero, 8, 8);
        assert!(rel_err(total(&m), 32.0) < 0.01);
    }

    #[test]
    fn reuse_gives_identical_results() {
        let mut r = Rasterizer::new();
        let mut p = Path::new();
        p.circle(10.0, 10.0, 7.7);
        let a = r.render_mask(&p, &Transform::IDENTITY, FillRule::NonZero, 20, 20);
        let mut q = Path::new();
        q.rect(0.0, 0.0, 300.0, 300.0);
        let _ = r.render_mask(&q, &Transform::IDENTITY, FillRule::NonZero, 300, 300);
        let b = r.render_mask(&p, &Transform::IDENTITY, FillRule::NonZero, 20, 20);
        assert_eq!(a, b);
        let mut lines = Rasterizer::new();
        lines.reset(4, 4);
        lines.add_line(Point::new(1.0, 0.0), Point::new(1.0, 4.0));
        lines.add_line(Point::new(3.0, 4.0), Point::new(3.0, 0.0));
        let mut m = Mask::new(4, 4);
        lines.sweep(FillRule::NonZero, |s| m.blit_span(&s));
        assert_eq!(m.row(0), &[0, 255, 255, 0]);
        let _ = vec![0u8; 1];
    }
}
