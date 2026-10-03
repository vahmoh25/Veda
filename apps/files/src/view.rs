//! Drawing and mouse interaction of the Files window: toolbar with the
//! breadcrumb path bar and search box, places sidebar, list and grid views,
//! status bar, context menus and the Properties dialog.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vfiles::HOME;
use vfiles::format::{friendly_time, human_size};
use vfiles::kind::{FileKind, file_kind, kind_name};
use vfiles::path::{display_path, file_name, is_read_only, is_within, join, parent, resolve};
use vui::{Align, ButtonKind, Color, Cursor, Font, Icon, MenuItem, Rect, Ui};

use crate::icons;
use crate::{Drag, Files, Props, SortKey, ViewMode};

const TOOLBAR_H: i32 = 56;
const STATUS_H: i32 = 30;
const SIDEBAR_W: i32 = 196;
const ROW_H: i32 = 32;
const HEADER_H: i32 = 32;
const TILE_W: i32 = 118;
const TILE_H: i32 = 128;

/// Gives keyboard focus to the text input `label` and selects `sel`
/// (anchor, cursor) in it.
fn focus_text(ui: &mut Ui, label: &str, sel: (usize, usize)) {
    ui.focus_text_input(label);
    ui.set_text_input_selection(label, sel.0, sel.1);
}

/// A toolbar icon button with a tooltip; drawn faded and inert when disabled.
fn tool_button(ui: &mut Ui, r: Rect, icon: Icon, tip: &str, enabled: bool) -> bool {
    if enabled {
        return ui.icon_button(r, icon, tip);
    }
    let c = ui.theme().text_faint.fade(0.6);
    ui.icon(r, icon, 18.0, c);
    false
}

/// Things a context menu entry can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    OpenInEditor,
    OpenTerminal,
    OpenTerminalHere,
    Cut,
    Copy,
    Paste,
    Rename,
    Delete,
    CopyPath,
    Properties,
    FolderProperties,
    NewFolder,
    NewDocument,
    SelectAll,
    ToggleHidden,
    Refresh,
    Sort(SortKey),
    Ascending(bool),
    Mode(ViewMode),
}

/// Draws the whole window.
pub fn draw(f: &mut Files, ui: &mut Ui) {
    let full = ui.rect();
    let (toolbar, rest) = full.split_top(TOOLBAR_H);
    let (body, status) = rest.split_bottom(STATUS_H);
    let (sidebar, main) = body.split_left(SIDEBAR_W);
    let mut typing = false;
    f.drop_target = None;
    draw_toolbar(f, ui, toolbar, &mut typing);
    draw_sidebar(f, ui, sidebar);
    draw_main(f, ui, main, &mut typing);
    draw_status(f, ui, status);
    f.typing = typing;
    finish_drag(f, ui);
    context_menus(f, ui);
}

/// Starts, tracks and completes drag-and-drop of items.
fn finish_drag(f: &mut Files, ui: &mut Ui) {
    let Some(d) = &mut f.drag else { return };
    if ui.input.down[0] {
        if !d.active
            && let Some((x, y)) = ui.input.pointer
            && (x - d.origin.0).abs() + (y - d.origin.1).abs() > 8
            && !d.paths.is_empty()
        {
            d.active = true;
            f.pending_single = None;
        }
        if !d.active {
            return;
        }
        ui.set_cursor(Cursor::Move);
        let Some((px, py)) = ui.input.pointer else { return };
        let t = ui.theme().clone();
        let copy = ui.input.ctrl() || d.paths.iter().any(|p| is_read_only(p));
        let what = if d.paths.len() == 1 {
            format!("“{}”", file_name(&d.paths[0]))
        } else {
            format!("{} items", d.paths.len())
        };
        let text = match &f.drop_target {
            Some(target) => {
                let name = if target == HOME {
                    "Home"
                } else if target == "/" {
                    "Computer"
                } else {
                    file_name(target)
                };
                format!("{} {what} to {name}", if copy { "Copy" } else { "Move" })
            }
            None => what,
        };
        let w = ui.measure(&text, Font::Regular, t.small_size) as i32 + 44;
        let x = (px + 14).min(ui.width - w - 4).max(4);
        let y = (py + 18).min(ui.height - 34).max(4);
        let badge = Rect::new(x, y, w, 30);
        ui.canvas.fill_rounded_rect(badge, 8.0, Color::hex(0x2E2E36).with_alpha(240));
        ui.canvas.stroke_rounded_rect(
            badge,
            8.0,
            1.0,
            if f.drop_target.is_some() { t.accent } else { t.border_strong },
        );
        let icon =
            if d.paths.len() == 1 { icons::small_icon(&d.paths[0], f.fs.is_dir(&d.paths[0])) } else { Icon::Copy };
        ui.icon(Rect::new(badge.x + 8, badge.y, 20, badge.h), icon, 15.0, t.accent_hover);
        ui.label(badge.inset(34, 0, 8, 0), &text, Font::Regular, t.small_size, t.text, Align::Left);
        return;
    }
    // Released.
    let Some(d) = f.drag.take() else { return };
    if d.active {
        if let Some(target) = f.drop_target.take() {
            f.drop_into(d.paths, &target, ui.input.ctrl());
        }
    } else if let Some(vi) = f.pending_single.take() {
        f.select_only(vi);
    }
}

// ---- toolbar ---------------------------------------------------------------

fn breadcrumbs(cwd: &str) -> Vec<(String, String, Option<Icon>)> {
    let mut out = Vec::new();
    let rest = if is_within(cwd, HOME) {
        out.push(("Home".to_string(), HOME.to_string(), Some(Icon::Home)));
        &cwd[HOME.len()..]
    } else {
        out.push(("Computer".to_string(), "/".to_string(), Some(Icon::Monitor)));
        cwd
    };
    let mut path = out[0].1.clone();
    for comp in rest.split('/').filter(|c| !c.is_empty()) {
        path = join(&path, comp);
        let label = if path == "/system" { "System".to_string() } else { comp.to_string() };
        out.push((label, path.clone(), None));
    }
    out
}

fn draw_toolbar(f: &mut Files, ui: &mut Ui, r: Rect, typing: &mut bool) {
    let t = ui.theme().clone();
    ui.canvas.fill_rect(Rect::new(r.x, r.bottom() - 1, r.w, 1), t.border);
    let y = r.y + (r.h - 34) / 2;
    let mut x = r.x + 10;
    if tool_button(ui, Rect::new(x, y, 34, 34), Icon::ChevronLeft, "Back (Alt+Left)", !f.back.is_empty()) {
        f.go_back();
    }
    x += 36;
    if tool_button(ui, Rect::new(x, y, 34, 34), Icon::ChevronRight, "Forward (Alt+Right)", !f.forward.is_empty()) {
        f.go_forward();
    }
    x += 36;
    if tool_button(ui, Rect::new(x, y, 34, 34), Icon::Up, "Up one level (Backspace)", f.cwd != "/") {
        f.go_up();
    }
    x += 44;

    // Right side, from the right edge: view switcher, search, sort, new.
    let mut right = r.right() - 10;
    let seg = Rect::new(right - 72, y, 72, 34);
    right = seg.x - 10;
    ui.canvas.fill_rounded_rect(seg, t.radius, t.control);
    ui.canvas.stroke_rounded_rect(seg, t.radius, 1.0, t.border);
    for (i, (icon, mode, tip)) in
        [(Icon::List, ViewMode::List, "List view (Ctrl+1)"), (Icon::Grid, ViewMode::Grid, "Icon view (Ctrl+2)")]
            .iter()
            .enumerate()
    {
        let b = Rect::new(seg.x + 2 + i as i32 * 35, seg.y + 2, 33, 30);
        if f.mode == *mode {
            ui.canvas.fill_rounded_rect(b, t.radius - 1.0, t.control_hover.shade(0.08));
        }
        if tool_button(ui, b, *icon, tip, true) {
            f.mode = *mode;
            f.reveal_cursor = true;
        }
    }
    let search_w = (r.w / 4).clamp(150, 240);
    let search = Rect::new(right - search_w, y, search_w, 34);
    right = search.x - 10;
    let sort_r = Rect::new(right - 92, y, 92, 34);
    right = sort_r.x - 4;
    let sort_label = match f.sort {
        SortKey::Name => "Name",
        SortKey::Size => "Size",
        SortKey::Kind => "Type",
        SortKey::Modified => "Date",
    };
    if ui.button_full(
        sort_r,
        Some(if f.ascending { Icon::ChevronUp } else { Icon::ChevronDown }),
        sort_label,
        ButtonKind::Ghost,
    ) {
        ui.open_context_menu("sort-menu", sort_r.x, sort_r.bottom() + 4);
    }
    if ui.hovered(sort_r) {
        let id = ui.id("sort-tip");
        ui.tooltip(id, sort_r, "Sort order");
    }
    let new_r = Rect::new(right - 34, y, 34, 34);
    right = new_r.x - 10;
    let ro = f.read_only();
    if tool_button(ui, new_r, Icon::Plus, "New folder (Ctrl+Shift+N)", !ro) {
        f.new_folder();
    }

    // Search box.
    if f.focus_search {
        f.focus_search = false;
        let len = f.search.len();
        focus_text(ui, "search", (0, len));
    }
    let folder = breadcrumbs(&f.cwd).pop().map(|c| c.0).unwrap_or_default();
    let placeholder = format!("Search {folder}");
    let resp = ui.text_input(search, "search", &mut f.search, &placeholder);
    if resp.changed {
        f.rebuild_view();
        f.reset_scroll = true;
    }
    if resp.submitted {
        ui.state.focus = None;
        if !f.view.is_empty() {
            f.select_only(0);
        }
    }
    *typing |= resp.focused;
    let icon_r = Rect::new(search.right() - 30, search.y, 24, search.h);
    if f.search.is_empty() {
        ui.icon(icon_r, Icon::Search, 15.0, t.text_faint);
    } else if tool_button(ui, icon_r.inset(0, 5, 0, 5), Icon::Close, "Clear search (Esc)", true) {
        f.search.clear();
        f.rebuild_view();
    }

    // Path bar.
    let bar = Rect::new(x, y, (right - x).max(80), 34);
    if let Some(mut text) = f.path_edit.take() {
        if !f.path_edit_focused {
            f.path_edit_focused = true;
            let len = text.len();
            focus_text(ui, "path-edit", (0, len));
        }
        let resp = ui.text_input(bar, "path-edit", &mut text, "Type a folder path and press Enter");
        *typing = true;
        if resp.submitted {
            f.path_edit_focused = false;
            let target = resolve(&f.cwd, text.trim());
            ui.state.focus = None;
            f.navigate(&target, true);
        } else if !resp.focused && f.path_edit_focused && !ui.input.pressed[0] {
            // Focus moved away (Esc or a click elsewhere): cancel.
            f.path_edit_focused = false;
        } else if !resp.focused && ui.input.pressed[0] && !ui.hovered(bar) {
            f.path_edit_focused = false;
        } else {
            f.path_edit = Some(text);
        }
        return;
    }
    ui.canvas.fill_rounded_rect(bar, t.radius, t.control);
    ui.canvas.stroke_rounded_rect(bar, t.radius, 1.0, t.border);
    let crumbs = breadcrumbs(&f.cwd);
    let size = t.font_size;
    let widths: Vec<i32> = crumbs
        .iter()
        .map(|(label, _, icon)| {
            ui.measure(label, Font::Regular, size) as i32 + 18 + if icon.is_some() { 22 } else { 0 }
        })
        .collect();
    let sep_w = 16;
    let avail = bar.w - 12;
    // Drop leading crumbs (after the first) until the rest fits.
    let mut first_shown = 1;
    let total = |from: usize| {
        widths[0]
            + widths[from.min(widths.len())..].iter().sum::<i32>()
            + sep_w * (widths.len() - from.min(widths.len())) as i32
    };
    while first_shown < crumbs.len().saturating_sub(1) && total(first_shown) + 30 > avail {
        first_shown += 1;
    }
    let mut cx = bar.x + 6;
    let mut clicked_crumb = false;
    for (i, (label, path, icon)) in crumbs.iter().enumerate() {
        if i > 0 && i < first_shown {
            continue;
        }
        if i == first_shown && first_shown > 1 {
            ui.label(Rect::new(cx, bar.y, 22, bar.h), "…", Font::Regular, size, t.text_faint, Align::Center);
            cx += 22;
        }
        if i > 0 {
            ui.icon(Rect::new(cx, bar.y, sep_w, bar.h), Icon::ChevronRight, 12.0, t.text_faint);
            cx += sep_w;
        }
        let w = widths[i].min(bar.right() - cx - 4).max(0);
        let cr = Rect::new(cx, bar.y + 4, w, bar.h - 8);
        let id = ui.id(path) ^ 0xb7ead;
        let resp = ui.interact(id, cr);
        let last = i + 1 == crumbs.len();
        if resp.hovered {
            ui.canvas.fill_rounded_rect(
                cr,
                t.radius - 1.0,
                Color::rgba(255, 255, 255, if resp.held { 18 } else { 12 }),
            );
            ui.set_cursor(Cursor::Hand);
        }
        let color = if last { t.text } else { t.text_dim };
        let mut tx = cr.x + 9;
        if let Some(icon) = icon {
            ui.icon(Rect::new(tx, cr.y, 16, cr.h), *icon, 15.0, color);
            tx += 22;
        }
        ui.label(
            Rect::new(tx, cr.y, cr.right() - tx - 6, cr.h),
            label,
            if last { Font::Bold } else { Font::Regular },
            size,
            color,
            Align::Left,
        );
        if resp.clicked {
            clicked_crumb = true;
            let p = path.clone();
            f.navigate(&p, true);
        }
        cx += w;
    }
    // Clicking the empty part of the bar edits the path as text.
    let id = ui.id("path-bar-empty");
    let empty = Rect::new(cx, bar.y, (bar.right() - cx).max(0), bar.h);
    let resp = ui.interact(id, empty);
    if resp.hovered {
        ui.set_cursor(Cursor::Text);
    }
    if resp.clicked && !clicked_crumb {
        f.path_edit = Some(f.cwd.clone());
        f.path_edit_focused = false;
    }
}

// ---- sidebar ---------------------------------------------------------------

fn draw_sidebar(f: &mut Files, ui: &mut Ui, r: Rect) {
    let t = ui.theme().clone();
    ui.canvas.fill_rect(r, t.surface);
    ui.canvas.fill_rect(Rect::new(r.right() - 1, r.y, 1, r.h), t.border);
    let places: [(&str, Icon, &str); 5] = [
        ("Home", Icon::Home, HOME),
        ("Desktop", Icon::Monitor, "/home/user/Desktop"),
        ("Documents", Icon::Document, "/home/user/Documents"),
        ("Pictures", Icon::Image, "/home/user/Pictures"),
        ("Music", Icon::Music, "/home/user/Music"),
    ];
    let locations: [(&str, Icon, &str); 3] =
        [("Computer", Icon::Cpu, "/"), ("System", Icon::Lock, "/system"), ("Temporary", Icon::Clock, "/tmp")];
    // The most specific place containing the current folder is active.
    let active = places
        .iter()
        .chain(locations.iter())
        .filter(|(_, _, p)| is_within(&f.cwd, p))
        .max_by_key(|(_, _, p)| p.len())
        .map(|(_, _, p)| *p);
    let mut y = r.y + 14;
    for (title, list) in [("Places", &places[..]), ("Locations", &locations[..])] {
        ui.label(Rect::new(r.x + 18, y, r.w - 30, 20), title, Font::Bold, t.small_size, t.text_faint, Align::Left);
        y += 26;
        for (label, icon, path) in list {
            let row = Rect::new(r.x + 8, y, r.w - 16, 34);
            let id = ui.id(path) ^ 0x51de;
            let resp = ui.interact(id, row);
            let is_active = active == Some(*path);
            let dragging = f.drag.as_ref().is_some_and(|d| d.active);
            if dragging && resp.hovered && *path != f.cwd.as_str() {
                f.drop_target = Some(path.to_string());
                ui.canvas.fill_rounded_rect(row, t.radius, t.accent.with_alpha(40));
                ui.canvas.stroke_rounded_rect(row, t.radius, 1.5, t.accent);
            }
            if is_active {
                ui.canvas.fill_rounded_rect(row, t.radius, Color::rgba(255, 255, 255, 16));
                ui.canvas.fill_rounded_rect(Rect::new(row.x, row.y + 9, 3, row.h - 18), 1.5, t.accent);
            } else if resp.hovered {
                ui.canvas.fill_rounded_rect(row, t.radius, Color::rgba(255, 255, 255, 9));
            }
            if resp.hovered {
                ui.set_cursor(Cursor::Hand);
            }
            let color = if is_active { t.accent_hover } else { t.text_dim };
            ui.icon(Rect::new(row.x + 12, row.y, 20, row.h), *icon, 17.0, color);
            let tc = if is_active { t.text } else { t.text_dim.lerp(t.text, 0.4) };
            ui.label(
                Rect::new(row.x + 42, row.y, row.w - 50, row.h),
                label,
                if is_active { Font::Bold } else { Font::Regular },
                t.font_size,
                tc,
                Align::Left,
            );
            if resp.clicked {
                f.navigate(path, true);
            }
            if resp.right_clicked
                && let Some((px, py)) = ui.input.pointer
            {
                f.context_target = Some(path.to_string());
                ui.open_context_menu("place-menu", px, py);
            }
            y += 36;
        }
        y += 14;
    }
}

// ---- main area -------------------------------------------------------------

fn draw_main(f: &mut Files, ui: &mut Ui, r: Rect, typing: &mut bool) {
    if let Some(err) = f.list_error.clone() {
        empty_state(ui, r, Icon::Warning, "Can't show this folder", &err);
        return;
    }
    let (rows_r, header) = if f.mode == ViewMode::List {
        let (h, rest) = r.split_top(HEADER_H);
        (rest, Some(h))
    } else {
        (r, None)
    };
    if let Some(h) = header {
        draw_header(f, ui, h);
    }
    if f.reset_scroll {
        f.reset_scroll = false;
        ui.scroll_into_view("list", rows_r.h, 0, 0);
        ui.scroll_into_view("grid", rows_r.h, 0, 0);
    }
    let n = f.view.len();
    let mut hit = false;
    let mut menu_at: Option<(i32, i32)> = None;
    let pressed_here = ui.hovered(rows_r) && (ui.input.pressed[0] || ui.input.pressed[1]);
    if pressed_here && f.path_edit.is_none() {
        // Clicking the file area takes the focus from text fields.
        if f.rename.is_none() {
            ui.state.focus = None;
        }
    }
    if f.mode == ViewMode::List {
        f.page_rows = (rows_r.h / ROW_H).max(1) as usize;
        if f.reveal_cursor {
            f.reveal_cursor = false;
            if let Some(c) = f.cursor {
                ui.scroll_into_view("list", rows_r.h, c as i32 * ROW_H + 4, ROW_H);
            }
        }
        let content_h = n as i32 * ROW_H + 8;
        ui.scroll_area(rows_r, "list", content_h, |ui, off| {
            let first = (off / ROW_H).max(0) as usize;
            let count = (rows_r.h / ROW_H + 2) as usize;
            for vi in first..(first + count).min(n) {
                let rr = Rect::new(rows_r.x, rows_r.y + 4 + vi as i32 * ROW_H - off, rows_r.w, ROW_H);
                if item(f, ui, vi, rr, false, typing, &mut menu_at) {
                    hit = true;
                }
            }
        });
    } else {
        let cols = (((rows_r.w - 16) / TILE_W).max(1)) as usize;
        f.grid_cols = cols;
        let cell_w = (rows_r.w - 16) / cols as i32;
        let rows = n.div_ceil(cols);
        f.page_rows = (rows_r.h / TILE_H).max(1) as usize;
        if f.reveal_cursor {
            f.reveal_cursor = false;
            if let Some(c) = f.cursor {
                ui.scroll_into_view("grid", rows_r.h, (c / cols) as i32 * TILE_H, TILE_H + 16);
            }
        }
        let content_h = rows as i32 * TILE_H + 16;
        ui.scroll_area(rows_r, "grid", content_h, |ui, off| {
            let first_row = (off / TILE_H).max(0) as usize;
            let last_row = ((off + rows_r.h) / TILE_H + 1) as usize;
            for row in first_row..=last_row.min(rows) {
                for col in 0..cols {
                    let vi = row * cols + col;
                    if vi >= n {
                        break;
                    }
                    let tile = Rect::new(
                        rows_r.x + 8 + col as i32 * cell_w,
                        rows_r.y + 8 + row as i32 * TILE_H - off,
                        cell_w,
                        TILE_H,
                    );
                    if item(f, ui, vi, tile, true, typing, &mut menu_at) {
                        hit = true;
                    }
                }
            }
        });
    }
    if n == 0 {
        if f.search.is_empty() {
            let msg = if f.read_only() {
                "This part of the system image has no files."
            } else {
                "Right-click to create a folder or a text document."
            };
            empty_state(ui, rows_r, Icon::Folder, "This folder is empty", msg);
        } else {
            let msg = format!("Nothing in this folder matches “{}”.", f.search);
            empty_state(ui, rows_r, Icon::Search, "No matches", &msg);
        }
    }
    // Clicks on the empty area.
    if pressed_here && !hit && ui.pointer().is_some() {
        if ui.input.pressed[0] && !ui.input.ctrl() && !ui.input.shift() {
            if f.rename.is_some() {
                f.commit_rename();
            }
            f.selected.clear();
            f.cursor = None;
        }
        if ui.input.pressed[1]
            && let Some((px, py)) = ui.input.pointer
        {
            f.selected.clear();
            ui.open_context_menu("background-menu", px, py);
        }
    }
    if let Some((x, y)) = menu_at {
        ui.open_context_menu("item-menu", x, y);
    }
}

fn empty_state(ui: &mut Ui, r: Rect, icon: Icon, title: &str, message: &str) {
    let t = ui.theme().clone();
    let cy = r.y + r.h / 2 - 50;
    ui.canvas.fill_circle((r.x + r.w / 2) as f32, (cy + 20) as f32, 34.0, Color::rgba(255, 255, 255, 8));
    ui.icon(Rect::new(r.x, cy, r.w, 40), icon, 34.0, t.text_faint);
    ui.label(Rect::new(r.x, cy + 66, r.w, 26), title, Font::Bold, t.heading_size - 2.0, t.text_dim, Align::Center);
    let w = (r.w - 80).clamp(100, 420);
    let text_w = ui.measure(message, Font::Regular, t.font_size) as i32;
    let mr = Rect::new(r.x + (r.w - w.min(text_w + 4)) / 2, cy + 98, w.min(text_w + 4), 60);
    ui.paragraph(mr, message, t.font_size, t.text_faint);
}

fn draw_header(f: &mut Files, ui: &mut Ui, r: Rect) {
    let t = ui.theme().clone();
    ui.canvas.fill_rect(Rect::new(r.x, r.bottom() - 1, r.w, 1), t.border);
    let cols = columns(r);
    let headers = [
        ("Name", SortKey::Name, cols.name),
        ("Size", SortKey::Size, cols.size),
        ("Type", SortKey::Kind, cols.kind),
        ("Modified", SortKey::Modified, cols.modified),
    ];
    for (label, key, cr) in headers {
        if cr.w <= 0 {
            continue;
        }
        let id = ui.id(label) ^ 0x4ead;
        let resp = ui.interact(id, cr);
        if resp.hovered {
            ui.canvas.fill_rect(cr.inset(0, 4, 0, 4), Color::rgba(255, 255, 255, 8));
            ui.set_cursor(Cursor::Hand);
        }
        let active = f.sort == key;
        let color = if active { t.text } else { t.text_dim };
        let align = if key == SortKey::Size { Align::Right } else { Align::Left };
        let text_r = if key == SortKey::Size {
            cr.inset(8, 0, 22, 0)
        } else {
            cr.inset(if key == SortKey::Name { 44 } else { 8 }, 0, 22, 0)
        };
        ui.label(text_r, label, Font::Bold, t.small_size + 1.0, color, align);
        if active {
            let icon = if f.ascending { Icon::ChevronUp } else { Icon::ChevronDown };
            let tw = ui.measure(label, Font::Bold, t.small_size + 1.0) as i32;
            let ix = if align == Align::Right { cr.right() - 20 } else { text_r.x + tw + 4 };
            ui.icon(Rect::new(ix, cr.y, 14, cr.h), icon, 11.0, t.accent_hover);
        }
        if resp.clicked {
            if f.sort == key {
                f.ascending = !f.ascending;
            } else {
                f.sort = key;
                f.ascending = key == SortKey::Name || key == SortKey::Kind;
            }
            f.rebuild_view();
        }
    }
}

struct Columns {
    name: Rect,
    size: Rect,
    kind: Rect,
    modified: Rect,
}

fn columns(r: Rect) -> Columns {
    let inner = r.inset(8, 0, 14, 0);
    let wide = inner.w >= 620;
    let (rest, modified) = inner.split_right(if inner.w >= 480 { 158 } else { 0 });
    let (rest, kind) = rest.split_right(if wide { 140 } else { 0 });
    let (name, size) = rest.split_right(96);
    Columns { name, size, kind, modified }
}

/// Draws one list row or grid tile and handles its mouse input. Returns
/// `true` if the pointer pressed on it.
fn item(
    f: &mut Files,
    ui: &mut Ui,
    vi: usize,
    r: Rect,
    grid: bool,
    typing: &mut bool,
    menu_at: &mut Option<(i32, i32)>,
) -> bool {
    let t = ui.theme().clone();
    let Some(e) = f.entry(vi).cloned() else { return false };
    let path = f.path_of(&e.name);
    let inner = if grid { r.inset(4, 4, 4, 4) } else { r.inset(6, 1, 6, 1) };
    let id = ui.id(&e.name) ^ if grid { 0x9e1d } else { 0x7105 };
    let resp = ui.interact(id, inner);
    let renaming = f.rename.as_ref().is_some_and(|rn| rn.original == e.name);
    let mut hit = false;
    if resp.pressed && !renaming {
        hit = true;
        if f.rename.is_some() {
            f.commit_rename();
        }
        let plain = !ui.input.ctrl() && !ui.input.shift();
        if ui.input.ctrl() {
            f.toggle(vi);
        } else if ui.input.shift() {
            f.select_range(vi);
        } else if f.selected.contains(&e.name) && f.selected.len() > 1 {
            // Keep the selection for a possible drag; collapse on release.
            f.cursor = Some(vi);
            f.pending_single = Some(vi);
        } else {
            f.select_only(vi);
        }
        if resp.double_clicked && plain {
            f.drag = None;
            f.pending_single = None;
            f.open_index(vi);
            return true;
        }
        if plain && let Some(origin) = ui.input.pointer {
            f.drag = Some(Drag { paths: f.selected_paths(), origin, active: false });
        }
    }
    // While dragging, folders (other than the dragged items) accept drops.
    if e.is_dir && f.drag.as_ref().is_some_and(|d| d.active && !d.paths.contains(&path)) && ui.hovered(inner) {
        f.drop_target = Some(path.clone());
        ui.canvas.fill_rounded_rect(inner, if grid { t.radius_large } else { t.radius }, t.accent.with_alpha(40));
        ui.canvas.stroke_rounded_rect(inner, if grid { t.radius_large } else { t.radius }, 1.5, t.accent);
    }
    if resp.right_clicked {
        hit = true;
        if !f.selected.contains(&e.name) {
            f.select_only(vi);
        }
        f.cursor = Some(vi);
        *menu_at = ui.input.pointer;
    }
    let selected = f.selected.contains(&e.name);
    let list_focused = !*typing && f.path_edit.is_none();
    if selected {
        let c = if list_focused { t.selection } else { Color::rgba(255, 255, 255, 24) };
        ui.canvas.fill_rounded_rect(inner, if grid { t.radius_large } else { t.radius }, c);
    } else if resp.hovered {
        ui.canvas.fill_rounded_rect(
            inner,
            if grid { t.radius_large } else { t.radius },
            Color::rgba(255, 255, 255, 10),
        );
    }
    if f.cursor == Some(vi) && list_focused && f.selected.len() > 1 {
        ui.canvas.stroke_rounded_rect(inner, if grid { t.radius_large } else { t.radius }, 1.0, t.accent.fade(0.6));
    }
    let cut = f.clipboard.as_ref().is_some_and(|(paths, cut)| *cut && paths.contains(&path));
    let op = if cut { 0.45 } else { 1.0 };
    let color = icons::kind_color(&e.name, e.is_dir).fade(op);
    if grid {
        let icon_r = Rect::new(inner.x + (inner.w - 76) / 2, inner.y + 8, 76, 64);
        let mut drawn = false;
        if !e.is_dir
            && file_kind(&e.name) == FileKind::Image
            && let Some(bmp) = f.thumbs.as_mut().and_then(|th| th.get(&path))
        {
            icons::draw_thumbnail(ui, icon_r.inset(2, 2, 2, 2), bmp, op);
            drawn = true;
        }
        if !drawn {
            icons::draw_large(ui, Rect::new(icon_r.x + 6, icon_r.y, 64, 64), &e.name, e.is_dir, op);
        }
        let name_r = Rect::new(inner.x + 4, icon_r.bottom() + 6, inner.w - 8, 40);
        if renaming {
            rename_box(f, ui, Rect::new(inner.x - 2, name_r.y, inner.w + 4, 30), typing);
        } else {
            draw_wrapped_name(ui, name_r, &e.name, if cut { t.text_dim } else { t.text });
        }
    } else {
        let cols = columns(r);
        let small = icons::small_icon(&e.name, e.is_dir);
        small.draw(&mut ui.canvas, Rect::new(cols.name.x + 12, r.y, 20, r.h), 18.0, color);
        let name_r = Rect::new(cols.name.x + 44, r.y, cols.name.w - 48, r.h);
        if renaming {
            rename_box(f, ui, Rect::new(name_r.x - 8, r.y + 2, name_r.w + 8, r.h - 4), typing);
        } else {
            let tc = if cut { t.text_dim } else { t.text };
            ui.label(name_r, &e.name, Font::Regular, t.font_size, tc, Align::Left);
        }
        let size = if e.is_dir {
            match e.size {
                0 => "Empty".to_string(),
                1 => "1 item".to_string(),
                n => format!("{n} items"),
            }
        } else {
            human_size(e.size)
        };
        let dim = t.text_dim.fade(op);
        ui.label(cols.size.inset(4, 0, 8, 0), &size, Font::Regular, t.font_size - 1.0, dim, Align::Right);
        if cols.kind.w > 0 {
            ui.label(
                cols.kind.inset(8, 0, 6, 0),
                &kind_name(&e.name, e.is_dir),
                Font::Regular,
                t.font_size - 1.0,
                dim,
                Align::Left,
            );
        }
        if cols.modified.w > 0 {
            ui.label(
                cols.modified.inset(8, 0, 4, 0),
                &friendly_time(e.modified),
                Font::Regular,
                t.font_size - 1.0,
                dim,
                Align::Left,
            );
        }
    }
    if resp.hovered && !renaming && grid && !e.is_dir {
        let tip = format!("{} · {} · {}", e.name, kind_name(&e.name, false), human_size(e.size));
        ui.tooltip(id, inner, &tip);
    }
    hit
}

/// Draws a name centred in up to two lines.
fn draw_wrapped_name(ui: &mut Ui, r: Rect, name: &str, color: Color) {
    let t = ui.theme().clone();
    let size = t.font_size - 1.0;
    let font = ui.ctx.font(Font::Regular);
    let lines = ui.ctx.text.wrap(font, size, name, r.w as f32);
    let line_h = 18;
    match lines.len() {
        0 => {}
        1 => ui.label(Rect::new(r.x, r.y, r.w, line_h), name, Font::Regular, size, color, Align::Center),
        _ => {
            let first = name[lines[0].clone()].trim_end();
            let rest = name[lines[1].start..].trim();
            ui.label(Rect::new(r.x, r.y, r.w, line_h), first, Font::Regular, size, color, Align::Center);
            ui.label(Rect::new(r.x, r.y + line_h, r.w, line_h), rest, Font::Regular, size, color, Align::Center);
        }
    }
}

/// The inline rename text box.
fn rename_box(f: &mut Files, ui: &mut Ui, r: Rect, typing: &mut bool) {
    let Some(mut rn) = f.rename.take() else { return };
    if !rn.focused {
        rn.focused = true;
        // Select the name without its extension.
        let stem = match rn.text.rfind('.') {
            Some(i) if i > 0 && !f.fs.is_dir(&f.path_of(&rn.original)) => i,
            _ => rn.text.len(),
        };
        focus_text(ui, "rename", (0, stem));
    }
    let resp = ui.text_input(r, "rename", &mut rn.text, "Name");
    *typing = true;
    if resp.submitted {
        f.rename = Some(rn);
        f.commit_rename();
        ui.state.focus = None;
    } else if !resp.focused && rn.focused {
        // Esc (focus dropped) cancels; a click elsewhere commits.
        if ui.input.pressed[0] {
            f.rename = Some(rn);
            f.commit_rename();
        }
    } else {
        f.rename = Some(rn);
    }
}

// ---- status bar ------------------------------------------------------------

fn draw_status(f: &mut Files, ui: &mut Ui, r: Rect) {
    let t = ui.theme().clone();
    ui.canvas.fill_rect(r, t.surface);
    ui.canvas.fill_rect(Rect::new(r.x, r.y, r.w, 1), t.border);
    let plural = |n: usize| if n == 1 { "1 item".to_string() } else { format!("{n} items") };
    let total = f.entries.iter().filter(|e| f.show_hidden || !e.name.starts_with('.')).count();
    let mut left =
        if f.search.is_empty() { plural(f.view.len()) } else { format!("{} of {}", f.view.len(), plural(total)) };
    if !f.selected.is_empty() {
        let bytes: u64 = f.entries.iter().filter(|e| f.selected.contains(&e.name) && !e.is_dir).map(|e| e.size).sum();
        let any_files = f.entries.iter().any(|e| f.selected.contains(&e.name) && !e.is_dir);
        left.push_str(&format!("  ·  {} selected", f.selected.len()));
        if any_files {
            left.push_str(&format!(" ({})", human_size(bytes)));
        }
    }
    ui.label(Rect::new(r.x + 14, r.y, r.w / 2, r.h), &left, Font::Regular, t.small_size, t.text_dim, Align::Left);
    let mut right = r.right() - 14;
    let status = if f.read_only() { Some(("Read-only", Icon::Lock)) } else { None };
    if let Some((label, icon)) = status {
        let w = ui.measure(label, Font::Regular, t.small_size) as i32 + 4;
        ui.label(Rect::new(right - w, r.y, w, r.h), label, Font::Regular, t.small_size, t.warning, Align::Right);
        ui.icon(Rect::new(right - w - 20, r.y, 16, r.h), icon, 13.0, t.warning);
        right -= w + 34;
    } else if let Some(s) = f.space.filter(|s| s.total > 0) {
        // "12.3 MiB free of 32.0 MiB" with a small meter; warns when low.
        let free = s.total.saturating_sub(s.used);
        let low = free < s.total / 10;
        let label = format!("{} free of {}", human_size(free), human_size(s.total));
        let w = ui.measure(&label, Font::Regular, t.small_size) as i32 + 4;
        let color = if low { t.warning } else { t.text_dim };
        ui.label(Rect::new(right - w, r.y, w, r.h), &label, Font::Regular, t.small_size, color, Align::Right);
        let meter = Rect::new(right - w - 62, r.y + (r.h - 6) / 2, 50, 6);
        ui.canvas.fill_rounded_rect(meter, 3.0, t.control_hover);
        let used_w = ((meter.w as u128 * s.used.min(s.total) as u128) / s.total as u128) as i32;
        if used_w > 0 {
            let fill = Rect::new(meter.x, meter.y, used_w.max(6), meter.h);
            ui.canvas.fill_rounded_rect(fill, 3.0, if low { t.warning } else { t.accent });
        }
        let area = Rect::new(meter.x, r.y, right - meter.x, r.h);
        if ui.hovered(area) {
            let what = if s.persistent {
                format!("Disk: {} used of {}", human_size(s.used), human_size(s.total))
            } else {
                "In memory: not kept after a restart".to_string()
            };
            let id = ui.id("space-tip");
            ui.tooltip(id, area, &what);
        }
        right = meter.x - 24;
    }
    if let Some((paths, cut)) = &f.clipboard {
        let label = format!("{} {} — paste with Ctrl+V", plural(paths.len()), if *cut { "cut" } else { "copied" });
        let w = ui.measure(&label, Font::Regular, t.small_size) as i32 + 4;
        if right - w > r.x + r.w / 2 {
            ui.label(
                Rect::new(right - w, r.y, w, r.h),
                &label,
                Font::Regular,
                t.small_size,
                t.text_faint,
                Align::Right,
            );
        }
    }
}

// ---- context menus ---------------------------------------------------------

fn context_menus(f: &mut Files, ui: &mut Ui) {
    let ro = f.read_only();
    let can_paste = f.clipboard.is_some() && !ro;
    // Item menu.
    let sel = f.selected_paths();
    let single = sel.len() == 1;
    let single_dir = single && f.fs.is_dir(&sel[0]);
    let mut items: Vec<(MenuItem, Option<Action>)> =
        vec![(MenuItem::new("Open").shortcut("Enter"), Some(Action::Open))];
    if single && !single_dir {
        items.push((MenuItem::new("Open in Text Editor"), Some(Action::OpenInEditor)));
    }
    if single_dir {
        items.push((MenuItem::new("Open in Terminal"), Some(Action::OpenTerminal)));
    }
    items.push((MenuItem::separator(), None));
    items.push((MenuItem::new("Cut").shortcut("Ctrl+X").enabled(!ro), Some(Action::Cut)));
    items.push((MenuItem::new("Copy").shortcut("Ctrl+C"), Some(Action::Copy)));
    items.push((MenuItem::new("Paste").shortcut("Ctrl+V").enabled(can_paste), Some(Action::Paste)));
    items.push((MenuItem::separator(), None));
    items.push((MenuItem::new("Rename").shortcut("F2").enabled(single && !ro), Some(Action::Rename)));
    items.push((MenuItem::new("Delete").shortcut("Del").enabled(!ro), Some(Action::Delete)));
    items.push((MenuItem::separator(), None));
    items.push((MenuItem::new("Copy path"), Some(Action::CopyPath)));
    items.push((MenuItem::new("Properties").enabled(single), Some(Action::Properties)));
    let menu: Vec<MenuItem> = items.iter().map(|(m, _)| m.clone()).collect();
    if let Some(i) = ui.context_menu("item-menu", &menu)
        && let Some(a) = items[i].1
    {
        run_action(f, ui, a);
    }

    // Background menu.
    let bg: Vec<(MenuItem, Option<Action>)> = vec![
        (MenuItem::new("New folder").shortcut("Ctrl+Shift+N").enabled(!ro), Some(Action::NewFolder)),
        (MenuItem::new("New text document").shortcut("Ctrl+N").enabled(!ro), Some(Action::NewDocument)),
        (MenuItem::separator(), None),
        (MenuItem::new("Paste").shortcut("Ctrl+V").enabled(can_paste), Some(Action::Paste)),
        (MenuItem::new("Select all").shortcut("Ctrl+A"), Some(Action::SelectAll)),
        (MenuItem::separator(), None),
        (
            MenuItem::new("List view").shortcut("Ctrl+1").checked(f.mode == ViewMode::List),
            Some(Action::Mode(ViewMode::List)),
        ),
        (
            MenuItem::new("Icon view").shortcut("Ctrl+2").checked(f.mode == ViewMode::Grid),
            Some(Action::Mode(ViewMode::Grid)),
        ),
        (MenuItem::new("Show hidden files").shortcut("Ctrl+H").checked(f.show_hidden), Some(Action::ToggleHidden)),
        (MenuItem::new("Refresh").shortcut("F5"), Some(Action::Refresh)),
        (MenuItem::separator(), None),
        (MenuItem::new("Open in Terminal").shortcut("Ctrl+T"), Some(Action::OpenTerminalHere)),
        (MenuItem::new("Properties"), Some(Action::FolderProperties)),
    ];
    let menu: Vec<MenuItem> = bg.iter().map(|(m, _)| m.clone()).collect();
    if let Some(i) = ui.context_menu("background-menu", &menu)
        && let Some(a) = bg[i].1
    {
        run_action(f, ui, a);
    }

    // Sort menu (from the toolbar button).
    let sorts: Vec<(MenuItem, Option<Action>)> = vec![
        (MenuItem::new("Name").checked(f.sort == SortKey::Name), Some(Action::Sort(SortKey::Name))),
        (MenuItem::new("Size").checked(f.sort == SortKey::Size), Some(Action::Sort(SortKey::Size))),
        (MenuItem::new("Type").checked(f.sort == SortKey::Kind), Some(Action::Sort(SortKey::Kind))),
        (MenuItem::new("Date modified").checked(f.sort == SortKey::Modified), Some(Action::Sort(SortKey::Modified))),
        (MenuItem::separator(), None),
        (MenuItem::new("Ascending").checked(f.ascending), Some(Action::Ascending(true))),
        (MenuItem::new("Descending").checked(!f.ascending), Some(Action::Ascending(false))),
    ];
    let menu: Vec<MenuItem> = sorts.iter().map(|(m, _)| m.clone()).collect();
    if let Some(i) = ui.context_menu("sort-menu", &menu)
        && let Some(a) = sorts[i].1
    {
        run_action(f, ui, a);
    }

    // Sidebar place menu.
    let place = [MenuItem::new("Open"), MenuItem::new("Open in Terminal"), MenuItem::new("Properties")];
    if let Some(i) = ui.context_menu("place-menu", &place)
        && let Some(p) = f.context_target.take()
    {
        match i {
            0 => {
                f.navigate(&p, true);
            }
            1 => f.open_terminal(&p),
            _ => f.show_properties(&p),
        }
    }
}

fn run_action(f: &mut Files, ui: &mut Ui, a: Action) {
    let sel = f.selected_paths();
    match a {
        Action::Open => f.open_selection(),
        Action::OpenInEditor => {
            if let Some(p) = sel.first() {
                f.launch(vfiles::kind::EDITOR.exe, vec![p.clone()]);
            }
        }
        Action::OpenTerminal => {
            if let Some(p) = sel.first() {
                f.open_terminal(&p.clone());
            }
        }
        Action::OpenTerminalHere => {
            let d = f.cwd.clone();
            f.open_terminal(&d);
        }
        Action::Cut => f.copy_selection(true),
        Action::Copy => f.copy_selection(false),
        Action::Paste => f.paste(),
        Action::Rename => f.start_rename(),
        Action::Delete => f.ask_delete(),
        Action::CopyPath => {
            let text = if sel.is_empty() { f.cwd.clone() } else { sel.join("\n") };
            ui.set_clipboard(&text);
        }
        Action::Properties => {
            if let Some(p) = sel.first() {
                f.show_properties(&p.clone());
            }
        }
        Action::FolderProperties => {
            let d = f.cwd.clone();
            f.show_properties(&d);
        }
        Action::NewFolder => f.new_folder(),
        Action::NewDocument => f.new_document(),
        Action::SelectAll => f.select_all(),
        Action::ToggleHidden => {
            f.show_hidden = !f.show_hidden;
            f.rebuild_view();
        }
        Action::Refresh => f.reload(),
        Action::Sort(k) => {
            f.sort = k;
            f.rebuild_view();
        }
        Action::Ascending(asc) => {
            f.ascending = asc;
            f.rebuild_view();
        }
        Action::Mode(m) => {
            f.mode = m;
            f.reveal_cursor = true;
        }
    }
}

// ---- properties ------------------------------------------------------------

/// Draws the Properties dialog; returns `true` when it is closed.
pub fn properties_dialog(ui: &mut Ui, p: &Props) -> bool {
    let mut closed = ui.input.key(vproto::input::keys::ESC) || ui.input.key(vproto::input::keys::ENTER);
    let rows: Vec<(&str, String)> = {
        let mut v = vec![("Type", p.kind.clone()), ("Location", display_path(parent(&p.path)))];
        v.push(("Size", p.size.clone()));
        if let Some(c) = &p.contents {
            v.push(("Contains", c.clone()));
        }
        v.push(("Modified", p.modified.clone()));
        v.push((
            "Access",
            if p.read_only { "Read-only (system image)".to_string() } else { "Read and write".to_string() },
        ));
        v
    };
    let h = 150 + rows.len() as i32 * 30;
    let clicked = ui.modal(460, h, |ui, card| {
        let t = ui.theme().clone();
        let inner = card.inset(24, 22, 24, 20);
        let icon_r = Rect::new(inner.x, inner.y, 52, 52);
        if p.is_dir {
            icons::draw_folder(ui, icon_r, 1.0);
        } else {
            icons::draw_page(ui, icon_r, &p.name, 1.0);
        }
        ui.label(
            Rect::new(inner.x + 68, inner.y + 2, inner.w - 68, 28),
            &p.name,
            Font::Bold,
            t.heading_size,
            t.text,
            Align::Left,
        );
        ui.label(
            Rect::new(inner.x + 68, inner.y + 28, inner.w - 68, 22),
            &p.path,
            Font::Regular,
            t.small_size,
            t.text_faint,
            Align::Left,
        );
        let mut y = inner.y + 70;
        ui.separator(inner.x, y - 8, inner.w);
        for (k, v) in &rows {
            ui.label(Rect::new(inner.x, y, 100, 26), k, Font::Regular, t.font_size, t.text_dim, Align::Left);
            ui.label(
                Rect::new(inner.x + 104, y, inner.w - 104, 26),
                v,
                Font::Regular,
                t.font_size,
                t.text,
                Align::Left,
            );
            y += 30;
        }
        ui.primary_button(Rect::new(inner.right() - 100, inner.bottom() - 36, 100, 36), "Close")
    });
    closed |= clicked;
    closed
}
