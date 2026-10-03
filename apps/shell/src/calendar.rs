//! The calendar flyout opened from the taskbar clock.

use alloc::format;

use vgfx::{Align, Color, Rect};
use vrt::time::DateTime;
use vui::{Font, Icon, Ui};

use crate::{Model, chrome, taskbar};

const WIDTH: i32 = 336;
const HEIGHT: i32 = 404;
const PAD: i32 = 20;

/// Where the flyout appears: above the clock, at the right edge.
pub fn placement(screen: Rect) -> Rect {
    Rect::new(screen.w - WIDTH - 12, screen.h - taskbar::HEIGHT - 12 - HEIGHT, WIDTH, HEIGHT)
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i32, m: u8, d: u8) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i32, m: u8) -> u8 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

pub struct Calendar {
    origin: (i32, i32),
    /// Month shown (year, 1..=12).
    year: i32,
    month: u8,
}

impl Calendar {
    pub fn new(origin: (i32, i32)) -> Calendar {
        let now = DateTime::now();
        Calendar { origin, year: now.year, month: now.month }
    }

    fn step(&mut self, delta: i32) {
        let m = self.year * 12 + self.month as i32 - 1 + delta;
        self.year = m.div_euclid(12);
        self.month = (m.rem_euclid(12) + 1) as u8;
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        let t = ui.theme().clone();
        let w = ui.width;
        chrome::popup_background(&mut ui.canvas, &m.wallpaper.blurred, self.origin);
        if ui.input.key(vproto::input::keys::ESC) {
            ui.close_window();
        }

        // Live clock.
        let now = DateTime::now();
        let time = format!("{:02}:{:02}:{:02}", now.hour, now.minute, now.second);
        ui.label(Rect::new(PAD, 18, w - 2 * PAD, 40), &time, Font::Bold, 30.0, t.text, Align::Left);
        let date = format!("{}, {} {} {}", now.weekday_name(), now.day, now.month_name(), now.year);
        ui.label(Rect::new(PAD, 58, w - 2 * PAD, 22), &date, Font::Regular, 13.5, t.accent_hover, Align::Left);
        let into_second = vrt::time::unix_time_ns() % 1_000_000_000;
        let now_ns = ui.now();
        ui.repaint_at(now_ns + 1_000_000_000 - into_second);
        ui.canvas.fill_rect(Rect::new(0, 94, w, 1), Color::rgba(255, 255, 255, 18));

        // Month header with navigation.
        let title = format!("{} {}", month_name(self.month), self.year);
        ui.label(Rect::new(PAD, 106, 200, 30), &title, Font::Bold, 15.0, t.text, Align::Left);
        if ui.icon_button(Rect::new(w - PAD - 68, 106, 32, 30), Icon::ChevronUp, "Previous month") {
            self.step(-1);
        }
        if ui.icon_button(Rect::new(w - PAD - 32, 106, 32, 30), Icon::ChevronDown, "Next month") {
            self.step(1);
        }

        // Weekday names, Monday first.
        let cell_w = (w - 2 * PAD) / 7;
        let cell_h = 36;
        for (i, name) in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"].iter().enumerate() {
            let r = Rect::new(PAD + i as i32 * cell_w, 146, cell_w, 22);
            ui.label(r, name, Font::Regular, 12.0, t.text_faint, Align::Center);
        }

        // Day grid: six weeks starting on the Monday on or before the 1st.
        let first = days_from_civil(self.year, self.month, 1);
        let weekday = (first + 4).rem_euclid(7); // 0 = Sunday
        let lead = (weekday + 6) % 7; // days shown from the previous month
        let (py, pm) = if self.month == 1 { (self.year - 1, 12) } else { (self.year, self.month - 1) };
        let prev_len = days_in_month(py, pm) as i64;
        let len = days_in_month(self.year, self.month) as i64;
        let today = (now.year, now.month, now.day);
        for cell in 0..42i64 {
            let (col, row) = ((cell % 7) as i32, (cell / 7) as i32);
            let r = Rect::new(PAD + col * cell_w, 172 + row * cell_h, cell_w, cell_h);
            let n = cell - lead + 1;
            let (day, current) = if n < 1 {
                (prev_len + n, false)
            } else if n > len {
                (n - len, false)
            } else {
                (n, true)
            };
            let is_today = current && today == (self.year, self.month, day as u8);
            let label = format!("{day}");
            if is_today {
                let (cx, cy) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0);
                ui.canvas.fill_circle(cx, cy, 15.0, t.accent);
                ui.label(r, &label, Font::Bold, 13.0, Color::WHITE, Align::Center);
            } else {
                let c = if current { t.text } else { t.text_faint };
                ui.label(r, &label, Font::Regular, 13.0, c, Align::Center);
            }
        }
        chrome::popup_frame(&mut ui.canvas);
    }
}

fn month_name(m: u8) -> &'static str {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    NAMES[(m.clamp(1, 12) - 1) as usize]
}
