//! The Processes tab: a sortable, searchable table of running processes
//! grouped into applications and system processes.

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cmp::Ordering;

use vproto::input::keys;
use vui::{Align, ButtonKind, Color, Cursor, Font, Icon, MenuItem, Rect, Ui};

use crate::{Column, Proc, TaskManager, human_size};

const ROW_H: i32 = 34;
const HEADER_H: i32 = 32;
const STATUS_H: i32 = 30;

/// A row of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// A group heading: (title, count).
    Group(&'static str, usize),
    Proc(u64),
}

/// Recomputes the visible rows (filter, grouping, sort order).
pub fn rebuild_rows(tm: &mut TaskManager) {
    let q = tm.search.trim().to_lowercase();
    let matches = |p: &Proc| {
        q.is_empty()
            || p.title.to_lowercase().contains(&q)
            || p.name.to_lowercase().contains(&q)
            || p.koid.to_string() == q
    };
    let (sort, desc) = (tm.sort, tm.descending);
    let order = |a: &&Proc, b: &&Proc| {
        let o = match sort {
            Column::Name => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
            Column::Pid => a.koid.cmp(&b.koid),
            Column::Threads => a.threads.cmp(&b.threads),
            Column::Memory => a.memory.cmp(&b.memory),
            Column::Cpu => a.cpu.partial_cmp(&b.cpu).unwrap_or(Ordering::Equal).then(a.cpu_ns.cmp(&b.cpu_ns)),
        }
        .then(a.koid.cmp(&b.koid));
        if desc { o.reverse() } else { o }
    };
    let mut apps: Vec<&Proc> = tm.procs.iter().filter(|p| p.is_app && matches(p)).collect();
    let mut sys: Vec<&Proc> = tm.procs.iter().filter(|p| !p.is_app && matches(p)).collect();
    apps.sort_by(order);
    sys.sort_by(order);
    let mut rows = Vec::new();
    if !apps.is_empty() {
        rows.push(Row::Group("Apps", apps.len()));
        rows.extend(apps.iter().map(|p| Row::Proc(p.koid)));
    }
    if !sys.is_empty() {
        rows.push(Row::Group("System processes", sys.len()));
        rows.extend(sys.iter().map(|p| Row::Proc(p.koid)));
    }
    tm.rows = rows;
}

/// Gives the keyboard focus to the search box, selecting its text.
pub fn focus_search(ui: &mut Ui, len: usize) {
    ui.focus_text_input("proc-search");
    ui.set_text_input_selection("proc-search", 0, len);
}

/// Keyboard navigation in the table.
pub fn handle_key(tm: &mut TaskManager, ui: &mut Ui, code: u16) {
    let procs: Vec<u64> = tm.rows.iter().filter_map(|r| if let Row::Proc(k) = r { Some(*k) } else { None }).collect();
    if procs.is_empty() {
        return;
    }
    let cur = tm.selected.and_then(|k| procs.iter().position(|&p| p == k));
    let next = match code {
        keys::DOWN => Some(cur.map_or(0, |c| (c + 1).min(procs.len() - 1))),
        keys::UP => Some(cur.map_or(0, |c| c.saturating_sub(1))),
        keys::HOME => Some(0),
        keys::END => Some(procs.len() - 1),
        keys::PAGEDOWN => Some(cur.map_or(0, |c| (c + 10).min(procs.len() - 1))),
        keys::PAGEUP => Some(cur.map_or(0, |c| c.saturating_sub(10))),
        keys::DELETE => {
            tm.ask_end();
            None
        }
        keys::ESC => {
            tm.selected = None;
            None
        }
        _ => None,
    };
    if let Some(i) = next {
        tm.selected = Some(procs[i]);
        if let Some(row) = tm.rows.iter().position(|r| *r == Row::Proc(procs[i])) {
            let h = ui.height - 58 - HEADER_H - STATUS_H;
            ui.scroll_into_view("procs", h, row as i32 * ROW_H, ROW_H);
        }
    }
}

struct Cols {
    name: Rect,
    pid: Rect,
    threads: Rect,
    memory: Rect,
    cpu: Rect,
}

fn columns(r: Rect) -> Cols {
    let inner = r.inset(10, 0, 18, 0);
    let (rest, cpu) = inner.split_right(130);
    let (rest, memory) = rest.split_right(120);
    let (rest, threads) = rest.split_right(if inner.w > 560 { 90 } else { 0 });
    let (name, pid) = rest.split_right(80);
    Cols { name, pid, threads, memory, cpu }
}

/// Draws the tab: toolbar items in `top`, the table and status in `r`.
pub fn draw(tm: &mut TaskManager, ui: &mut Ui, top: Rect, r: Rect) {
    let t = ui.theme().clone();
    // Toolbar: search and End task.
    let end_r = Rect::new(top.right() - 14 - 116, top.y + 12, 116, 34);
    let enabled = tm.selected.is_some();
    if enabled {
        if ui.button_full(end_r, Some(Icon::Close), "End task", ButtonKind::Danger) {
            tm.ask_end();
        }
    } else {
        ui.canvas.fill_rounded_rect(end_r, t.radius, t.control);
        ui.canvas.stroke_rounded_rect(end_r, t.radius, 1.0, t.border);
        let tw = ui.measure("End task", Font::Bold, t.font_size) as i32;
        let x = end_r.x + (end_r.w - tw - 26) / 2;
        ui.icon(Rect::new(x, end_r.y, 18, end_r.h), Icon::Close, 16.0, t.text_faint);
        ui.label(
            Rect::new(x + 26, end_r.y, tw + 4, end_r.h),
            "End task",
            Font::Bold,
            t.font_size,
            t.text_faint,
            Align::Left,
        );
    }
    if ui.hovered(end_r) {
        let id = ui.id("end-tip");
        ui.tooltip(id, end_r, if enabled { "End the selected process (Del)" } else { "Select a process first" });
    }
    let sw = (top.w / 4).clamp(160, 240);
    let search_r = Rect::new(end_r.x - 12 - sw, end_r.y, sw, 34);
    let resp = ui.text_input(search_r, "proc-search", &mut tm.search, "Search processes");
    tm.typing = resp.focused;
    if resp.focused && ui.input.key(keys::ESC) && !tm.search.is_empty() {
        tm.search.clear();
        rebuild_rows(tm);
    } else if resp.changed {
        rebuild_rows(tm);
    }
    if resp.submitted {
        // Enter selects the first match and moves to the table.
        ui.state.focus = None;
        if let Some(Row::Proc(k)) = tm.rows.iter().find(|r| matches!(r, Row::Proc(_))) {
            tm.selected = Some(*k);
        }
    }
    if tm.search.is_empty() {
        ui.icon(Rect::new(search_r.right() - 30, search_r.y, 24, search_r.h), Icon::Search, 15.0, t.text_faint);
    }

    let (table, status) = r.split_bottom(STATUS_H);
    let (header, body) = table.split_top(HEADER_H);
    draw_header(tm, ui, header);
    let rows = tm.rows.clone();
    let content_h = rows.len() as i32 * ROW_H + 6;
    let mut menu_at = None;
    let mut hit = false;
    ui.scroll_area(body, "procs", content_h, |ui, off| {
        let first = (off / ROW_H).max(0) as usize;
        let count = (body.h / ROW_H + 2) as usize;
        for (i, row) in rows.iter().enumerate().skip(first).take(count) {
            let rr = Rect::new(body.x, body.y + 2 + i as i32 * ROW_H - off, body.w, ROW_H);
            let cols = columns(rr);
            match row {
                Row::Group(title, n) => {
                    ui.label(
                        Rect::new(cols.name.x + 6, rr.y + 6, rr.w, rr.h - 6),
                        &format!("{title} ({n})"),
                        Font::Bold,
                        t.small_size + 1.0,
                        t.text_dim,
                        Align::Left,
                    );
                }
                Row::Proc(koid) => {
                    let Some(p) = tm.procs.iter().find(|p| p.koid == *koid).cloned() else { continue };
                    let inner = rr.inset(6, 1, 6, 1);
                    let id = ui.id("proc") ^ koid.wrapping_mul(0x9e37_79b9);
                    let resp = ui.interact(id, inner);
                    if resp.pressed || resp.right_clicked {
                        hit = true;
                        tm.selected = Some(p.koid);
                        if resp.right_clicked {
                            menu_at = ui.input.pointer;
                        }
                    }
                    let selected = tm.selected == Some(p.koid);
                    if selected {
                        ui.canvas.fill_rounded_rect(inner, t.radius, t.selection);
                    } else if resp.hovered {
                        ui.canvas.fill_rounded_rect(inner, t.radius, Color::rgba(255, 255, 255, 10));
                    }
                    let icon_color = if p.is_app { t.accent_hover } else { t.text_dim };
                    ui.icon(Rect::new(cols.name.x + 8, rr.y, 20, rr.h), p.icon, 17.0, icon_color);
                    let name_r = Rect::new(cols.name.x + 38, rr.y, cols.name.w - 40, rr.h);
                    ui.label(name_r, &p.title, Font::Regular, t.font_size, t.text, Align::Left);
                    if p.title != p.name {
                        let tw = ui.measure(&p.title, Font::Regular, t.font_size) as i32;
                        let rest = Rect::new(name_r.x + tw + 8, rr.y, name_r.w - tw - 8, rr.h);
                        if rest.w > 40 {
                            ui.label(rest, &p.name, Font::Regular, t.small_size, t.text_faint, Align::Left);
                        }
                    }
                    let dim = t.text_dim;
                    ui.label(
                        cols.pid.inset(4, 0, 12, 0),
                        &p.koid.to_string(),
                        Font::Regular,
                        t.font_size - 1.0,
                        dim,
                        Align::Right,
                    );
                    if cols.threads.w > 0 {
                        ui.label(
                            cols.threads.inset(4, 0, 12, 0),
                            &p.threads.to_string(),
                            Font::Regular,
                            t.font_size - 1.0,
                            dim,
                            Align::Right,
                        );
                    }
                    // Memory and CPU cells are tinted by load, like a heat map.
                    let mem_f = (p.memory as f32 / (64u64 << 20) as f32).min(1.0);
                    heat(ui, cols.memory.inset(2, 4, 2, 4), mem_f);
                    ui.label(
                        cols.memory.inset(4, 0, 12, 0),
                        &human_size(p.memory),
                        Font::Regular,
                        t.font_size - 1.0,
                        t.text,
                        Align::Right,
                    );
                    heat(ui, cols.cpu.inset(2, 4, 2, 4), (p.cpu / 50.0).min(1.0));
                    let cpu_text = if p.cpu < 0.05 { "0%".to_string() } else { format!("{:.1}%", p.cpu) };
                    ui.label(
                        cols.cpu.inset(4, 0, 12, 0),
                        &cpu_text,
                        Font::Regular,
                        t.font_size - 1.0,
                        t.text,
                        Align::Right,
                    );
                }
            }
        }
    });
    if rows.is_empty() {
        let msg = if tm.search.is_empty() {
            "No processes".to_string()
        } else {
            format!("No process matches “{}”", tm.search)
        };
        ui.icon(Rect::new(body.x, body.y + body.h / 2 - 50, body.w, 40), Icon::Search, 30.0, t.text_faint);
        ui.label(
            Rect::new(body.x, body.y + body.h / 2, body.w, 24),
            &msg,
            Font::Bold,
            t.font_size,
            t.text_dim,
            Align::Center,
        );
    }
    if ui.hovered(body) && ui.input.pressed[0] && !hit {
        tm.selected = None;
    }
    if let Some((x, y)) = menu_at {
        ui.open_context_menu("proc-menu", x, y);
    }
    context_menu(tm, ui);
    draw_status(tm, ui, status);
}

/// A cell background whose intensity shows a load (0..=1).
fn heat(ui: &mut Ui, r: Rect, f: f32) {
    if f < 0.02 {
        return;
    }
    let a = (12.0 + f * 70.0) as u8;
    ui.canvas.fill_rounded_rect(r, 4.0, Color::rgba(91, 140, 255, a));
}

fn draw_header(tm: &mut TaskManager, ui: &mut Ui, r: Rect) {
    let t = ui.theme().clone();
    ui.canvas.fill_rect(Rect::new(r.x, r.bottom() - 1, r.w, 1), t.border);
    let c = columns(r);
    // Totals in the header, like the Windows Task Manager.
    let cpu_total = format!("{:.0}%", tm.cpu_now * 100.0);
    let mem_total = if tm.info.total_memory > 0 {
        format!(
            "{:.0}%",
            tm.info.total_memory.saturating_sub(tm.info.free_memory) as f32 * 100.0 / tm.info.total_memory as f32
        )
    } else {
        "-".to_string()
    };
    let headers = [
        ("Name", Column::Name, c.name, None),
        ("PID", Column::Pid, c.pid, None),
        ("Threads", Column::Threads, c.threads, None),
        ("Memory", Column::Memory, c.memory, Some(mem_total)),
        ("CPU", Column::Cpu, c.cpu, Some(cpu_total)),
    ];
    for (label, col, cr, total) in headers {
        if cr.w <= 0 {
            continue;
        }
        let id = ui.id(label) ^ 0x4ead;
        let resp = ui.interact(id, cr);
        if resp.hovered {
            ui.canvas.fill_rect(cr.inset(0, 3, 0, 3), Color::rgba(255, 255, 255, 8));
            ui.set_cursor(Cursor::Hand);
        }
        let active = tm.sort == col;
        let right = col != Column::Name;
        let color = if active { t.text } else { t.text_dim };
        let text_r = if right { cr.inset(4, 0, 12, 0) } else { cr.inset(38, 0, 4, 0) };
        let mut text = label.to_string();
        if let Some(total) = total {
            text = format!("{total} {label}");
        }
        ui.label(text_r, &text, Font::Bold, t.small_size + 1.0, color, if right { Align::Right } else { Align::Left });
        if active {
            let tw = ui.measure(&text, Font::Bold, t.small_size + 1.0) as i32;
            let ix = if right { text_r.right() - tw - 16 } else { text_r.x + tw + 4 };
            let icon = if tm.descending { Icon::ChevronDown } else { Icon::ChevronUp };
            ui.icon(Rect::new(ix, cr.y, 14, cr.h), icon, 11.0, t.accent_hover);
        }
        if resp.clicked {
            if tm.sort == col {
                tm.descending = !tm.descending;
            } else {
                tm.sort = col;
                tm.descending = col != Column::Name;
            }
            rebuild_rows(tm);
        }
    }
}

fn context_menu(tm: &mut TaskManager, ui: &mut Ui) {
    let items = [
        MenuItem::new("End task").shortcut("Del"),
        MenuItem::separator(),
        MenuItem::new("Copy name"),
        MenuItem::new("Copy PID"),
    ];
    if let Some(i) = ui.context_menu("proc-menu", &items) {
        let Some(p) = tm.selected_proc().cloned() else { return };
        match i {
            0 => tm.ask_end(),
            2 => ui.set_clipboard(&p.name),
            3 => ui.set_clipboard(&p.koid.to_string()),
            _ => {}
        }
    }
}

fn draw_status(tm: &mut TaskManager, ui: &mut Ui, r: Rect) {
    let t = ui.theme().clone();
    ui.canvas.fill_rect(r, t.surface);
    ui.canvas.fill_rect(Rect::new(r.x, r.y, r.w, 1), t.border);
    let threads: u32 = tm.procs.iter().map(|p| p.threads).sum();
    let mem = tm.info.total_memory.saturating_sub(tm.info.free_memory);
    let text = format!(
        "{} processes  ·  {} threads  ·  CPU {:.0}%  ·  Memory {} of {}",
        tm.procs.len(),
        threads,
        tm.cpu_now * 100.0,
        human_size(mem),
        human_size(tm.info.total_memory)
    );
    ui.label(Rect::new(r.x + 14, r.y, r.w - 28, r.h), &text, Font::Regular, t.small_size, t.text_dim, Align::Left);
    if let Some(p) = tm.selected_proc() {
        let sel = format!("Selected: {} (PID {})", p.title, p.koid);
        ui.label(
            Rect::new(r.x + r.w / 2, r.y, r.w / 2 - 14, r.h),
            &sel,
            Font::Regular,
            t.small_size,
            t.text_faint,
            Align::Right,
        );
    }
}
