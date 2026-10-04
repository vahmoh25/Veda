//! The Veda visual theme (dark).

use vgfx::Color;

/// Font roles; mapped to loaded fonts by the toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
    Mono,
    MonoBold,
}

#[derive(Debug, Clone)]
pub struct Theme {
    /// Window background.
    pub bg: Color,
    /// Raised surfaces (cards, panels, sidebars).
    pub surface: Color,
    /// Controls (text boxes, buttons) and hover backgrounds.
    pub control: Color,
    pub control_hover: Color,
    pub control_pressed: Color,
    pub border: Color,
    pub border_strong: Color,
    pub text: Color,
    pub text_dim: Color,
    pub text_faint: Color,
    pub accent: Color,
    pub accent_hover: Color,
    pub accent_pressed: Color,
    /// Second stop of accent gradients.
    pub accent2: Color,
    pub on_accent: Color,
    pub selection: Color,
    pub danger: Color,
    pub success: Color,
    pub warning: Color,
    pub radius: f32,
    pub radius_large: f32,
    pub font_size: f32,
    pub small_size: f32,
    pub heading_size: f32,
    pub title_size: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

impl Theme {
    pub fn dark() -> Theme {
        Theme {
            bg: Color::hex(0x1B1B20),
            surface: Color::hex(0x232329),
            control: Color::hex(0x2C2C33),
            control_hover: Color::hex(0x35353D),
            control_pressed: Color::hex(0x2A2A30),
            border: Color::rgba(255, 255, 255, 22),
            border_strong: Color::rgba(255, 255, 255, 40),
            text: Color::hex(0xECECF1),
            text_dim: Color::hex(0xA6A6B2),
            text_faint: Color::hex(0x6E6E7A),
            accent: Color::hex(0x5B8CFF),
            accent_hover: Color::hex(0x6E9BFF),
            accent_pressed: Color::hex(0x4F7BE6),
            accent2: Color::hex(0x8B5CF6),
            on_accent: Color::WHITE,
            selection: Color::rgba(91, 140, 255, 70),
            danger: Color::hex(0xE5484D),
            success: Color::hex(0x30A46C),
            warning: Color::hex(0xF5A524),
            radius: 6.0,
            radius_large: 10.0,
            font_size: 14.0,
            small_size: 12.0,
            heading_size: 18.0,
            title_size: 26.0,
        }
    }
}
