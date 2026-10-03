//! The volume flyout opened from the speaker icon on the taskbar.

use alloc::format;

use vgfx::{Align, Rect};
use vui::{Font, Icon, Ui};

use crate::{Action, Model, chrome, taskbar};

const WIDTH: i32 = 340;
const HEIGHT: i32 = 128;
const PAD: i32 = 20;

/// Where the flyout appears: above the tray, at the right edge.
pub fn placement(screen: Rect) -> Rect {
    Rect::new(screen.w - WIDTH - 12, screen.h - taskbar::HEIGHT - 12 - HEIGHT, WIDTH, HEIGHT)
}

/// The icon for a volume level.
pub fn icon(volume: f32, muted: bool) -> Icon {
    if muted || volume <= 0.001 { Icon::Mute } else { Icon::Volume }
}

pub struct VolumeFlyout {
    origin: (i32, i32),
}

impl VolumeFlyout {
    pub fn new(origin: (i32, i32)) -> VolumeFlyout {
        VolumeFlyout { origin }
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        let t = ui.theme().clone();
        let w = ui.width;
        chrome::popup_background(&mut ui.canvas, &m.wallpaper.blurred, self.origin);
        if ui.input.key(vproto::input::keys::ESC) {
            ui.close_window();
        }
        let Some(audio) = m.audio.clone() else {
            ui.label(
                Rect::new(PAD, 0, w - 2 * PAD, HEIGHT),
                "No sound device",
                Font::Regular,
                14.0,
                t.text_dim,
                Align::Center,
            );
            chrome::popup_frame(&mut ui.canvas);
            return;
        };
        ui.label(Rect::new(PAD, 14, 200, 24), "Volume", Font::Bold, 14.0, t.text, Align::Left);
        let detail = format!("{} \u{00b7} {} kHz", audio.device, audio.rate / 1000);
        ui.label(Rect::new(PAD, 14, w - 2 * PAD, 24), &detail, Font::Regular, 12.0, t.text_faint, Align::Right);

        // Mute button, slider and percentage.
        let row = Rect::new(PAD, 56, w - 2 * PAD, 40);
        let mute_r = Rect::new(row.x - 6, row.y, 40, 40);
        let mut muted = audio.muted;
        if ui.icon_button(mute_r, icon(audio.master_volume, audio.muted), if muted { "Unmute" } else { "Mute" }) {
            muted = !muted;
        }
        let mut volume = audio.master_volume;
        let slider = Rect::new(mute_r.right() + 8, row.y, row.w - 40 - 8 - 52, 40);
        if ui.slider(slider, "master-volume", &mut volume, 0.0, 1.0) && volume > 0.0 {
            muted = false;
        }
        let pct = format!("{}", (volume * 100.0 + 0.5) as u32);
        let color = if muted { t.text_faint } else { t.text };
        ui.label(Rect::new(slider.right() + 8, row.y, 44, 40), &pct, Font::Bold, 15.0, color, Align::Right);
        if (volume, muted) != (audio.master_volume, audio.muted) {
            m.push(Action::SetVolume(volume, muted));
        }
        chrome::popup_frame(&mut ui.canvas);
    }
}
