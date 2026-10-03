//! About Vindows: the system's version, hardware and licences.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use vui::{Align, App, Font, Icon, Rect, Ui, WindowSpec};

vrt::entry!(main);

struct About {
    info: vabi::SystemInfo,
    last_refresh: u64,
    show_details: bool,
}

fn cstr(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).trim().into()
}

impl App for About {
    fn update(&mut self, ui: &mut Ui) {
        let now = ui.now();
        if now - self.last_refresh > 1_000_000_000 {
            if let Ok(i) = vrt::object::system_info() {
                self.info = i;
            }
            self.last_refresh = now;
        }
        ui.repaint_at(self.last_refresh + 1_000_000_000);
        let t = ui.theme().clone();
        let area = ui.rect().inset(32, 28, 32, 24);

        vui::draw_logo(&mut ui.canvas, Rect::new(area.x, area.y + 4, 64, 64));
        ui.label(
            Rect::new(area.x + 88, area.y, 400, 40),
            "Vindows",
            Font::Bold,
            t.title_size + 6.0,
            t.text,
            Align::Left,
        );
        let version = cstr(&self.info.version);
        ui.label(
            Rect::new(area.x + 90, area.y + 40, 400, 24),
            &format!("{version} · microkernel edition"),
            Font::Regular,
            t.font_size,
            t.text_dim,
            Align::Left,
        );

        let card = Rect::new(area.x, area.y + 96, area.w, 168);
        ui.card(card);
        let rows: [(Icon, &str, String); 5] = [
            (Icon::Cpu, "Processor", cstr(&self.info.cpu_brand)),
            (Icon::Grid, "Cores", format!("{} online", self.info.cpu_count)),
            (
                Icon::Chart,
                "Memory",
                format!("{} MiB free of {} MiB", self.info.free_memory >> 20, self.info.total_memory >> 20),
            ),
            (Icon::Clock, "Uptime", fmt_uptime(self.info.uptime_ns)),
            (
                Icon::List,
                "Processes",
                format!("{} processes, {} threads", self.info.process_count, self.info.thread_count),
            ),
        ];
        for (i, (icon, label, value)) in rows.iter().enumerate() {
            let y = card.y + 12 + i as i32 * 30;
            ui.icon(Rect::new(card.x + 16, y, 22, 28), *icon, 18.0, t.accent);
            ui.label(Rect::new(card.x + 48, y, 110, 28), label, Font::Regular, t.font_size, t.text_dim, Align::Left);
            ui.label(Rect::new(card.x + 160, y, card.w - 176, 28), value, Font::Bold, t.font_size, t.text, Align::Left);
        }

        let mut toggle = self.show_details;
        let ty = card.bottom() + 18;
        ui.toggle(Rect::new(area.x, ty, 44, 28), "details", &mut toggle);
        self.show_details = toggle;
        ui.label(Rect::new(area.x + 54, ty, 300, 28), "Show licences", Font::Regular, t.font_size, t.text, Align::Left);
        if self.show_details {
            let text = "Vindows is written from scratch in Rust and C++. Fonts: Inter (© The Inter Project Authors), \
                        Lato (© tyPoland Lukasz Dziedzic) and JetBrains Mono (© The JetBrains Mono Project Authors), \
                        all under the SIL Open Font License 1.1.";
            ui.paragraph(Rect::new(area.x, ty + 40, area.w, 80), text, t.small_size + 1.0, t.text_dim);
        }
        let br = Rect::new(area.right() - 110, area.bottom() - 36, 110, 36);
        if ui.primary_button(br, "OK") {
            ui.close_window();
        }
    }
}

fn fmt_uptime(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h} h {m} min") } else { format!("{m} min {s} s") }
}

fn main() -> i32 {
    let mut spec = WindowSpec::new("About Vindows", 560, 500);
    spec.resizable = false;
    spec.app_id = "about".into();
    vui::run(spec, About { info: vabi::SystemInfo::default(), last_refresh: 0, show_details: false })
}
