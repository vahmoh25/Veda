//! The display protocol: how applications get windows from the compositor.
//!
//! A client creates a window, then attaches pixel buffers that live in a
//! shared VMO (premultiplied `0xAARRGGBB`, typically two buffers for double
//! buffering). It draws into a buffer and `present`s it with the damaged
//! rectangles; the compositor answers with a `FrameDone` event when it has
//! finished reading the previous buffer, which paces clients to the display.
//!
//! The compositor draws window decorations (title bar, frame, shadow) itself,
//! so windows stay movable and closable even when their client hangs.
//! Events for a window (input, resize requests, focus) arrive on a separate
//! per-window event channel.

use alloc::string::String;
use alloc::vec::Vec;
use vipc::{enumeration, message, protocol, union};
use vrt::object::{Channel, Vmo};

message! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Rect {
        pub x: i32,
        pub y: i32,
        pub w: u32,
        pub h: u32,
    }
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }
}

enumeration! {
    /// What kind of surface a window is (determines layering and decoration).
    ///
    /// Normal and desktop windows are opaque: the compositor copies their
    /// pixels and ignores alpha, and does not draw what they cover. The other
    /// kinds are blended (premultiplied alpha), so they may be translucent or
    /// have rounded corners.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WindowKind {
        /// An ordinary application window with a title bar.
        Normal = 1,
        /// An undecorated window (splash screens, games in borderless mode).
        Borderless = 2,
        /// The desktop background (shell only), below everything.
        Desktop = 3,
        /// A panel such as the taskbar (shell only), above normal windows.
        Panel = 4,
        /// A popup or menu, above everything; closed when it loses focus.
        Popup = 5,
        /// A notification bubble (shell only), above windows, no focus.
        Notification = 6,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WindowState {
        Normal = 1,
        Maximized = 2,
        Minimized = 3,
        Fullscreen = 4,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Cursor {
        Arrow = 1,
        Text = 2,
        Hand = 3,
        Move = 4,
        ResizeHorizontal = 5,
        ResizeVertical = 6,
        ResizeDiagonal = 7,
        ResizeAntiDiagonal = 8,
        Busy = 9,
        Crosshair = 10,
        Hidden = 11,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DisplayError {
        NoSuchWindow = 1,
        BadBuffer = 2,
        Denied = 3,
        NoMemory = 4,
        Invalid = 5,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WindowSpec {
        pub title: String,
        pub kind: WindowKind,
        /// Client-area size.
        pub width: u32,
        pub height: u32,
        /// Requested position (`i32::MIN` = let the compositor choose).
        pub x: i32,
        pub y: i32,
        pub min_width: u32,
        pub min_height: u32,
        pub resizable: bool,
        /// Application id (matches the `.app` manifest; used for the taskbar).
        pub app_id: String,
    }
}

impl WindowSpec {
    pub fn new(title: &str, width: u32, height: u32) -> WindowSpec {
        WindowSpec {
            title: title.into(),
            kind: WindowKind::Normal,
            width,
            height,
            x: i32::MIN,
            y: i32::MIN,
            min_width: 160,
            min_height: 100,
            resizable: true,
            app_id: String::new(),
        }
    }
}

message! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ScreenInfo {
        pub width: u32,
        pub height: u32,
        /// Area not covered by panels (where windows are maximised).
        pub work_area: Rect,
    }
}

message! {
    /// A window as seen by the shell's taskbar.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WindowInfo {
        pub id: u32,
        pub title: String,
        pub app_id: String,
        pub state: WindowState,
        pub focused: bool,
        pub kind: WindowKind,
    }
}

/// Arrangements for [`display::Client::arrange_window`].
pub mod arrangement {
    pub const MAXIMIZE: u32 = 1;
    /// Back to the normal size and place (from maximised or snapped).
    pub const RESTORE: u32 = 2;
    pub const SNAP_LEFT: u32 = 3;
    pub const SNAP_RIGHT: u32 = 4;
}

/// Keyboard modifier bits in [`WindowEvent::Key`].
pub mod modifiers {
    pub const SHIFT: u32 = 1;
    pub const CTRL: u32 = 2;
    pub const ALT: u32 = 4;
    pub const SUPER: u32 = 8;
    pub const CAPS_LOCK: u32 = 16;
}

union! {
    /// Events delivered on a window's event channel.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum WindowEvent {
        /// The window should become `width` x `height` (attach new buffers).
        1 => Configure { width: u32, height: u32, state: WindowState },
        /// The user asked to close the window.
        2 => CloseRequested {},
        3 => Focus { focused: bool },
        /// Pointer moved inside the client area (window-relative pixels).
        4 => PointerMove { x: i32, y: i32 },
        /// `clicks` counts rapid successive presses (2 = double click).
        5 => PointerButton { x: i32, y: i32, button: u8, pressed: bool, clicks: u8 },
        6 => PointerLeave {},
        7 => Scroll { dx: i32, dy: i32 },
        /// A key event. `text` is the character produced (empty if none).
        8 => Key { code: u16, pressed: bool, repeat: bool, modifiers: u32, text: String },
        /// Buffer `shown` is now on screen: draw the next frame into another
        /// buffer and present it.
        9 => FrameDone { shown: u8 },
        /// (Shell) the set of windows or their states changed.
        10 => WindowsChanged {},
        /// (Shell) the user pressed the Start/Super key.
        11 => StartMenuKey {},
        /// (Shell) the user pressed Super+Space: the agent.
        12 => AgentKey {},
    }
}

/// Ordinal of window events on the event channel.
pub const EVENT: u32 = 1;

protocol! {
    /// The window system.
    pub mod display = "display" {
        /// Creates a window; returns its id and its event channel.
        1 => fn create_window(spec: WindowSpec) -> Result<(u32, Channel), DisplayError>;
        /// Attaches `count` buffers of `width` x `height` pixels (`stride`
        /// pixels per row), laid out back to back in `buffers`.
        2 => fn attach_buffers(id: u32, buffers: Vmo, width: u32, height: u32, stride: u32, count: u8) -> Result<(), DisplayError>;
        /// Shows buffer `index`; `damage` lists the changed rectangles
        /// (empty = everything).
        3 => fn present(id: u32, index: u8, damage: Vec<Rect>) -> Result<(), DisplayError>;
        4 => fn set_title(id: u32, title: String) -> Result<(), DisplayError>;
        5 => fn set_state(id: u32, state: WindowState) -> Result<(), DisplayError>;
        6 => fn destroy_window(id: u32) -> Result<(), DisplayError>;
        7 => fn set_cursor(id: u32, cursor: Cursor) -> Result<(), DisplayError>;
        /// Starts an interactive move (for client-drawn drag areas).
        8 => fn begin_move(id: u32) -> Result<(), DisplayError>;
        9 => fn screen_info() -> ScreenInfo;
        10 => fn set_clipboard(text: String) -> ();
        11 => fn get_clipboard() -> String;
        /// Moves a window (e.g. to place a popup).
        12 => fn set_position(id: u32, x: i32, y: i32) -> Result<(), DisplayError>;
        /// (Shell) all top-level windows.
        20 => fn list_windows() -> Vec<WindowInfo>;
        /// Raises, restores and focuses a window. The shell may activate
        /// any window, other clients only their own.
        21 => fn activate_window(id: u32) -> Result<(), DisplayError>;
        /// (Shell) minimises a window.
        22 => fn minimize_window(id: u32) -> Result<(), DisplayError>;
        /// (Shell) closes a window as if its close button was pressed.
        23 => fn close_window(id: u32) -> Result<(), DisplayError>;
        /// Shell only: maximises, restores or snaps any window (see
        /// [`arrangement`]), bringing it to the front.
        24 => fn arrange_window(id: u32, arrangement: u32) -> Result<(), DisplayError>;
    }
}
