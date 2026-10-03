//! Client-side windows: connection to the compositor, double-buffered
//! shared surfaces, frame pacing and event delivery.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Canvas, Rect};
use vproto::display::{self as dp, Cursor, WindowEvent, WindowSpec, WindowState, display};
use vrt::object::{Channel, Vmo};
use vrt::vm::Mapping;

/// Errors creating or driving a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowError {
    /// The display service is unavailable.
    NoDisplay,
    Ipc(vipc::IpcError),
    Display(dp::DisplayError),
    NoMemory,
}

/// A connection to the display service, shared by all windows of a process.
pub type Display = Rc<display::Client>;

/// Connects to the compositor.
pub fn connect() -> Result<Display, WindowError> {
    let ch = vproto::connect(display::NAME).map_err(|_| WindowError::NoDisplay)?;
    Ok(Rc::new(display::Client::new(ch)))
}

struct Surface {
    map: Mapping,
    width: i32,
    height: i32,
    stride: i32,
}

const BUFFERS: u8 = 2;

pub struct Window {
    pub display: Display,
    pub id: u32,
    events: Channel,
    surface: Option<Surface>,
    /// Buffer currently on screen (draw into the other one).
    shown: Option<u8>,
    /// A presented frame has not been acknowledged yet.
    in_flight: bool,
    pub width: i32,
    pub height: i32,
    pub state: WindowState,
    pub focused: bool,
    pub closed: bool,
}

impl Window {
    pub fn new(display: &Display, spec: WindowSpec) -> Result<Window, WindowError> {
        let (w, h) = (spec.width as i32, spec.height as i32);
        let (id, events) = display.create_window(spec).map_err(WindowError::Ipc)?.map_err(WindowError::Display)?;
        Ok(Window {
            display: display.clone(),
            id,
            events,
            surface: None,
            shown: None,
            in_flight: false,
            width: w,
            height: h,
            state: WindowState::Normal,
            focused: false,
            closed: false,
        })
    }

    /// The channel on which this window's events arrive (for wait sets).
    pub fn event_channel(&self) -> &Channel {
        &self.events
    }

    /// Reads all pending events (non-blocking), applying the ones that
    /// affect the window itself (configure, frame pacing, focus).
    pub fn poll_events(&mut self) -> Vec<WindowEvent> {
        let mut out = Vec::new();
        loop {
            match self.events.read() {
                Ok(msg) => {
                    if let Ok((_, ev)) = vipc::decode_event::<WindowEvent>(msg) {
                        match &ev {
                            WindowEvent::Configure { width, height, state } => {
                                self.width = *width as i32;
                                self.height = *height as i32;
                                self.state = *state;
                            }
                            WindowEvent::FrameDone { .. } => self.in_flight = false,
                            WindowEvent::Focus { focused } => self.focused = *focused,
                            _ => {}
                        }
                        out.push(ev);
                    }
                }
                Err(vabi::Error::PeerClosed) => {
                    self.closed = true;
                    break;
                }
                Err(_) => break,
            }
        }
        out
    }

    /// True if a new frame may be drawn now (the previous one was shown).
    pub fn can_draw(&self) -> bool {
        !self.in_flight && !self.closed
    }

    fn ensure_surface(&mut self) -> Result<(), WindowError> {
        let (w, h) = (self.width.max(1), self.height.max(1));
        if self.surface.as_ref().is_some_and(|s| s.width == w && s.height == h) {
            return Ok(());
        }
        let stride = w;
        let bytes = (stride * h * 4) as usize * BUFFERS as usize;
        let vmo = Vmo::create(bytes).map_err(|_| WindowError::NoMemory)?;
        let theirs = Vmo::from_handle(vmo.0.duplicate(None).map_err(|_| WindowError::NoMemory)?);
        let map = Mapping::new(vmo, bytes.next_multiple_of(4096), vabi::map_flags::READ | vabi::map_flags::WRITE)
            .map_err(|_| WindowError::NoMemory)?;
        self.display
            .attach_buffers(self.id, theirs, w as u32, h as u32, stride as u32, BUFFERS)
            .map_err(WindowError::Ipc)?
            .map_err(WindowError::Display)?;
        self.surface = Some(Surface { map, width: w, height: h, stride });
        self.shown = None;
        Ok(())
    }

    fn back_index(&self) -> u8 {
        match self.shown {
            Some(i) => (i + 1) % BUFFERS,
            None => 0,
        }
    }

    /// Returns a canvas for the next frame (the buffer that is not on
    /// screen), resizing the surface if the window size changed.
    pub fn begin_frame(&mut self) -> Result<Canvas<'_>, WindowError> {
        self.ensure_surface()?;
        let index = self.back_index();
        let s = self.surface.as_ref().unwrap();
        let per = (s.stride * s.height) as usize;
        // SAFETY: the mapping holds BUFFERS buffers of `per` pixels; the
        // compositor only reads the one on screen.
        let pixels =
            unsafe { core::slice::from_raw_parts_mut((s.map.as_ptr() as *mut u32).add(per * index as usize), per) };
        Ok(Canvas::new(pixels, s.width, s.height, s.stride))
    }

    /// Shows the frame drawn since `begin_frame`. `damage` lists changed
    /// rectangles (empty = everything).
    pub fn present(&mut self, damage: &[Rect]) -> Result<(), WindowError> {
        let index = self.back_index();
        let rects: Vec<dp::Rect> = damage.iter().map(|r| dp::Rect::new(r.x, r.y, r.w as u32, r.h as u32)).collect();
        self.display.present(self.id, index, rects).map_err(WindowError::Ipc)?.map_err(WindowError::Display)?;
        self.shown = Some(index);
        self.in_flight = true;
        Ok(())
    }

    pub fn set_title(&self, title: &str) {
        let _ = self.display.set_title(self.id, String::from(title));
    }

    pub fn set_cursor(&self, cursor: Cursor) {
        let _ = self.display.set_cursor(self.id, cursor);
    }

    pub fn set_state(&self, state: WindowState) {
        let _ = self.display.set_state(self.id, state);
    }

    pub fn set_position(&self, x: i32, y: i32) {
        let _ = self.display.set_position(self.id, x, y);
    }

    pub fn begin_move(&self) {
        let _ = self.display.begin_move(self.id);
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = self.display.destroy_window(self.id);
    }
}
