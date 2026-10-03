//! The library: a grid of thumbnail cards for the pictures in the folder.

use alloc::format;
use alloc::string::String;

use vgfx::{Align, Color, Rect};
use vproto::input::keys;
use vui::{Cursor, Font, Icon, Ui};

use crate::Photos;
use crate::catalog::{self, Thumb};
use crate::loader::{CARD_H, CARD_W};
use crate::render::draw_rounded;

const HEADER_H: i32 = 92;
const MARGIN: i32 = 32;
const MIN_GAP: i32 = 22;
const THUMB_W: i32 = CARD_W as i32;
const THUMB_H: i32 = CARD_H as i32;
/// A card: the thumbnail and two lines of text.
const ITEM_H: i32 = THUMB_H + 52;
const ROW_GAP: i32 = 22;
const RADIUS: f32 = 10.0;

impl Photos {
    pub(crate) fn library(&mut self, ui: &mut Ui) {
        let full = ui.rect();
        let (header, body) = full.split_top(HEADER_H);
        self.library_header(ui, header);
        if let Some(err) = self.list_error.clone() {
            empty_state(ui, body, Icon::Warning, "Can't show this folder", &err);
            return;
        }
        if self.entries.is_empty() {
            let hint = format!("Pictures you save in {} (PNG, JPEG, BMP or QOI) appear here.", self.dir);
            empty_state(ui, body, Icon::Image, "No pictures yet", &hint);
            return;
        }
        // Columns: as many cards as fit, spread over the width.
        let avail = body.w - 2 * MARGIN;
        let cols = ((avail + MIN_GAP) / (THUMB_W + MIN_GAP)).max(1);
        let gap = if cols > 1 { ((avail - cols * THUMB_W) / (cols - 1)).clamp(MIN_GAP, 56) } else { 0 };
        let grid_w = cols * THUMB_W + (cols - 1) * gap;
        let x0 = body.x + (body.w - grid_w) / 2;
        let pitch = ITEM_H + ROW_GAP;
        let n = self.entries.len();
        let rows = (n as i32 + cols - 1) / cols;
        let content_h = rows * pitch + 24;
        self.library_keys(ui, cols as usize, (body.h / pitch).max(1) as usize);
        if self.reveal_selection {
            let row = self.current as i32 / cols;
            ui.scroll_into_view("grid", body.h, row * pitch, ITEM_H + 18);
            self.reveal_selection = false;
        }
        let mut open = None;
        let mut below = 0;
        ui.scroll_area(body, "grid", content_h, |ui, offset| {
            let first_row = ((offset - 8) / pitch).max(0);
            let last_row = ((offset + body.h) / pitch + 1).min(rows);
            for row in first_row..last_row {
                for col in 0..cols {
                    let i = (row * cols + col) as usize;
                    if i >= n {
                        break;
                    }
                    let x = x0 + col * (THUMB_W + gap);
                    let y = body.y + 8 + row * pitch - offset;
                    if self.card(ui, i, x, y) {
                        open = Some(i);
                    }
                }
            }
            below = last_row;
        });
        // Thumbnails for the row below the visible ones, so that scrolling finds them ready.
        for i in (below * cols) as usize..((below + 1) * cols).min(n as i32) as usize {
            self.want_thumbs(i, false);
        }
        if let Some(i) = open {
            self.keyboard_nav = false;
            self.open(i);
        }
    }

    fn library_header(&mut self, ui: &mut Ui, r: Rect) {
        let t = ui.theme().clone();
        let title = String::from(catalog::folder_name(&self.dir));
        ui.label(
            Rect::new(r.x + MARGIN, r.y + 20, r.w - 2 * MARGIN - 60, 38),
            &title,
            Font::Bold,
            t.title_size + 2.0,
            t.text,
            Align::Left,
        );
        let count = self.entries.len();
        let what = if self.list_error.is_some() {
            String::from("Unavailable")
        } else if count == 0 {
            String::from("No pictures")
        } else if count == 1 {
            String::from("1 picture")
        } else {
            format!("{count} pictures")
        };
        let sub = format!("{what}  ·  {}", self.dir);
        ui.label(
            Rect::new(r.x + MARGIN + 1, r.y + 58, r.w - 2 * MARGIN - 60, 20),
            &sub,
            Font::Regular,
            13.0,
            t.text_dim,
            Align::Left,
        );
        if ui.icon_button(Rect::new(r.right() - MARGIN - 38, r.y + 26, 38, 38), Icon::Refresh, "Refresh (F5)") {
            self.reload();
        }
    }

    /// Draws card `i` with its top-left corner at (x, y); returns `true` if it was clicked.
    fn card(&mut self, ui: &mut Ui, i: usize, x: i32, y: i32) -> bool {
        let t = ui.theme().clone();
        let id = ui.id("card") ^ ((i as u64 + 1) << 24);
        let item = Rect::new(x, y, THUMB_W, ITEM_H);
        let resp = ui.interact(id, item.inflate(6));
        let hover = ui.animate(id, if resp.hovered { 1.0 } else { 0.0 }, 7.0);
        if resp.hovered {
            ui.set_cursor(Cursor::Hand);
        }
        if resp.pressed {
            self.current = i;
        }
        let lift = (hover * 3.0 + 0.5) as i32;
        let thumb = Rect::new(x, y - lift, THUMB_W, THUMB_H);
        if hover > 0.01 {
            ui.canvas.fill_rounded_rect(
                item.inflate(8).translate(0, -lift),
                14.0,
                Color::rgba(255, 255, 255, (9.0 * hover) as u8),
            );
            self.card_shadow.draw(
                &mut ui.canvas,
                thumb.translate(0, 6),
                Color::rgba(0, 0, 0, (130.0 * hover) as u8),
                false,
            );
        }
        self.want_thumbs(i, false);
        self.entries[i].used = ui.now();
        let e = &self.entries[i];
        match &e.thumb {
            Thumb::Ready(th) => draw_rounded(&mut ui.canvas, &th.card, thumb.x, thumb.y, RADIUS as i32, 255),
            Thumb::Failed(_) => placeholder(ui, thumb, Icon::Warning, t.warning.fade(0.8)),
            _ => placeholder(ui, thumb, Icon::Image, t.text_faint),
        }
        if self.keyboard_nav && i == self.current {
            ui.canvas.stroke_rounded_rect(thumb.inflate(4), RADIUS + 4.0, 2.0, t.accent);
        }
        let e = &self.entries[i];
        let (name, info) = (e.name.clone(), card_info(e));
        ui.label(
            Rect::new(x + 2, thumb.bottom() + 10, THUMB_W - 4, 20),
            &name,
            Font::Regular,
            14.0,
            t.text,
            Align::Left,
        );
        ui.label(
            Rect::new(x + 2, thumb.bottom() + 31, THUMB_W - 4, 16),
            &info,
            Font::Regular,
            12.0,
            t.text_dim,
            Align::Left,
        );
        resp.clicked
    }

    fn library_keys(&mut self, ui: &mut Ui, cols: usize, rows_visible: usize) {
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        let cur = self.current.min(n - 1);
        let mut next = None;
        let mut open = false;
        for k in &ui.input.keys {
            let page = cols * rows_visible.max(1);
            next = match k.code {
                keys::LEFT => Some(cur.saturating_sub(1)),
                keys::RIGHT => Some((cur + 1).min(n - 1)),
                keys::UP => Some(if cur >= cols { cur - cols } else { cur }),
                keys::DOWN => Some(if cur + cols < n { cur + cols } else { cur }),
                keys::HOME => Some(0),
                keys::END => Some(n - 1),
                keys::PAGEUP => Some(cur.saturating_sub(page)),
                keys::PAGEDOWN => Some((cur + page).min(n - 1)),
                keys::ENTER | keys::KPENTER | keys::SPACE => {
                    open = true;
                    next
                }
                _ => next,
            };
        }
        if ui.input.key(keys::F5) {
            self.reload();
            return;
        }
        if let Some(i) = next {
            self.current = i;
            self.keyboard_nav = true;
            self.reveal_selection = true;
        }
        if open {
            self.open(self.current);
        }
    }
}

/// "2400 × 1600 · JPEG", or the file size until the file has been read.
fn card_info(e: &catalog::Entry) -> String {
    match (&e.meta, &e.thumb) {
        (Some(m), _) => format!("{} × {}  ·  {}", m.width, m.height, m.format.name()),
        (None, Thumb::Failed(why)) => why.clone(),
        (None, _) if e.size > 0 => catalog::human_size(e.size),
        _ => String::new(),
    }
}

/// The grey stand-in for a thumbnail that is not ready.
fn placeholder(ui: &mut Ui, r: Rect, icon: Icon, color: Color) {
    ui.canvas.fill_rounded_rect_gradient(r, RADIUS, Color::hex(0x2C2C34), Color::hex(0x24242A));
    ui.icon(r, icon, 30.0, color);
}

/// A centred icon, heading and explanation; returns the y just below the text.
pub(crate) fn empty_state(ui: &mut Ui, area: Rect, icon: Icon, title: &str, text: &str) -> i32 {
    let t = ui.theme().clone();
    let w = area.w.min(460);
    let top = area.y + (area.h - 220).max(0) / 2;
    let circle = Rect::new(area.x + (area.w - 88) / 2, top, 88, 88);
    ui.canvas.fill_rounded_rect(circle, 44.0, Color::rgba(255, 255, 255, 12));
    ui.icon(circle, icon, 40.0, if icon == Icon::Warning { t.warning } else { t.text_dim });
    let x = area.x + (area.w - w) / 2;
    ui.label(Rect::new(x, circle.bottom() + 18, w, 28), title, Font::Bold, t.heading_size + 2.0, t.text, Align::Center);
    let f = ui.ctx.font(Font::Regular);
    let used = ui.ctx.text.draw_wrapped(
        &mut ui.canvas,
        f,
        14.0,
        Rect::new(x, circle.bottom() + 54, w, 80),
        text,
        t.text_dim,
        Align::Center,
    );
    circle.bottom() + 54 + used
}
