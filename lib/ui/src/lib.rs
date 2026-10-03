//! `vui` — the Vindows GUI toolkit.
//!
//! * [`window`]: client-side windows on top of the display protocol.
//! * [`app`]: the event loop ([`run`]) driving an [`App`].
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

pub mod app;
pub mod icons;
pub mod menu;
pub mod text_input;
pub mod theme;
pub mod ui;
pub mod widgets;
pub mod window;

pub use app::{App, load_fonts, run};
pub use icons::Icon;
pub use menu::{Menu, MenuItem};
pub use text_input::TextInputResponse;
pub use theme::{Font, Theme};
pub use ui::{Id, Input, Response, Ui};
pub use vgfx::{Align, Canvas, Color, Rect};
pub use vproto::display::{Cursor, WindowKind, WindowSpec, WindowState};
pub use widgets::{ButtonKind, ListResponse, RowState};
