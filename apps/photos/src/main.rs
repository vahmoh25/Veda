//! Photos: browse the pictures in a folder and look at them.
//!
//! * The library ([`library`]) shows a grid of thumbnails of `~/Pictures`, of a folder given
//!   on the command line, or of the folder of a picture given there (which then opens
//!   directly).
//! * The viewer ([`viewer`]) shows one picture: fitted to the window or zoomed (wheel, `+`/`-`,
//!   `0`, `1`) and panned by dragging, rotated with `R`, with a filmstrip, an info panel, a
//!   full-screen mode (`F11` or a double-click) and a full-screen slideshow (`S`). It can make
//!   the picture the wallpaper.
//! * [`agent`] lets the voice agent do the same.
//!
//! Files are read and decoded on background threads ([`loader`]); the UI thread only scales
//! ([`render`]) and draws, so it stays responsive under emulation.

#![no_std]
#![no_main]

extern crate alloc;

mod agent;
mod catalog;
mod library;
mod loader;
mod render;
mod viewer;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::RawHandle;
use vgfx::{Align, Color, Rect, ShadowTemplate};
use vproto::shell::{ShellLink, ShellReply};
use vproto::vfs;
use vui::{App, Font, Icon, Ui, WindowSpec, WindowState};

use crate::catalog::{Entry, Thumb};
use crate::loader::{Done, Loader, Picture};
use crate::render::ViewCache;
use crate::viewer::{Slideshow, View};

vrt::entry!(main);

/// At most this many pictures keep their thumbnails in memory (about 0.6 MB each).
const MAX_THUMBS: usize = 240;
/// The window's size when it opens.
const WINDOW_SIZE: (i32, i32) = (1100, 640);

/// What the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Library,
    Viewer,
}

/// A full picture requested from the loader.
pub(crate) enum Slot {
    Loading,
    Ready(Arc<Picture>),
    Failed(String),
}

/// A short message at the bottom of the window.
pub(crate) struct Toast {
    text: String,
    icon: Icon,
    shown_at: u64,
    until: u64,
}

/// The application: the folder, the picture on screen and everything about the window.
pub(crate) struct Photos {
    /// The folder shown in the library.
    pub dir: String,
    pub entries: Vec<Entry>,
    /// Why the folder could not be listed.
    pub list_error: Option<String>,
    pub mode: Mode,
    /// The selected picture (library) or the picture on screen (viewer).
    pub current: usize,
    /// The selection was moved with the keyboard (it is then outlined).
    pub keyboard_nav: bool,
    /// Scroll the library so that the selection is visible.
    pub reveal_selection: bool,
    pub loader: Option<Loader>,
    shell: Option<ShellLink>,
    vfs: Option<vfs::Client>,
    /// Decoded pictures: the one on screen and its neighbours.
    pub pictures: Vec<(String, Slot)>,
    pub view: View,
    pub cache: ViewCache,
    pub show_info: bool,
    pub fullscreen: bool,
    /// Entering or leaving full screen asked for between frames (it needs the window, so it
    /// happens at the next frame).
    pub pending_fullscreen: Option<bool>,
    /// The viewer moves on by itself.
    pub slideshow: Option<Slideshow>,
    /// Return to a maximised window when leaving full screen.
    restore_maximized: bool,
    last_size: (i32, i32),
    last_pointer: Option<(i32, i32)>,
    /// When the pointer last moved (full-screen controls hide after a while).
    pub pointer_moved_at: u64,
    pub wallpaper_busy: bool,
    toast: Option<Toast>,
    /// Shadow under hovered cards.
    pub card_shadow: ShadowTemplate,
}

impl Photos {
    fn new(dir: String) -> Photos {
        Photos {
            dir,
            entries: Vec::new(),
            list_error: None,
            mode: Mode::Library,
            current: 0,
            keyboard_nav: false,
            reveal_selection: false,
            loader: Loader::start(),
            shell: ShellLink::start(),
            vfs: vproto::connect(vfs::NAME).ok().map(vfs::Client::new),
            pictures: Vec::new(),
            view: View::default(),
            cache: ViewCache::new(),
            show_info: false,
            fullscreen: false,
            pending_fullscreen: None,
            slideshow: None,
            restore_maximized: false,
            last_size: (0, 0),
            last_pointer: None,
            pointer_moved_at: 0,
            wallpaper_busy: false,
            toast: None,
            card_shadow: ShadowTemplate::new(10, 16),
        }
    }

    /// Lists the folder again, keeping thumbnails of files that are still there.
    pub fn reload(&mut self) {
        let Some(vfs) = &self.vfs else {
            self.list_error = Some("The file system is not available.".into());
            return;
        };
        match catalog::list(vfs, &self.dir) {
            Ok(mut fresh) => {
                let selected = self.entries.get(self.current).map(|e| e.path.clone());
                for e in fresh.iter_mut() {
                    if let Some(old) = self.entries.iter_mut().find(|o| o.path == e.path && o.size == e.size) {
                        e.meta = old.meta.take();
                        e.thumb = core::mem::replace(&mut old.thumb, Thumb::Missing);
                    }
                }
                // Thumbnails still queued for files that went away are not wanted any more.
                if let Some(loader) = &self.loader {
                    for path in loader.cancel_thumbs() {
                        if let Some(e) = fresh.iter_mut().find(|e| e.path == path) {
                            e.thumb = Thumb::Missing;
                        }
                    }
                }
                self.entries = fresh;
                self.current = selected.and_then(|p| self.entries.iter().position(|e| e.path == p)).unwrap_or(0);
                self.list_error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.list_error = Some(e);
            }
        }
    }

    /// Shows the pictures of the folder `dir` in the library (the viewer closes); the same
    /// folder is listed again.
    pub fn set_folder(&mut self, dir: &str) {
        if self.mode == Mode::Viewer {
            self.close_viewer();
        }
        if dir != self.dir {
            if let Some(l) = &self.loader {
                l.cancel_thumbs();
            }
            self.dir = dir.to_string();
            self.entries.clear();
            self.current = 0;
            self.keyboard_nav = false;
        }
        self.reload();
        self.reveal_selection = true;
        vrt::println!("showing {} pictures in {}", self.entries.len(), self.dir);
    }

    /// Opens picture `index` in the viewer.
    pub fn open(&mut self, index: usize) {
        if index < self.entries.len() {
            self.mode = Mode::Viewer;
            self.go_to(index);
        }
    }

    /// Returns to the library with the current picture selected.
    pub fn close_viewer(&mut self) {
        self.stop_slideshow();
        self.mode = Mode::Library;
        self.reveal_selection = true;
        self.view = View::default();
        self.cache.clear();
        self.pictures.clear();
        if let Some(l) = &self.loader {
            l.retain_pictures(&[]);
        }
    }

    /// Shows picture `index` in the viewer.
    pub fn go_to(&mut self, index: usize) {
        if index >= self.entries.len() {
            return;
        }
        if index != self.current {
            self.view = View::default();
        }
        self.current = index;
        // Keep the picture and its neighbours (decoded or queued); forget the rest.
        let keep = self.neighbourhood();
        self.pictures.retain(|(p, _)| keep.contains(p));
        if let Some(l) = &self.loader {
            l.retain_pictures(&keep);
        }
        let path = self.entries[index].path.clone();
        let queued = self.pictures.iter().any(|(p, s)| *p == path && matches!(s, Slot::Loading));
        match &self.loader {
            // A prefetch of this picture becomes urgent.
            Some(l) if queued => l.request_picture(&path, true),
            _ => self.want_picture(index, true),
        }
    }

    /// Paths of the picture on screen and its neighbours.
    fn neighbourhood(&self) -> Vec<String> {
        let n = self.entries.len();
        let mut out = Vec::new();
        for d in [0isize, 1, -1] {
            let i = self.current as isize + d;
            if i >= 0 && (i as usize) < n {
                out.push(self.entries[i as usize].path.clone());
            }
        }
        out
    }

    /// Makes sure picture `index` is decoded, being decoded or queued.
    pub fn want_picture(&mut self, index: usize, urgent: bool) {
        let Some(path) = self.entries.get(index).map(|e| e.path.clone()) else { return };
        if self.pictures.iter().any(|(p, _)| *p == path) {
            return;
        }
        match &self.loader {
            Some(l) => {
                l.request_picture(&path, urgent);
                self.pictures.push((path, Slot::Loading));
            }
            None => self.pictures.push((path, Slot::Failed("Photos could not start its decoder.".into()))),
        }
    }

    /// Decodes the picture on screen again (after an error).
    pub fn retry(&mut self) {
        if let Some(path) = self.entries.get(self.current).map(|e| e.path.clone()) {
            self.pictures.retain(|(p, _)| *p != path);
            if let Some(e) = self.entries.get_mut(self.current)
                && matches!(e.thumb, Thumb::Failed(_))
            {
                e.thumb = Thumb::Missing;
            }
            self.want_picture(self.current, true);
        }
    }

    /// Prefetches the neighbours once the picture on screen is done.
    pub fn prefetch(&mut self) {
        let Some(path) = self.entries.get(self.current).map(|e| e.path.clone()) else { return };
        let done = self.pictures.iter().any(|(p, s)| *p == path && !matches!(s, Slot::Loading));
        if done {
            for d in [1isize, -1] {
                let i = self.current as isize + d;
                if i >= 0 && (i as usize) < self.entries.len() {
                    self.want_picture(i as usize, false);
                }
            }
        }
    }

    /// Asks for the thumbnails of entry `index` if nobody has yet.
    pub fn want_thumbs(&mut self, index: usize, first: bool) {
        let Some(loader) = &self.loader else { return };
        if let Some(e) = self.entries.get_mut(index)
            && matches!(e.thumb, Thumb::Missing)
        {
            loader.request_thumbs(&e.path, first);
            e.thumb = Thumb::Requested;
        }
    }

    /// Collects finished work from the background threads.
    fn absorb(&mut self) {
        let mut evict = false;
        if let Some(loader) = &self.loader {
            for done in loader.take() {
                match done {
                    Done::Thumbs { path, result, meta } => {
                        if let Some(e) = self.entries.iter_mut().find(|e| e.path == path) {
                            if meta.is_some() {
                                e.meta = meta;
                            }
                            e.thumb = match result {
                                Ok(t) => Thumb::Ready(t),
                                Err(m) => Thumb::Failed(m),
                            };
                        }
                        evict = true;
                    }
                    Done::Picture { path, result, meta } => {
                        if let Some(e) = self.entries.iter_mut().find(|e| e.path == path)
                            && e.meta.is_none()
                        {
                            e.meta = meta;
                        }
                        // Pictures nobody waits for any more are dropped.
                        if let Some((_, slot)) = self.pictures.iter_mut().find(|(p, _)| *p == path) {
                            *slot = match result {
                                Ok(p) => Slot::Ready(p),
                                Err(m) => Slot::Failed(m),
                            };
                        }
                    }
                }
            }
        }
        if evict {
            self.evict_thumbs();
        }
        let replies = self.shell.as_ref().map(|s| s.take()).unwrap_or_default();
        for reply in replies {
            let ShellReply::WallpaperSet { result, .. } = reply else { continue };
            self.wallpaper_busy = false;
            match result {
                Ok(()) => self.show_toast("Wallpaper changed", Icon::Check),
                Err(e) => self.show_toast(&e.to_string(), Icon::Warning),
            }
        }
    }

    /// Drops the least recently drawn thumbnails beyond [`MAX_THUMBS`] (they are made again
    /// when they come back into view).
    fn evict_thumbs(&mut self) {
        let mut ready: Vec<usize> =
            (0..self.entries.len()).filter(|&i| matches!(self.entries[i].thumb, Thumb::Ready(_))).collect();
        if ready.len() <= MAX_THUMBS {
            return;
        }
        ready.sort_by_key(|&i| self.entries[i].used);
        let excess = ready.len() - MAX_THUMBS;
        for &i in &ready[..excess] {
            self.entries[i].thumb = Thumb::Missing;
        }
    }

    /// Asks the shell to use the picture on screen as the wallpaper.
    pub fn set_wallpaper(&mut self) {
        let Some(path) = self.entries.get(self.current).map(|e| e.path.clone()) else { return };
        match &self.shell {
            Some(shell) if !self.wallpaper_busy => {
                shell.set_wallpaper(&path);
                self.wallpaper_busy = true;
                self.show_toast("Setting the wallpaper…", Icon::Wallpaper);
                // Stays up until the shell answers.
                if let Some(t) = &mut self.toast {
                    t.until = t.shown_at + 60_000_000_000;
                }
            }
            Some(_) => {}
            None => self.show_toast("The desktop is not available.", Icon::Warning),
        }
    }

    pub fn show_toast(&mut self, text: &str, icon: Icon) {
        let now = vrt::time::now_ns();
        self.toast = Some(Toast { text: text.to_string(), icon, shown_at: now, until: now + 2_800_000_000 });
    }

    /// Enters or leaves full screen (leaving it ends a slideshow).
    pub fn set_fullscreen(&mut self, ui: &mut Ui, on: bool) {
        if !on {
            self.stop_slideshow();
        }
        if on == self.fullscreen {
            return;
        }
        if on {
            self.restore_maximized = ui.window_state() == WindowState::Maximized;
            ui.set_window_state(WindowState::Fullscreen);
        } else {
            ui.set_window_state(if self.restore_maximized { WindowState::Maximized } else { WindowState::Normal });
        }
        self.fullscreen = on;
        self.pointer_moved_at = ui.now();
        self.view.moved_at = ui.now();
    }

    /// Notices size changes (and full screen being left by other means).
    fn track_window(&mut self, ui: &mut Ui) {
        let size = (ui.width, ui.height);
        if size == self.last_size {
            return;
        }
        let first = self.last_size == (0, 0);
        self.last_size = size;
        if !first {
            // Drafts while the window is being resized.
            self.view.moved_at = ui.now();
            self.fullscreen = ui.window_state() == WindowState::Fullscreen;
        }
    }

    fn track_pointer(&mut self, ui: &Ui) {
        if ui.input.pointer != self.last_pointer {
            self.last_pointer = ui.input.pointer;
            self.pointer_moved_at = ui.now();
        }
    }

    fn sync_title(&mut self, ui: &mut Ui) {
        let title = match (self.mode, self.entries.get(self.current)) {
            (Mode::Viewer, Some(e)) => format!("{} – Photos", e.name),
            _ => "Photos".to_string(),
        };
        ui.set_title(&title);
    }

    fn draw_toast(&mut self, ui: &mut Ui) {
        let Some(toast) = &self.toast else { return };
        let now = ui.now();
        if now >= toast.until {
            self.toast = None;
            return;
        }
        let t = ui.theme().clone();
        let appear = ((now - toast.shown_at) as f32 / 160_000_000.0).min(1.0);
        let vanish = ((toast.until - now) as f32 / 320_000_000.0).min(1.0);
        let a = appear.min(vanish);
        if a < 1.0 {
            ui.repaint_at(now + 30_000_000);
        } else {
            ui.repaint_at(toast.until - 320_000_000);
        }
        let size = 14.0;
        let text_w = ui.measure(&toast.text, Font::Regular, size) as i32;
        let (w, h) = ((text_w + 74).min(ui.width - 24), 44);
        let above = if self.mode == Mode::Viewer && !self.fullscreen { viewer::STRIP_BAR_H + 20 } else { 28 };
        let y = ui.height - above - h + ((1.0 - a) * 10.0) as i32;
        let r = Rect::new((ui.width - w) / 2, y, w, h);
        ui.canvas.draw_shadow(r.translate(0, 4), 22, 18, Color::rgba(0, 0, 0, (120.0 * a) as u8));
        ui.canvas.fill_rounded_rect(r, 22.0, Color::hex(0x2D2D35).fade(a));
        ui.canvas.stroke_rounded_rect(r, 22.0, 1.0, t.border_strong.fade(a));
        let icon_color = match toast.icon {
            Icon::Warning => t.warning,
            Icon::Check => t.success,
            _ => t.accent,
        };
        let (icon, text) = (toast.icon, toast.text.clone());
        ui.icon(Rect::new(r.x + 16, r.y, 22, h), icon, 18.0, icon_color.fade(a));
        ui.label(Rect::new(r.x + 46, r.y, w - 62, h), &text, Font::Regular, size, t.text.fade(a), Align::Left);
    }
}

impl App for Photos {
    fn update(&mut self, ui: &mut Ui) {
        self.absorb();
        self.track_window(ui);
        self.track_pointer(ui);
        if let Some(on) = self.pending_fullscreen.take() {
            self.set_fullscreen(ui, on);
        }
        match self.mode {
            Mode::Library => self.library(ui),
            Mode::Viewer => self.viewer(ui),
        }
        self.draw_toast(ui);
        self.sync_title(ui);
    }

    fn wait_handles(&self) -> Vec<(RawHandle, u32)> {
        let mut handles = Vec::new();
        if let Some(l) = &self.loader {
            handles.push((l.event_handle(), vabi::signals::SIGNALED));
        }
        if let Some(s) = &self.shell {
            handles.push((s.event_handle(), vabi::signals::SIGNALED));
        }
        handles
    }

    fn handle_signaled(&mut self, _index: usize, _observed: u32) {
        self.absorb();
    }

    fn agent_info(&self) -> Option<vui::agent::AppAgentInfo> {
        Some(agent::info())
    }

    fn agent_state(&self) -> vui::agent::Value {
        agent::state(self)
    }

    fn agent_invoke(&mut self, action: &str, args: &vui::agent::Value) -> Result<vui::agent::Value, String> {
        Photos::agent_invoke(self, action, args)
    }
}

fn main() -> i32 {
    // `photos [PATH]`: a picture opens in the viewer with its folder in the library; a folder
    // opens in the library.
    let args = vrt::env::args();
    let arg = args.get(1).filter(|a| !a.is_empty()).cloned();
    let mut app = Photos::new(catalog::PICTURES.to_string());
    let mut file = None;
    if let Some(path) = arg {
        let is_dir = app.vfs.as_ref().and_then(|v| v.stat(path.clone()).ok()?.ok()).is_some_and(|s| s.is_dir);
        if is_dir {
            let trimmed = path.trim_end_matches('/');
            app.dir = if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() };
        } else {
            let (dir, name) = catalog::split(&path);
            app.dir = dir;
            file = Some((path, name));
        }
    }
    app.reload();
    if let Some((path, name)) = file {
        let index = match app.entries.iter().position(|e| e.name == name) {
            Some(i) => i,
            None => {
                // Not listed as a picture (e.g. an unusual extension): show it on its own.
                app.list_error = None;
                app.entries.push(Entry { name, path, size: 0, meta: None, thumb: Thumb::Missing, used: 0 });
                app.entries.len() - 1
            }
        };
        app.open(index);
    }
    vrt::println!("showing {} pictures in {}", app.entries.len(), app.dir);
    let mut spec = WindowSpec::new("Photos", WINDOW_SIZE.0 as u32, WINDOW_SIZE.1 as u32);
    spec.min_width = 560;
    spec.min_height = 380;
    spec.app_id = "photos".into();
    vui::run(spec, app)
}
