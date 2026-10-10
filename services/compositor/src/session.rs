//! The `display` protocol: one [`Session`] per client connection.

use alloc::string::String;
use alloc::vec::Vec;

use vgfx::Rect;
use vproto::display::{
    self as dp, Cursor, DisplayError, ScreenInfo, WindowEvent, WindowInfo, WindowKind, WindowSpec, WindowState, display,
};
use vrt::object::{Channel, Vmo};
use vrt::vm::Mapping;

use crate::state::{Compositor, Drag};
use crate::window::{Anim, AnimKind, Buffers, TITLE_HEIGHT, Window};

/// One display-protocol connection.
pub(crate) struct Session<'a> {
    pub(crate) comp: &'a mut Compositor,
    pub(crate) client: u64,
}

impl Session<'_> {
    fn own(&self, id: u32) -> Result<(), DisplayError> {
        match self.comp.windows.get(&id) {
            Some(w) if w.client == self.client => Ok(()),
            _ => Err(DisplayError::NoSuchWindow),
        }
    }

    fn is_shell(&self) -> bool {
        self.comp.shell == Some(self.client)
    }
}

impl display::Server for Session<'_> {
    fn create_window(&mut self, spec: WindowSpec) -> Result<(u32, Channel), DisplayError> {
        if spec.width == 0 || spec.height == 0 || spec.width > 8192 || spec.height > 8192 {
            return Err(DisplayError::Invalid);
        }
        if matches!(spec.kind, WindowKind::Desktop | WindowKind::Panel | WindowKind::Notification) {
            // The first client to create shell surfaces becomes the shell.
            match self.comp.shell {
                None => self.comp.shell = Some(self.client),
                Some(s) if s != self.client => return Err(DisplayError::Denied),
                _ => {}
            }
        }
        let (ours, theirs) = Channel::create().map_err(|_| DisplayError::NoMemory)?;
        let id = self.comp.next_id;
        self.comp.next_id += 1;
        let (rect, auto_placed) = self.comp.place_new_window(&spec);
        let animate = spec.kind == WindowKind::Normal || spec.kind == WindowKind::Popup;
        let win = Window {
            id,
            client: self.client,
            kind: spec.kind,
            title: spec.title,
            app_id: spec.app_id,
            state: WindowState::Normal,
            client_rect: rect,
            restore_rect: rect,
            min_w: spec.min_width.max(64) as i32,
            min_h: spec.min_height.max(32) as i32,
            resizable: spec.resizable && spec.kind == WindowKind::Normal,
            events: ours,
            buffers: None,
            current: None,
            upload: vgfx::Damage::new(),
            upload_all: true,
            frame_owed: None,
            cursor: Cursor::Arrow,
            anim: animate.then(|| Anim { kind: AnimKind::Open, start: vrt::time::now_ns(), duration: 180_000_000 }),
            closing: false,
            snapped: None,
            auto_placed,
        };
        let kind = win.kind;
        self.comp.windows.insert(id, win);
        self.comp.order.push(id);
        if let Some(c) = self.comp.clients.get_mut(&self.client) {
            c.windows.push(id);
        }
        // The client may already know its final size; tell it anyway.
        let _ = vipc::send_event(
            &self.comp.windows[&id].events,
            dp::EVENT,
            WindowEvent::Configure { width: rect.w as u32, height: rect.h as u32, state: WindowState::Normal },
        );
        if matches!(kind, WindowKind::Normal | WindowKind::Borderless | WindowKind::Popup) {
            self.comp.raise(id);
            self.comp.focus(Some(id));
        }
        if kind == WindowKind::Panel {
            self.comp.update_work_area();
        }
        self.comp.damage_window(id);
        self.comp.notify_shell();
        Ok((id, theirs))
    }

    fn attach_buffers(
        &mut self,
        id: u32,
        buffers: Vmo,
        width: u32,
        height: u32,
        stride: u32,
        count: u8,
    ) -> Result<(), DisplayError> {
        self.own(id)?;
        if width == 0 || height == 0 || stride < width || count == 0 || count > 3 || width > 8192 || height > 8192 {
            return Err(DisplayError::BadBuffer);
        }
        let bytes = stride as usize * height as usize * 4 * count as usize;
        let size = buffers.size().map_err(|_| DisplayError::BadBuffer)?;
        if size < bytes {
            return Err(DisplayError::BadBuffer);
        }
        let map = Mapping::new(buffers, bytes.next_multiple_of(4096), vabi::map_flags::READ)
            .map_err(|_| DisplayError::NoMemory)?;
        let w = self.comp.windows.get_mut(&id).unwrap();
        w.buffers = Some(Buffers { map, width: width as i32, height: height as i32, stride: stride as i32, count });
        w.current = None;
        w.upload_all = true;
        Ok(())
    }

    fn present(&mut self, id: u32, index: u8, damage: Vec<dp::Rect>) -> Result<(), DisplayError> {
        self.own(id)?;
        let w = self.comp.windows.get_mut(&id).unwrap();
        let Some(b) = &w.buffers else { return Err(DisplayError::BadBuffer) };
        if index >= b.count {
            return Err(DisplayError::BadBuffer);
        }
        let first = w.current.is_none();
        w.current = Some(index);
        // Acknowledged with FrameDone after the next composite.
        w.frame_owed = Some(index);
        // What the GPU's copy lacks now (see `gpu`).
        if first || damage.is_empty() {
            w.upload_all = true;
        } else {
            let pixels = Rect::new(0, 0, b.width, b.height);
            for d in &damage {
                w.upload.add(Rect::new(d.x, d.y, d.w as i32, d.h as i32).intersect(&pixels));
            }
        }
        let c = w.client_rect;
        if first || w.anim.is_some() {
            // The window appears or is animating: redraw it with its frame.
            let r = w.paint_bounds();
            self.comp.damage.add(r);
        } else if damage.is_empty() {
            // A whole new frame: the decorations around it are unchanged.
            self.comp.damage.add(c);
        } else {
            for d in damage {
                self.comp.damage.add(Rect::new(c.x + d.x, c.y + d.y, d.w as i32, d.h as i32).intersect(&c));
            }
        }
        Ok(())
    }

    fn set_title(&mut self, id: u32, title: String) -> Result<(), DisplayError> {
        self.own(id)?;
        let w = self.comp.windows.get_mut(&id).unwrap();
        w.title = title;
        let r = w.title_rect();
        self.comp.damage.add(r);
        self.comp.notify_shell();
        Ok(())
    }

    fn set_state(&mut self, id: u32, state: WindowState) -> Result<(), DisplayError> {
        self.own(id)?;
        self.comp.set_state(id, state);
        Ok(())
    }

    fn destroy_window(&mut self, id: u32) -> Result<(), DisplayError> {
        self.own(id)?;
        let w = self.comp.windows.get_mut(&id).unwrap();
        if w.kind == WindowKind::Normal && w.state != WindowState::Minimized {
            // Fade out, then remove.
            w.closing = true;
            w.anim = Some(Anim { kind: AnimKind::Close, start: vrt::time::now_ns(), duration: 130_000_000 });
            if self.comp.focused == Some(id) {
                self.comp.focused = None;
                self.comp.focus_top();
            }
            self.comp.notify_shell();
        } else {
            self.comp.destroy_window(id);
        }
        Ok(())
    }

    fn set_cursor(&mut self, id: u32, cursor: Cursor) -> Result<(), DisplayError> {
        self.own(id)?;
        self.comp.windows.get_mut(&id).unwrap().cursor = cursor;
        if self.comp.hover_window == Some(id) {
            self.comp.update_hover();
        }
        Ok(())
    }

    fn begin_move(&mut self, id: u32) -> Result<(), DisplayError> {
        self.own(id)?;
        let c = self.comp.windows[&id].client_rect;
        let (x, y) = self.comp.pointer;
        self.comp.drag = Some(Drag::Move { id, dx: x - c.x, dy: y - c.y, origin: (x, y), restore: false });
        Ok(())
    }

    fn screen_info(&mut self) -> ScreenInfo {
        let s = self.comp.screen_rect();
        let a = self.comp.work_area;
        ScreenInfo { width: s.w as u32, height: s.h as u32, work_area: dp::Rect::new(a.x, a.y, a.w as u32, a.h as u32) }
    }

    fn set_clipboard(&mut self, text: String) {
        self.comp.clipboard = text;
    }

    fn get_clipboard(&mut self) -> String {
        self.comp.clipboard.clone()
    }

    fn set_position(&mut self, id: u32, x: i32, y: i32) -> Result<(), DisplayError> {
        self.own(id)?;
        self.comp.damage_window(id);
        let w = self.comp.windows.get_mut(&id).unwrap();
        let tb = if w.decorated() { TITLE_HEIGHT } else { 0 };
        w.client_rect = Rect::new(x, y + tb, w.client_rect.w, w.client_rect.h);
        w.auto_placed = false;
        self.comp.damage_window(id);
        if self.comp.windows[&id].kind == WindowKind::Panel {
            self.comp.update_work_area();
        }
        Ok(())
    }

    fn list_windows(&mut self) -> Vec<WindowInfo> {
        self.comp
            .order
            .iter()
            .filter_map(|id| self.comp.windows.get(id))
            .filter(|w| w.kind == WindowKind::Normal && !w.closing)
            .map(|w| WindowInfo {
                id: w.id,
                title: w.title.clone(),
                app_id: w.app_id.clone(),
                state: w.state,
                focused: self.comp.focused == Some(w.id),
                kind: w.kind,
            })
            .collect()
    }

    fn activate_window(&mut self, id: u32) -> Result<(), DisplayError> {
        // The shell activates any window; an application only its own (a
        // single-instance application handed a file, for example).
        if !self.is_shell() {
            self.own(id)?;
        }
        let st = self.comp.windows.get(&id).map(|w| w.state).ok_or(DisplayError::NoSuchWindow)?;
        if st == WindowState::Minimized {
            self.comp.set_state(id, WindowState::Normal);
        }
        self.comp.raise(id);
        self.comp.focus(Some(id));
        Ok(())
    }

    fn minimize_window(&mut self, id: u32) -> Result<(), DisplayError> {
        if !self.is_shell() {
            return Err(DisplayError::Denied);
        }
        if !self.comp.windows.contains_key(&id) {
            return Err(DisplayError::NoSuchWindow);
        }
        self.comp.set_state(id, WindowState::Minimized);
        Ok(())
    }

    fn arrange_window(&mut self, id: u32, arrangement: u32) -> Result<(), DisplayError> {
        if !self.is_shell() {
            return Err(DisplayError::Denied);
        }
        let st = self.comp.windows.get(&id).map(|w| w.state).ok_or(DisplayError::NoSuchWindow)?;
        if st == WindowState::Minimized {
            self.comp.set_state(id, WindowState::Normal);
        }
        self.comp.raise(id);
        self.comp.focus(Some(id));
        // The same arrangements as Super+arrow keys on the focused window.
        let key = match arrangement {
            vproto::display::arrangement::MAXIMIZE => vproto::input::keys::UP,
            vproto::display::arrangement::RESTORE => vproto::input::keys::DOWN,
            vproto::display::arrangement::SNAP_LEFT => vproto::input::keys::LEFT,
            vproto::display::arrangement::SNAP_RIGHT => vproto::input::keys::RIGHT,
            _ => return Err(DisplayError::Invalid),
        };
        if key == vproto::input::keys::DOWN && st == WindowState::Minimized {
            return Ok(());
        }
        self.comp.arrange_focused(key);
        Ok(())
    }

    fn desktop_ready(&mut self) -> Result<(), DisplayError> {
        if !self.is_shell() {
            return Err(DisplayError::Denied);
        }
        if let Some(s) = &mut self.comp.startup {
            s.clear();
        }
        Ok(())
    }

    fn close_window(&mut self, id: u32) -> Result<(), DisplayError> {
        if !self.is_shell() {
            return Err(DisplayError::Denied);
        }
        if !self.comp.windows.contains_key(&id) {
            return Err(DisplayError::NoSuchWindow);
        }
        self.comp.send(id, WindowEvent::CloseRequested {});
        Ok(())
    }
}
