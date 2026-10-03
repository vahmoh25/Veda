//! Photos for the voice agent: what the window shows (the library of a folder, or the picture
//! in the viewer with its size, zoom and turn) and what it does (showing pictures and folders,
//! moving through them, zooming, turning, full screen, slideshows, the details panel and the
//! wallpaper), through the same operations as the mouse and the keyboard.
//!
//! Nothing here needs the user's consent: Photos never changes or deletes files (turning a
//! picture only turns the view), and the wallpaper is easily changed back.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use vfiles::kind::is_image;
use vfiles::path::file_stem;
use vproto::fs::Stat;
use vui::agent::{
    self, Action, AppAgentInfo, Value, arg_bool, arg_f64, arg_opt_str, arg_path, arg_str, object, show_path,
};

use crate::catalog::{self, Entry};
use crate::viewer::{SLIDE_NS, ZOOM_STEP};
use crate::{Mode, Photos, Slot};

/// The most picture names the agent gets at once.
const MAX_NAMES: usize = 60;

pub fn info() -> AppAgentInfo {
    agent::info(
        "Photos shows the pictures of a folder (~/Pictures unless asked otherwise) as a library of thumbnails, \
         and one at a time in a viewer that zooms and turns them, shows them full screen or as a slideshow, and \
         makes one the wallpaper.",
        vec![
            Action::new("show_picture", "Shows a picture in the viewer")
                .param(
                    "picture",
                    "string",
                    "A picture's name in the folder shown (such as Lighthouse.jpg, or just Lighthouse), or the path \
                     of a picture anywhere",
                    true,
                )
                .build(),
            Action::new("go", "Moves to another picture of the folder: the viewer shows it, the library selects it")
                .choice("to", "Which picture", true, &["next", "previous", "first", "last"])
                .build(),
            Action::new("show_library", "Goes back to the library of the folder's pictures, listed again").build(),
            Action::new("open_folder", "Shows the pictures of a folder in the library")
                .param("path", "string", "The folder, such as ~/Pictures or ~/Documents", true)
                .build(),
            Action::new("zoom", "Zooms the picture in the viewer")
                .choice(
                    "to",
                    "Fitted to the window, its actual size (100%), or a step in or out",
                    false,
                    &["fit", "actual_size", "in", "out"],
                )
                .param("percent", "number", "An exact zoom instead, in percent (from the fitted size to 1600)", false)
                .build(),
            Action::new(
                "rotate",
                "Turns the picture in the viewer a quarter turn (only on the screen: the file is not changed)",
            )
            .choice("direction", "Which way (clockwise by default)", false, &["clockwise", "anticlockwise"])
            .build(),
            Action::new("full_screen", "Shows the viewer on the whole screen, or back in its window")
                .param("on", "boolean", "true for full screen, false for the window", true)
                .build(),
            Action::new(
                "slideshow",
                "Starts a full-screen slideshow of the folder from the picture shown (in the library, the selected \
                 one), or stops it; any key the user presses also stops it",
            )
            .param("on", "boolean", "true to start, false to stop", true)
            .param("seconds", "number", "How long each picture stays, 2 to 60 (5 by default)", false)
            .build(),
            Action::new(
                "details",
                "Shows or hides the details panel beside the picture (its size in pixels, type, file size and folder)",
            )
            .param("show", "boolean", "true to show it, false to hide it", true)
            .build(),
            Action::new("set_as_wallpaper", "Makes a picture the desktop wallpaper")
                .param("picture", "string", "Its name or path (the picture shown if not given)", false)
                .build(),
        ],
    )
}

/// A zoom as the toolbar shows it.
fn percent(scale: f32) -> u32 {
    (scale * 100.0 + 0.5) as u32
}

/// The picture in the viewer.
fn picture(p: &Photos, e: &Entry) -> Value {
    let mut v = object! { "name" => e.name.as_str(), "path" => show_path(&e.path), "number" => p.current + 1 };
    match p.pictures.iter().find(|(path, _)| *path == e.path).map(|(_, s)| s) {
        Some(Slot::Ready(pic)) => {
            v.set("status", "shown");
            if pic.damaged {
                v.set("damaged", true);
            }
        }
        Some(Slot::Failed(why)) => {
            v.set("status", "failed");
            v.set("problem", why.as_str());
        }
        _ => {
            v.set("status", "loading");
        }
    }
    if let Some((w, h)) = p.dims() {
        v.set("width", w);
        v.set("height", h);
    }
    match &e.meta {
        Some(m) => {
            v.set("format", m.format.name());
            v.set("file_size", catalog::human_size(m.file_size));
        }
        None if e.size > 0 => {
            v.set("file_size", catalog::human_size(e.size));
        }
        None => {}
    }
    v
}

pub fn state(p: &Photos) -> Value {
    let names: Vec<&str> = p.entries.iter().take(MAX_NAMES).map(|e| e.name.as_str()).collect();
    let mut v = object! {
        "showing" => if p.mode == Mode::Viewer { "viewer" } else { "library" },
        "folder" => show_path(&p.dir),
        "picture_count" => p.entries.len(),
        "pictures" => names,
    };
    if p.entries.len() > MAX_NAMES {
        v.set("pictures_cut_short", true);
    }
    if let Some(e) = &p.list_error {
        v.set("error", e.as_str());
    }
    match (p.mode, p.entries.get(p.current)) {
        (Mode::Viewer, Some(e)) => {
            v.set("picture", picture(p, e));
            if let Some(d) = p.dims() {
                v.set("zoom_percent", percent(p.view.placement(d, p.viewport()).scale));
            }
            v.set("fitted", p.view.zoom.is_none());
            v.set("rotation", p.view.rot as u32 * 90);
            v.set("full_screen", p.pending_fullscreen.unwrap_or(p.fullscreen));
            v.set("details_panel", p.show_info);
            v.set("slideshow", p.slideshow.map(|s| object! { "seconds" => s.interval as f64 / 1e9 }));
        }
        (Mode::Library, Some(e)) if p.keyboard_nav => {
            v.set("selected", e.name.as_str());
        }
        _ => {}
    }
    if p.wallpaper_busy {
        v.set("wallpaper", "being changed");
    }
    if let Some(t) = &p.toast
        && vrt::time::now_ns() < t.until
    {
        v.set("message", t.text.as_str());
    }
    v
}

impl Photos {
    fn stat(&self, path: &str) -> Option<Stat> {
        self.vfs.as_ref().and_then(|v| v.stat(path.into()).ok()?.ok())
    }

    fn no_pictures(&self) -> String {
        format!("there are no pictures in {}", show_path(&self.dir))
    }

    /// The picture shown or selected, for results: `{key: name, "number": n, "of": count}`.
    fn position(&self, key: &str) -> Value {
        let name = self.entries.get(self.current).map_or("", |e| e.name.as_str());
        object! { key => name, "number" => self.current + 1, "of" => self.entries.len() }
    }

    /// The picture of the folder called `name`: exactly, else ignoring case, else without its
    /// extension, else the only one whose name contains it.
    fn find_picture(&self, name: &str) -> Option<usize> {
        if name.is_empty() {
            return None;
        }
        if let Some(i) = self.entries.iter().position(|e| e.name == name) {
            return Some(i);
        }
        let lower = name.to_lowercase();
        let only = |test: &dyn Fn(&Entry) -> bool| {
            let mut found = self.entries.iter().enumerate().filter(|(_, e)| test(e)).map(|(i, _)| i);
            match (found.next(), found.next()) {
                (Some(i), None) => Some(i),
                _ => None,
            }
        };
        only(&|e| e.name.to_lowercase() == lower)
            .or_else(|| only(&|e| file_stem(&e.name).to_lowercase() == lower))
            .or_else(|| only(&|e| e.name.to_lowercase().contains(&lower)))
    }

    /// The picture file at `path`, as when Photos is started with it: its folder is listed
    /// (becoming the folder shown); returns its index.
    fn picture_file(&mut self, path: &str) -> Result<usize, String> {
        match self.stat(path) {
            None => return Err(format!("there is no {}", show_path(path))),
            Some(st) if st.is_dir => {
                return Err(format!("{} is a folder (open_folder shows its pictures)", show_path(path)));
            }
            Some(_) => {}
        }
        let (dir, name) = catalog::split(path);
        if !is_image(&name) {
            return Err(format!("{} is not a picture Photos can show (PNG, JPEG, BMP or QOI)", show_path(path)));
        }
        if dir != self.dir {
            self.set_folder(&dir);
        } else {
            self.reload();
        }
        self.entries.iter().position(|e| e.name == name).ok_or_else(|| format!("{} could not be read", show_path(path)))
    }

    /// The picture an action names (see [`Photos::find_picture`] and [`Photos::picture_file`]).
    fn picture_arg(&mut self, args: &Value, key: &str) -> Result<usize, String> {
        let s = arg_str(args, key)?.trim();
        if s.contains('/') || s.starts_with('~') {
            let path = arg_path(args, key)?;
            return self.picture_file(&path);
        }
        if let Some(i) = self.find_picture(s) {
            return Ok(i);
        }
        // It may have arrived since the folder was listed.
        self.reload();
        self.find_picture(s)
            .ok_or_else(|| format!("there is no picture called \u{201c}{s}\u{201d} in {}", show_path(&self.dir)))
    }

    /// Shows picture `index` in the viewer, as a click on its card does (a slideshow ends: the
    /// user asked for this picture).
    fn show_index(&mut self, index: usize) {
        self.stop_slideshow();
        self.keyboard_nav = false;
        self.open(index);
    }

    /// Goes back to the library, as the viewer's "All photos" button does.
    fn leave_viewer(&mut self) {
        if self.mode == Mode::Viewer {
            self.pending_fullscreen = Some(false);
            self.close_viewer();
        }
    }

    /// Refuses actions that need a picture in the viewer.
    fn need_picture(&self) -> Result<(), String> {
        if self.mode == Mode::Viewer && self.current < self.entries.len() {
            Ok(())
        } else {
            Err("no picture is open: show one first".into())
        }
    }

    pub(crate) fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        let now = vrt::time::now_ns();
        match action {
            "show_picture" => {
                let i = self.picture_arg(args, "picture")?;
                self.show_index(i);
                Ok(self.position("showing"))
            }
            "go" => {
                let n = self.entries.len();
                if n == 0 {
                    return Err(self.no_pictures());
                }
                let cur = self.current.min(n - 1);
                let name = &self.entries[cur].name;
                let target = match arg_str(args, "to")? {
                    "next" if cur + 1 < n => cur + 1,
                    "next" => return Err(format!("{name} is the last picture")),
                    "previous" if cur > 0 => cur - 1,
                    "previous" => return Err(format!("{name} is the first picture")),
                    "first" => 0,
                    "last" => n - 1,
                    other => return Err(format!("'to' cannot be {other}: say next, previous, first or last")),
                };
                self.stop_slideshow();
                if self.mode == Mode::Viewer {
                    self.go_to(target);
                    Ok(self.position("showing"))
                } else {
                    // As the arrow keys do in the library.
                    self.current = target;
                    self.keyboard_nav = true;
                    self.reveal_selection = true;
                    Ok(self.position("selected"))
                }
            }
            "show_library" => {
                self.leave_viewer();
                self.reload();
                Ok(object! { "folder" => show_path(&self.dir), "picture_count" => self.entries.len() })
            }
            "open_folder" => {
                let path = arg_path(args, "path")?;
                match self.stat(&path) {
                    None => return Err(format!("there is no {}", show_path(&path))),
                    Some(st) if !st.is_dir => {
                        return Err(format!("{} is a file (show_picture shows a picture)", show_path(&path)));
                    }
                    Some(_) => {}
                }
                self.leave_viewer();
                self.set_folder(&path);
                if let Some(e) = &self.list_error {
                    return Err(e.clone());
                }
                let names: Vec<&str> = self.entries.iter().take(MAX_NAMES).map(|e| e.name.as_str()).collect();
                Ok(object! {
                    "folder" => show_path(&self.dir),
                    "picture_count" => self.entries.len(),
                    "pictures" => names,
                })
            }
            "zoom" => {
                self.need_picture()?;
                let d = self.dims().ok_or("the picture is still loading: ask again in a moment")?;
                let vp = self.viewport();
                let centre = (vp.w as f32 / 2.0, vp.h as f32 / 2.0);
                if !matches!(args.get("percent"), None | Some(Value::Null)) {
                    let pct = arg_f64(args, "percent")?;
                    if pct.is_nan() || pct <= 0.0 {
                        return Err("'percent' must be more than 0".into());
                    }
                    self.view.zoom_to(pct as f32 / 100.0, centre, d, vp, now);
                } else {
                    match arg_opt_str(args, "to") {
                        Some("fit") => self.zoom_to_fit(now),
                        Some("actual_size" | "actual") => self.view.zoom_to(1.0, centre, d, vp, now),
                        Some("in") => self.view.zoom_by(ZOOM_STEP, centre, d, vp, now),
                        Some("out") => self.view.zoom_by(1.0 / ZOOM_STEP, centre, d, vp, now),
                        Some(other) => {
                            return Err(format!("'to' cannot be {other}: say fit, actual_size, in or out"));
                        }
                        None => return Err("how? give 'to' (fit, actual_size, in or out) or 'percent'".into()),
                    }
                }
                let scale = self.view.placement(d, vp).scale;
                Ok(object! { "zoom_percent" => percent(scale), "fitted" => self.view.zoom.is_none() })
            }
            "rotate" => {
                self.need_picture()?;
                let turn = match arg_opt_str(args, "direction").unwrap_or("clockwise") {
                    "clockwise" | "right" => 1,
                    "anticlockwise" | "counterclockwise" | "left" => -1,
                    other => {
                        return Err(format!("'direction' cannot be {other}: say clockwise or anticlockwise"));
                    }
                };
                self.rotate_by(turn, now);
                Ok(object! { "rotation" => self.view.rot as u32 * 90 })
            }
            "full_screen" => {
                let on = arg_bool(args, "on").ok_or("say whether full screen goes on (true) or off (false)")?;
                if on {
                    if self.entries.is_empty() {
                        return Err(self.no_pictures());
                    }
                    if self.mode == Mode::Library {
                        self.show_index(self.current.min(self.entries.len() - 1));
                    }
                }
                self.pending_fullscreen = Some(on);
                Ok(object! { "full_screen" => on })
            }
            "slideshow" => {
                let on = arg_bool(args, "on").ok_or("say whether the slideshow starts (true) or stops (false)")?;
                if !on {
                    if self.slideshow.is_none() {
                        return Err("no slideshow is running".into());
                    }
                    // As a key would, and back in the window.
                    self.stop_slideshow();
                    self.pending_fullscreen = Some(false);
                    return Ok(self.position("showing"));
                }
                if self.entries.is_empty() {
                    return Err(self.no_pictures());
                }
                let secs = match args.get("seconds") {
                    None | Some(Value::Null) => SLIDE_NS as f64 / 1e9,
                    Some(_) => arg_f64(args, "seconds")?.clamp(2.0, 60.0),
                };
                if self.mode == Mode::Library {
                    self.show_index(self.current.min(self.entries.len() - 1));
                }
                self.start_slideshow((secs * 1e9) as u64);
                let mut v = self.position("from");
                v.set("seconds", secs);
                Ok(v)
            }
            "details" => {
                let show = arg_bool(args, "show").ok_or("say whether to show (true) or hide (false) the details")?;
                self.show_details(show, now);
                Ok(object! { "details_panel" => show })
            }
            "set_as_wallpaper" => {
                if arg_opt_str(args, "picture").is_some() {
                    let i = self.picture_arg(args, "picture")?;
                    self.show_index(i);
                } else if self.entries.is_empty() {
                    return Err(self.no_pictures());
                } else if self.mode == Mode::Library && !self.keyboard_nav {
                    return Err("which picture? name it, or show it first".into());
                }
                if self.wallpaper_busy {
                    return Err("the wallpaper is still being changed: try again in a moment".into());
                }
                let path = self.entries[self.current.min(self.entries.len() - 1)].path.clone();
                self.set_wallpaper();
                if !self.wallpaper_busy {
                    return Err("the desktop is not available".into());
                }
                Ok(object! { "wallpaper" => show_path(&path) })
            }
            other => Err(format!("Photos has no action called {other}")),
        }
    }
}
