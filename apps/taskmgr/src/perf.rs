//! The Performance tab: a resource list with sparklines on the left and a
//! large history graph with summary tiles for the selected resource.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};

use vgfx::{LineJoin, Path, StrokeStyle};
use vui::{Align, Color, Cursor, Font, Icon, Rect, Ui};

use crate::{HISTORY, Resource, TaskManager, format_uptime, human_size};

const CPU_COLOR: Color = Color::hex(0x5B8CFF);
const MEM_COLOR: Color = Color::hex(0xA77BFF);

/// Draws a history graph (values 0..=1, newest at the right).
fn graph(ui: &mut Ui, r: Rect, hist: &VecDeque<f32>, color: Color, detailed: bool) {
    ui.canvas.fill_rect(r, Color::hex(0x141418));
    if detailed {
        for k in 1..4 {
            let y = r.y + r.h * k / 4;
            ui.canvas.fill_rect(Rect::new(r.x, y, r.w, 1), Color::rgba(255, 255, 255, 12));
        }
        for k in 1..6 {
            let x = r.x + r.w * k / 6;
            ui.canvas.fill_rect(Rect::new(x, r.y, 1, r.h), Color::rgba(255, 255, 255, 9));
        }
    }
    let n = hist.len();
    if n >= 2 {
        let step = r.w as f32 / (HISTORY - 1) as f32;
        let x_of = |i: usize| r.right() as f32 - (n - 1 - i) as f32 * step;
        let y_of = |v: f32| r.bottom() as f32 - 1.0 - v.clamp(0.0, 1.0) * (r.h - 3) as f32;
        let mut area = Path::new();
        area.move_to(x_of(0), r.bottom() as f32);
        let mut line = Path::new();
        for (i, v) in hist.iter().enumerate() {
            area.line_to(x_of(i), y_of(*v));
            if i == 0 {
                line.move_to(x_of(i), y_of(*v));
            } else {
                line.line_to(x_of(i), y_of(*v));
            }
        }
        area.line_to(x_of(n - 1), r.bottom() as f32);
        area.close();
        ui.canvas.save();
        ui.canvas.clip_to(r);
        ui.canvas.fill_path(&area, color.with_alpha(if detailed { 56 } else { 48 }), vgfx::FillRule::NonZero);
        let style = StrokeStyle::new(if detailed { 1.6 } else { 1.2 }).with_join(LineJoin::Round);
        ui.canvas.stroke_path(&line, &style, color);
        ui.canvas.restore();
    }
    ui.canvas.stroke_rounded_rect(r, 2.0, 1.0, color.with_alpha(if detailed { 110 } else { 70 }));
}

fn tile(ui: &mut Ui, r: Rect, label: &str, value: &str) {
    let t = ui.theme().clone();
    ui.label(Rect::new(r.x, r.y, r.w, 18), label, Font::Regular, t.small_size, t.text_dim, Align::Left);
    ui.label(Rect::new(r.x, r.y + 20, r.w, 30), value, Font::Bold, t.heading_size + 2.0, t.text, Align::Left);
}

fn detail(ui: &mut Ui, x: i32, y: i32, w: i32, label: &str, value: &str) {
    let t = ui.theme().clone();
    ui.label(Rect::new(x, y, 110, 22), label, Font::Regular, t.font_size - 1.0, t.text_dim, Align::Left);
    ui.label(Rect::new(x + 110, y, w - 110, 22), value, Font::Regular, t.font_size - 1.0, t.text, Align::Left);
}

/// A card in the resource list; returns `true` when clicked.
fn resource_card(
    ui: &mut Ui,
    r: Rect,
    title: &str,
    value: &str,
    hist: &VecDeque<f32>,
    color: Color,
    active: bool,
) -> bool {
    let t = ui.theme().clone();
    let id = ui.id(title) ^ 0x7e50;
    let resp = ui.interact(id, r);
    if active {
        ui.canvas.fill_rounded_rect(r, t.radius_large, Color::rgba(255, 255, 255, 16));
        ui.canvas.fill_rounded_rect(Rect::new(r.x, r.y + 14, 3, r.h - 28), 1.5, color);
    } else if resp.hovered {
        ui.canvas.fill_rounded_rect(r, t.radius_large, Color::rgba(255, 255, 255, 8));
    }
    if resp.hovered {
        ui.set_cursor(Cursor::Hand);
    }
    let spark = Rect::new(r.x + 12, r.y + 12, 70, r.h - 24);
    graph(ui, spark, hist, color, false);
    ui.label(
        Rect::new(spark.right() + 12, r.y + 12, r.w - 100, 22),
        title,
        Font::Bold,
        t.font_size + 1.0,
        t.text,
        Align::Left,
    );
    ui.label(
        Rect::new(spark.right() + 12, r.y + 34, r.w - 100, 20),
        value,
        Font::Regular,
        t.small_size,
        t.text_dim,
        Align::Left,
    );
    resp.clicked
}

pub fn draw(tm: &mut TaskManager, ui: &mut Ui, r: Rect) {
    let t = ui.theme().clone();
    let (side, main) = r.split_left(250);
    ui.canvas.fill_rect(side, t.surface);
    ui.canvas.fill_rect(Rect::new(side.right() - 1, side.y, 1, side.h), t.border);
    let info = tm.info;
    let total = info.total_memory;
    let used = total.saturating_sub(info.free_memory);
    let mem_pct = if total > 0 { used as f32 * 100.0 / total as f32 } else { 0.0 };
    let cpu_value = format!("{:.0}%  ·  {} cores", tm.cpu_now * 100.0, info.cpu_count);
    let mem_value = format!("{}  ·  {:.0}%", short_size(used), mem_pct);
    let card_w = side.w - 20;
    if resource_card(
        ui,
        Rect::new(side.x + 10, side.y + 12, card_w, 72),
        "CPU",
        &cpu_value,
        &tm.cpu_hist,
        CPU_COLOR,
        tm.resource == Resource::Cpu,
    ) {
        tm.resource = Resource::Cpu;
    }
    if resource_card(
        ui,
        Rect::new(side.x + 10, side.y + 90, card_w, 72),
        "Memory",
        &mem_value,
        &tm.mem_hist,
        MEM_COLOR,
        tm.resource == Resource::Memory,
    ) {
        tm.resource = Resource::Memory;
    }
    // System summary below the cards.
    let mut y = side.y + 184;
    ui.separator(side.x + 18, y, side.w - 36);
    y += 14;
    for (icon, label, value) in [
        (Icon::Clock, "Up time", format_uptime(info.uptime_ns)),
        (Icon::List, "Processes", info.process_count.to_string()),
        (Icon::Grid, "Threads", info.thread_count.to_string()),
    ] {
        ui.icon(Rect::new(side.x + 18, y, 18, 28), icon, 15.0, t.text_faint);
        ui.label(Rect::new(side.x + 44, y, 100, 28), label, Font::Regular, t.small_size + 1.0, t.text_dim, Align::Left);
        ui.label(
            Rect::new(side.x + 120, y, side.w - 138, 28),
            &value,
            Font::Bold,
            t.small_size + 1.0,
            t.text,
            Align::Right,
        );
        y += 30;
    }

    // Main panel.
    let p = main.inset(26, 18, 26, 18);
    let (title, subtitle, color, hist) = match tm.resource {
        Resource::Cpu => ("CPU", tm.cpu_model.clone(), CPU_COLOR, &tm.cpu_hist),
        Resource::Memory => ("Memory", format!("{} total", human_size(total)), MEM_COLOR, &tm.mem_hist),
    };
    ui.label(Rect::new(p.x, p.y, 200, 34), title, Font::Bold, t.title_size, t.text, Align::Left);
    ui.label(
        Rect::new(p.x + 140, p.y + 4, p.w - 140, 30),
        &subtitle,
        Font::Regular,
        t.font_size,
        t.text_dim,
        Align::Right,
    );
    let caption = if tm.resource == Resource::Cpu { "% Utilization" } else { "Memory in use" };
    let gy = p.y + 50;
    ui.label(Rect::new(p.x, gy, 200, 18), caption, Font::Regular, t.small_size, t.text_dim, Align::Left);
    let top_label = if tm.resource == Resource::Cpu { "100%".to_string() } else { short_size(total) };
    ui.label(
        Rect::new(p.right() - 120, gy, 120, 18),
        &top_label,
        Font::Regular,
        t.small_size,
        t.text_dim,
        Align::Right,
    );
    // Room below the graph: tiles plus three detail rows (CPU) or the three
    // largest processes (memory).
    let tiles_h = if tm.resource == Resource::Cpu { 150 } else { 174 };
    let graph_r = Rect::new(p.x, gy + 22, p.w, (p.bottom() - tiles_h - gy - 44).max(60));
    let hist = hist.clone();
    graph(ui, graph_r, &hist, color, true);
    ui.label(
        Rect::new(p.x, graph_r.bottom() + 2, 200, 18),
        "60 seconds",
        Font::Regular,
        t.small_size,
        t.text_faint,
        Align::Left,
    );
    ui.label(
        Rect::new(p.right() - 60, graph_r.bottom() + 2, 60, 18),
        "0",
        Font::Regular,
        t.small_size,
        t.text_faint,
        Align::Right,
    );

    let ty = graph_r.bottom() + 30;
    let tw = p.w / 4;
    match tm.resource {
        Resource::Cpu => {
            tile(ui, Rect::new(p.x, ty, tw, 50), "Utilization", &format!("{:.0}%", tm.cpu_now * 100.0));
            tile(ui, Rect::new(p.x + tw, ty, tw, 50), "Processes", &info.process_count.to_string());
            tile(ui, Rect::new(p.x + 2 * tw, ty, tw, 50), "Threads", &info.thread_count.to_string());
            tile(ui, Rect::new(p.x + 3 * tw, ty, tw, 50), "Up time", &format_uptime(info.uptime_ns));
            let dy = ty + 66;
            detail(ui, p.x, dy, p.w, "Cores", &format!("{} logical processors", info.cpu_count));
            detail(ui, p.x, dy + 24, p.w, "Model", &tm.cpu_model);
            detail(ui, p.x, dy + 48, p.w, "Kernel", &format!("{} (microkernel)", tm.version));
        }
        Resource::Memory => {
            tile(ui, Rect::new(p.x, ty, tw, 50), "In use", &short_size(used));
            tile(ui, Rect::new(p.x + tw, ty, tw, 50), "Available", &short_size(info.free_memory));
            tile(ui, Rect::new(p.x + 2 * tw, ty, tw, 50), "Total", &short_size(total));
            tile(ui, Rect::new(p.x + 3 * tw, ty, tw, 50), "Usage", &format!("{mem_pct:.1}%"));
            // The largest processes (their resident pages, which may include
            // memory shared with other processes).
            let mut top: alloc::vec::Vec<(String, u64)> =
                tm.procs.iter().map(|p| (p.title.clone(), p.memory)).collect();
            top.sort_by_key(|e| core::cmp::Reverse(e.1));
            let dy = ty + 64;
            ui.label(
                Rect::new(p.x, dy, p.w, 20),
                "Largest processes",
                Font::Bold,
                t.small_size + 1.0,
                t.text_dim,
                Align::Left,
            );
            let max = top.first().map(|e| e.1).unwrap_or(1).max(1);
            for (k, (name, bytes)) in top.iter().take(3).enumerate() {
                let y = dy + 24 + k as i32 * 24;
                ui.label(Rect::new(p.x, y, 170, 22), name, Font::Regular, t.font_size - 1.0, t.text, Align::Left);
                let bar = Rect::new(p.x + 180, y + 8, (p.w - 280).max(40), 6);
                ui.canvas.fill_rounded_rect(bar, 3.0, t.control_hover);
                let w = (bar.w as f64 * *bytes as f64 / max as f64) as i32;
                if w > 0 {
                    ui.canvas.fill_rounded_rect(Rect::new(bar.x, bar.y, w.max(6), bar.h), 3.0, MEM_COLOR);
                }
                ui.label(
                    Rect::new(bar.right() + 10, y, p.right() - bar.right() - 10, 22),
                    &short_size(*bytes),
                    Font::Regular,
                    t.font_size - 1.0,
                    t.text_dim,
                    Align::Right,
                );
            }
        }
    }
}

/// Memory size with at most one decimal and no trailing ".0".
fn short_size(bytes: u64) -> String {
    let s = human_size(bytes);
    s.replace(".0 ", " ")
}
