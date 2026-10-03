//! `vui` — the Vindows GUI toolkit.
//!
//! * [`window`]: client-side windows on top of the display protocol.
//! * [`app`]: the event loop ([`run`]) driving an [`App`].
//! * [`agent`]: offering an application's abilities to the voice agent.
//! * [`Ui`]: the immediate-mode context passed to `App::update` each frame,
//!   with widgets ([`widgets`], [`text_input`], [`menu`]), icons
//!   ([`Icon`]) and the dark [`Theme`].
//!
//! ```ignore
//! struct Hello { clicks: u32 }
//! impl vui::App for Hello {
//!     fn update(&mut self, ui: &mut vui::Ui) {
//!         let r = ui.rect().centered(160, 40);
//!         if ui.primary_button(r, "Click me") { self.clicks += 1; }
//!     }
//! }
//! ```

#![no_std]

extern crate alloc;

pub mod agent;
pub mod app;
pub mod file_dialog;
pub mod icons;
pub mod menu;
pub mod text_input;
pub mod theme;
pub mod ui;
pub mod widgets;
pub mod window;

pub use app::{App, Host, load_fonts, run};
pub use file_dialog::{FileDialog, FileDialogMode, FileDialogResult};
pub use icons::{Icon, draw_logo};
pub use menu::{Menu, MenuItem};
pub use text_input::TextInputResponse;
pub use theme::{Font, Theme};
pub use ui::{Context, Id, Input, Response, Ui, UiState};
pub use vgfx::{Align, Canvas, Color, Rect};
pub use vproto::display::{Cursor, WindowKind, WindowSpec, WindowState};
pub use widgets::{ButtonKind, ListResponse, RowState};
