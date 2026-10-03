//! The Vindows icon set: simple line icons designed on a 24x24 grid, drawn
//! as vector paths so they are crisp at any size.

use vgfx::{Canvas, Color, FillRule, LineCap, LineJoin, Path, Rect, StrokeStyle, Transform};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Folder,
    File,
    Document,
    Image,
    Music,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    Shuffle,
    Repeat,
    Volume,
    Mute,
    Search,
    Settings,
    Power,
    Restart,
    Close,
    Plus,
    Minus,
    Check,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronDown,
    Home,
    Terminal,
    Grid,
    Edit,
    Save,
    Open,
    Trash,
    Info,
    Warning,
    Star,
    Heart,
    Gamepad,
    Monitor,
    Cpu,
    Clock,
    Calendar,
    Refresh,
    ZoomIn,
    ZoomOut,
    /// Rotate anticlockwise.
    Rotate,
    /// Rotate clockwise.
    RotateRight,
    Fullscreen,
    List,
    User,
    Sun,
    Moon,
    Palette,
    Chart,
    Car,
    Rocket,
    Chess,
    Cube,
    Lock,
    Up,
    Download,
    Copy,
    Cut,
    Paste,
    Undo,
    Redo,
    Wallpaper,
    Speaker,
}

/// Stroke-based icon geometry on a 24x24 grid; `filled` parts are filled.
struct Builder {
    stroke: Path,
    fill: Path,
}

impl Builder {
    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.stroke.move_to(x0, y0);
        self.stroke.line_to(x1, y1);
    }
    fn poly(&mut self, pts: &[(f32, f32)], close: bool) {
        for (i, &(x, y)) in pts.iter().enumerate() {
            if i == 0 { self.stroke.move_to(x, y) } else { self.stroke.line_to(x, y) }
        }
        if close {
            self.stroke.close();
        }
    }
    fn fill_poly(&mut self, pts: &[(f32, f32)]) {
        for (i, &(x, y)) in pts.iter().enumerate() {
            if i == 0 { self.fill.move_to(x, y) } else { self.fill.line_to(x, y) }
        }
        self.fill.close();
    }
    fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        self.stroke.rounded_rect(x, y, w, h, [r; 4]);
    }
    fn circle(&mut self, cx: f32, cy: f32, r: f32) {
        self.stroke.circle(cx, cy, r);
    }
    fn arc(&mut self, cx: f32, cy: f32, r: f32, start_deg: f32, sweep_deg: f32) {
        let rad = core::f32::consts::PI / 180.0;
        self.stroke.arc(cx, cy, r, start_deg * rad, sweep_deg * rad);
    }
    fn dot(&mut self, cx: f32, cy: f32, r: f32) {
        self.fill.circle(cx, cy, r);
    }
}

fn build(icon: Icon) -> Builder {
    let mut b = Builder { stroke: Path::new(), fill: Path::new() };
    match icon {
        Icon::Folder => b.poly(
            &[(3.0, 7.0), (3.0, 18.0), (21.0, 18.0), (21.0, 8.5), (12.0, 8.5), (10.0, 5.5), (3.0, 5.5), (3.0, 7.0)],
            true,
        ),
        Icon::File => {
            b.poly(&[(6.0, 3.0), (14.0, 3.0), (19.0, 8.0), (19.0, 21.0), (6.0, 21.0)], true);
            b.poly(&[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)], false);
        }
        Icon::Document => {
            b.poly(&[(6.0, 3.0), (14.0, 3.0), (19.0, 8.0), (19.0, 21.0), (6.0, 21.0)], true);
            b.poly(&[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)], false);
            b.line(9.0, 12.5, 16.0, 12.5);
            b.line(9.0, 16.0, 16.0, 16.0);
        }
        Icon::Image | Icon::Wallpaper => {
            b.rrect(3.0, 4.0, 18.0, 16.0, 2.5);
            b.dot(8.5, 9.0, 1.8);
            b.poly(&[(3.5, 18.0), (9.5, 12.0), (13.0, 15.5), (16.0, 12.5), (20.5, 17.0)], false);
        }
        Icon::Music => {
            b.poly(&[(9.0, 18.0), (9.0, 5.0), (20.0, 3.0), (20.0, 16.0)], false);
            b.circle(6.5, 18.0, 2.6);
            b.circle(17.5, 16.0, 2.6);
        }
        Icon::Play => b.fill_poly(&[(7.0, 4.5), (19.5, 12.0), (7.0, 19.5)]),
        Icon::Pause => {
            b.fill.rounded_rect(6.0, 4.5, 4.0, 15.0, [1.0; 4]);
            b.fill.rounded_rect(14.0, 4.5, 4.0, 15.0, [1.0; 4]);
        }
        Icon::Stop => b.fill.rounded_rect(5.5, 5.5, 13.0, 13.0, [2.0; 4]),
        Icon::Next => {
            b.fill_poly(&[(5.0, 5.0), (15.0, 12.0), (5.0, 19.0)]);
            b.fill.rounded_rect(16.0, 5.0, 3.0, 14.0, [1.0; 4]);
        }
        Icon::Previous => {
            b.fill_poly(&[(19.0, 5.0), (9.0, 12.0), (19.0, 19.0)]);
            b.fill.rounded_rect(5.0, 5.0, 3.0, 14.0, [1.0; 4]);
        }
        Icon::Shuffle => {
            b.poly(&[(3.0, 7.0), (7.0, 7.0), (15.0, 17.0), (20.0, 17.0)], false);
            b.poly(&[(3.0, 17.0), (7.0, 17.0), (15.0, 7.0), (20.0, 7.0)], false);
            b.poly(&[(17.5, 4.5), (20.5, 7.0), (17.5, 9.5)], false);
            b.poly(&[(17.5, 14.5), (20.5, 17.0), (17.5, 19.5)], false);
        }
        Icon::Repeat => {
            b.poly(&[(4.0, 11.0), (4.0, 8.0), (19.0, 8.0)], false);
            b.poly(&[(16.0, 5.0), (19.0, 8.0), (16.0, 11.0)], false);
            b.poly(&[(20.0, 13.0), (20.0, 16.0), (5.0, 16.0)], false);
            b.poly(&[(8.0, 19.0), (5.0, 16.0), (8.0, 13.0)], false);
        }
        Icon::Volume | Icon::Speaker => {
            b.poly(&[(4.0, 9.0), (8.0, 9.0), (13.0, 5.0), (13.0, 19.0), (8.0, 15.0), (4.0, 15.0)], true);
            b.arc(13.0, 12.0, 4.0, -45.0, 90.0);
            b.arc(13.0, 12.0, 7.5, -50.0, 100.0);
        }
        Icon::Mute => {
            b.poly(&[(4.0, 9.0), (8.0, 9.0), (13.0, 5.0), (13.0, 19.0), (8.0, 15.0), (4.0, 15.0)], true);
            b.line(16.5, 9.5, 21.5, 14.5);
            b.line(21.5, 9.5, 16.5, 14.5);
        }
        Icon::Search => {
            b.circle(10.5, 10.5, 6.5);
            b.line(15.5, 15.5, 20.5, 20.5);
        }
        Icon::Settings => {
            b.circle(12.0, 12.0, 3.0);
            // Gear: eight teeth around a ring.
            let rad = core::f32::consts::PI / 180.0;
            for i in 0..8 {
                let a = i as f32 * 45.0 * rad;
                let (s, c) = (vmath::FloatExt::sin(a), vmath::FloatExt::cos(a));
                b.line(12.0 + c * 6.5, 12.0 + s * 6.5, 12.0 + c * 9.0, 12.0 + s * 9.0);
            }
            b.circle(12.0, 12.0, 6.5);
        }
        Icon::Power => {
            b.line(12.0, 3.5, 12.0, 11.0);
            b.arc(12.0, 13.0, 7.5, -60.0, 300.0);
        }
        Icon::Restart | Icon::Refresh => {
            b.arc(12.0, 12.0, 7.5, -70.0, 300.0);
            b.poly(&[(14.5, 2.5), (14.6, 5.4), (17.6, 5.0)], false);
        }
        Icon::Close => {
            b.line(6.0, 6.0, 18.0, 18.0);
            b.line(18.0, 6.0, 6.0, 18.0);
        }
        Icon::Plus => {
            b.line(12.0, 5.0, 12.0, 19.0);
            b.line(5.0, 12.0, 19.0, 12.0);
        }
        Icon::Minus => b.line(5.0, 12.0, 19.0, 12.0),
        Icon::Check => b.poly(&[(5.0, 12.5), (10.0, 17.5), (19.5, 7.0)], false),
        Icon::ChevronLeft => b.poly(&[(15.0, 5.0), (8.0, 12.0), (15.0, 19.0)], false),
        Icon::ChevronRight => b.poly(&[(9.0, 5.0), (16.0, 12.0), (9.0, 19.0)], false),
        Icon::ChevronUp => b.poly(&[(5.0, 15.0), (12.0, 8.0), (19.0, 15.0)], false),
        Icon::ChevronDown => b.poly(&[(5.0, 9.0), (12.0, 16.0), (19.0, 9.0)], false),
        Icon::Up => {
            b.line(12.0, 19.0, 12.0, 5.0);
            b.poly(&[(6.0, 11.0), (12.0, 5.0), (18.0, 11.0)], false);
        }
        Icon::Download => {
            b.line(12.0, 4.0, 12.0, 15.0);
            b.poly(&[(7.0, 10.0), (12.0, 15.0), (17.0, 10.0)], false);
            b.poly(&[(4.0, 15.0), (4.0, 20.0), (20.0, 20.0), (20.0, 15.0)], false);
        }
        Icon::Home => {
            b.poly(&[(3.5, 11.0), (12.0, 3.5), (20.5, 11.0)], false);
            b.poly(&[(6.0, 9.0), (6.0, 20.0), (18.0, 20.0), (18.0, 9.0)], false);
            b.poly(&[(10.0, 20.0), (10.0, 14.0), (14.0, 14.0), (14.0, 20.0)], false);
        }
        Icon::Terminal => {
            b.rrect(2.5, 4.0, 19.0, 16.0, 2.5);
            b.poly(&[(6.5, 9.0), (9.5, 12.0), (6.5, 15.0)], false);
            b.line(11.5, 15.0, 16.5, 15.0);
        }
        Icon::Grid => {
            for (x, y) in [(4.0, 4.0), (13.5, 4.0), (4.0, 13.5), (13.5, 13.5)] {
                b.rrect(x, y, 6.5, 6.5, 1.5);
            }
        }
        Icon::Edit => {
            b.poly(&[(4.0, 20.0), (4.5, 16.0), (16.0, 4.5), (19.5, 8.0), (8.0, 19.5)], true);
            b.line(13.5, 7.0, 17.0, 10.5);
        }
        Icon::Save => {
            b.poly(&[(4.0, 4.0), (16.5, 4.0), (20.0, 7.5), (20.0, 20.0), (4.0, 20.0)], true);
            b.poly(&[(8.0, 4.0), (8.0, 9.0), (15.0, 9.0), (15.0, 4.0)], false);
            b.rrect(7.0, 13.0, 10.0, 7.0, 1.0);
        }
        Icon::Open => {
            b.poly(&[(3.0, 18.0), (3.0, 5.5), (9.5, 5.5), (11.5, 8.0), (18.0, 8.0), (18.0, 10.5)], false);
            b.poly(&[(3.0, 18.0), (6.0, 10.5), (21.5, 10.5), (18.5, 18.0)], true);
        }
        Icon::Trash => {
            b.line(4.0, 6.5, 20.0, 6.5);
            b.poly(&[(9.0, 6.5), (9.5, 3.5), (14.5, 3.5), (15.0, 6.5)], false);
            b.poly(&[(6.0, 6.5), (7.0, 20.5), (17.0, 20.5), (18.0, 6.5)], false);
            b.line(10.0, 10.5, 10.0, 16.5);
            b.line(14.0, 10.5, 14.0, 16.5);
        }
        Icon::Info => {
            b.circle(12.0, 12.0, 9.0);
            b.line(12.0, 11.0, 12.0, 16.5);
            b.dot(12.0, 7.8, 1.2);
        }
        Icon::Warning => {
            b.poly(&[(12.0, 3.5), (21.5, 20.0), (2.5, 20.0)], true);
            b.line(12.0, 9.5, 12.0, 14.0);
            b.dot(12.0, 17.0, 1.1);
        }
        Icon::Star => b.poly(
            &[
                (12.0, 3.0),
                (14.7, 8.9),
                (21.0, 9.5),
                (16.2, 13.7),
                (17.6, 20.0),
                (12.0, 16.7),
                (6.4, 20.0),
                (7.8, 13.7),
                (3.0, 9.5),
                (9.3, 8.9),
            ],
            true,
        ),
        Icon::Heart => {
            b.stroke.move_to(12.0, 20.0);
            b.stroke.cubic_to(2.0, 13.0, 3.0, 4.5, 8.0, 4.5);
            b.stroke.cubic_to(10.0, 4.5, 11.5, 6.0, 12.0, 7.5);
            b.stroke.cubic_to(12.5, 6.0, 14.0, 4.5, 16.0, 4.5);
            b.stroke.cubic_to(21.0, 4.5, 22.0, 13.0, 12.0, 20.0);
            b.stroke.close();
        }
        Icon::Gamepad => {
            b.rrect(2.5, 7.0, 19.0, 11.0, 5.0);
            b.line(6.5, 12.5, 10.0, 12.5);
            b.line(8.25, 10.75, 8.25, 14.25);
            b.dot(15.0, 11.5, 1.2);
            b.dot(17.5, 13.5, 1.2);
        }
        Icon::Monitor => {
            b.rrect(2.5, 4.0, 19.0, 13.0, 2.0);
            b.line(9.0, 20.5, 15.0, 20.5);
            b.line(12.0, 17.0, 12.0, 20.5);
        }
        Icon::Cpu => {
            b.rrect(6.0, 6.0, 12.0, 12.0, 2.0);
            b.rrect(9.5, 9.5, 5.0, 5.0, 1.0);
            for t in [9.5, 14.5] {
                b.line(t, 2.5, t, 6.0);
                b.line(t, 18.0, t, 21.5);
                b.line(2.5, t, 6.0, t);
                b.line(18.0, t, 21.5, t);
            }
        }
        Icon::Clock => {
            b.circle(12.0, 12.0, 9.0);
            b.poly(&[(12.0, 7.0), (12.0, 12.0), (15.5, 14.0)], false);
        }
        Icon::Calendar => {
            b.rrect(3.5, 5.0, 17.0, 15.5, 2.0);
            b.line(3.5, 10.0, 20.5, 10.0);
            b.line(8.0, 3.0, 8.0, 7.0);
            b.line(16.0, 3.0, 16.0, 7.0);
        }
        Icon::ZoomIn | Icon::ZoomOut => {
            b.circle(10.5, 10.5, 6.5);
            b.line(15.5, 15.5, 20.5, 20.5);
            b.line(7.5, 10.5, 13.5, 10.5);
            if icon == Icon::ZoomIn {
                b.line(10.5, 7.5, 10.5, 13.5);
            }
        }
        Icon::Rotate => {
            b.arc(12.0, 13.0, 7.0, 200.0, 270.0);
            b.poly(&[(3.0, 6.5), (4.6, 11.0), (9.0, 9.4)], false);
        }
        Icon::RotateRight => {
            b.arc(12.0, 13.0, 7.0, -20.0, -270.0);
            b.poly(&[(21.0, 6.5), (19.4, 11.0), (15.0, 9.4)], false);
        }
        Icon::Fullscreen => {
            b.poly(&[(4.0, 9.0), (4.0, 4.0), (9.0, 4.0)], false);
            b.poly(&[(15.0, 4.0), (20.0, 4.0), (20.0, 9.0)], false);
            b.poly(&[(20.0, 15.0), (20.0, 20.0), (15.0, 20.0)], false);
            b.poly(&[(9.0, 20.0), (4.0, 20.0), (4.0, 15.0)], false);
        }
        Icon::List => {
            for y in [6.0, 12.0, 18.0] {
                b.dot(5.0, y, 1.2);
                b.line(9.0, y, 20.0, y);
            }
        }
        Icon::User => {
            b.circle(12.0, 8.0, 4.0);
            b.arc(12.0, 21.0, 7.5, 180.0, 180.0);
        }
        Icon::Sun => {
            b.circle(12.0, 12.0, 4.0);
            let rad = core::f32::consts::PI / 180.0;
            for i in 0..8 {
                let a = i as f32 * 45.0 * rad;
                let (s, c) = (vmath::FloatExt::sin(a), vmath::FloatExt::cos(a));
                b.line(12.0 + c * 7.0, 12.0 + s * 7.0, 12.0 + c * 9.5, 12.0 + s * 9.5);
            }
        }
        Icon::Moon => {
            b.stroke.move_to(19.5, 14.5);
            b.stroke.cubic_to(15.0, 17.0, 8.0, 13.0, 10.0, 4.0);
            b.stroke.cubic_to(4.0, 6.0, 3.0, 13.0, 6.5, 17.0);
            b.stroke.cubic_to(10.0, 21.0, 17.0, 20.0, 19.5, 14.5);
            b.stroke.close();
        }
        Icon::Palette => {
            b.circle(12.0, 12.0, 9.0);
            b.dot(8.0, 9.0, 1.4);
            b.dot(12.0, 7.0, 1.4);
            b.dot(16.0, 9.0, 1.4);
            b.dot(15.5, 14.5, 1.4);
        }
        Icon::Chart => {
            b.poly(&[(3.5, 3.5), (3.5, 20.5), (20.5, 20.5)], false);
            b.poly(&[(7.0, 16.0), (11.0, 11.0), (14.0, 13.5), (19.5, 6.5)], false);
        }
        Icon::Car => {
            b.poly(&[(3.0, 16.0), (3.0, 12.5), (5.5, 7.5), (18.5, 7.5), (21.0, 12.5), (21.0, 16.0)], true);
            b.line(3.5, 12.5, 20.5, 12.5);
            b.circle(7.5, 16.5, 2.0);
            b.circle(16.5, 16.5, 2.0);
        }
        Icon::Rocket => {
            b.stroke.move_to(12.0, 2.5);
            b.stroke.cubic_to(16.5, 6.0, 16.5, 12.0, 15.0, 16.0);
            b.stroke.line_to(9.0, 16.0);
            b.stroke.cubic_to(7.5, 12.0, 7.5, 6.0, 12.0, 2.5);
            b.stroke.close();
            b.circle(12.0, 9.0, 1.8);
            b.poly(&[(9.0, 13.0), (5.5, 16.0), (5.5, 19.5), (9.0, 16.0)], false);
            b.poly(&[(15.0, 13.0), (18.5, 16.0), (18.5, 19.5), (15.0, 16.0)], false);
            b.line(12.0, 18.0, 12.0, 21.5);
        }
        Icon::Chess => {
            // A knight.
            b.poly(
                &[
                    (7.0, 20.0),
                    (17.0, 20.0),
                    (16.0, 17.0),
                    (16.5, 9.0),
                    (13.0, 4.0),
                    (10.0, 4.5),
                    (5.5, 9.5),
                    (7.0, 11.5),
                    (11.0, 10.0),
                    (8.0, 17.0),
                ],
                true,
            );
            b.dot(11.5, 7.5, 0.9);
        }
        Icon::Cube => {
            b.poly(&[(12.0, 3.0), (20.0, 7.5), (20.0, 16.5), (12.0, 21.0), (4.0, 16.5), (4.0, 7.5)], true);
            b.poly(&[(4.0, 7.5), (12.0, 12.0), (20.0, 7.5)], false);
            b.line(12.0, 12.0, 12.0, 21.0);
        }
        Icon::Lock => {
            b.rrect(5.0, 11.0, 14.0, 10.0, 2.0);
            b.arc(12.0, 11.0, 4.5, 180.0, 180.0);
        }
        Icon::Copy => {
            b.rrect(8.5, 8.5, 12.0, 12.0, 2.0);
            b.poly(
                &[(15.5, 8.5), (15.5, 5.5), (13.5, 3.5), (5.5, 3.5), (3.5, 5.5), (3.5, 13.5), (5.5, 15.5), (8.5, 15.5)],
                false,
            );
        }
        Icon::Cut => {
            b.circle(6.5, 17.5, 2.8);
            b.circle(17.5, 17.5, 2.8);
            b.line(8.5, 15.5, 18.0, 4.0);
            b.line(15.5, 15.5, 6.0, 4.0);
        }
        Icon::Paste => {
            b.rrect(5.0, 4.5, 14.0, 17.0, 2.0);
            b.rrect(9.0, 2.5, 6.0, 4.0, 1.0);
        }
        Icon::Undo => {
            b.poly(&[(8.0, 5.0), (4.0, 9.0), (8.0, 13.0)], false);
            b.stroke.move_to(4.0, 9.0);
            b.stroke.line_to(14.5, 9.0);
            b.stroke.cubic_to(18.5, 9.0, 20.5, 11.5, 20.5, 14.0);
            b.stroke.cubic_to(20.5, 16.5, 18.5, 19.0, 14.5, 19.0);
            b.stroke.line_to(10.0, 19.0);
        }
        Icon::Redo => {
            b.poly(&[(16.0, 5.0), (20.0, 9.0), (16.0, 13.0)], false);
            b.stroke.move_to(20.0, 9.0);
            b.stroke.line_to(9.5, 9.0);
            b.stroke.cubic_to(5.5, 9.0, 3.5, 11.5, 3.5, 14.0);
            b.stroke.cubic_to(3.5, 16.5, 5.5, 19.0, 9.5, 19.0);
            b.stroke.line_to(14.0, 19.0);
        }
    }
    b
}

/// Draws the Vindows logo (four rounded gradient tiles) filling the square `r`.
pub fn draw_logo(c: &mut Canvas, r: Rect) {
    let tile = (r.w - r.w / 8) / 2;
    let gap = r.w - 2 * tile;
    let colors = [
        (Color::hex(0x2FD4FF), Color::hex(0x3D8BFF)),
        (Color::hex(0x4D7CFF), Color::hex(0x7A5CFF)),
        (Color::hex(0x3D8BFF), Color::hex(0x6A63FF)),
        (Color::hex(0x7A5CFF), Color::hex(0xC04DFF)),
    ];
    for (i, (a, b)) in colors.iter().enumerate() {
        let x = r.x + (i as i32 % 2) * (tile + gap);
        let y = r.y + (i as i32 / 2) * (tile + gap);
        c.fill_rounded_rect_gradient(Rect::new(x, y, tile, tile), tile as f32 / 5.0, *a, *b);
    }
}

impl Icon {
    /// Draws the icon centred in `r` at `size` pixels with `color`.
    pub fn draw(self, c: &mut Canvas, r: Rect, size: f32, color: Color) {
        let b = build(self);
        let s = size / 24.0;
        let ox = r.x as f32 + (r.w as f32 - size) / 2.0;
        let oy = r.y as f32 + (r.h as f32 - size) / 2.0;
        let t = Transform::scale(s, s).then_translate(ox, oy);
        let width = (1.75 * s).max(1.1);
        let style = StrokeStyle::new(width / s).with_cap(LineCap::Round).with_join(LineJoin::Round);
        if !b.stroke.is_empty() {
            c.stroke_path_transformed(&b.stroke, &style, &t, color);
        }
        if !b.fill.is_empty() {
            c.fill_path_transformed(&b.fill, &t, color, FillRule::NonZero);
        }
    }

    /// Every icon (for galleries and tests).
    pub const ALL: [Icon; 68] = [
        Icon::Folder,
        Icon::File,
        Icon::Document,
        Icon::Image,
        Icon::Music,
        Icon::Play,
        Icon::Pause,
        Icon::Stop,
        Icon::Next,
        Icon::Previous,
        Icon::Shuffle,
        Icon::Repeat,
        Icon::Volume,
        Icon::Mute,
        Icon::Search,
        Icon::Settings,
        Icon::Power,
        Icon::Restart,
        Icon::Close,
        Icon::Plus,
        Icon::Minus,
        Icon::Check,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::ChevronUp,
        Icon::ChevronDown,
        Icon::Home,
        Icon::Terminal,
        Icon::Grid,
        Icon::Edit,
        Icon::Save,
        Icon::Open,
        Icon::Trash,
        Icon::Info,
        Icon::Warning,
        Icon::Star,
        Icon::Heart,
        Icon::Gamepad,
        Icon::Monitor,
        Icon::Cpu,
        Icon::Clock,
        Icon::Calendar,
        Icon::Refresh,
        Icon::ZoomIn,
        Icon::ZoomOut,
        Icon::Rotate,
        Icon::RotateRight,
        Icon::Fullscreen,
        Icon::List,
        Icon::User,
        Icon::Sun,
        Icon::Moon,
        Icon::Palette,
        Icon::Chart,
        Icon::Car,
        Icon::Rocket,
        Icon::Chess,
        Icon::Cube,
        Icon::Lock,
        Icon::Up,
        Icon::Download,
        Icon::Copy,
        Icon::Cut,
        Icon::Paste,
        Icon::Undo,
        Icon::Redo,
        Icon::Wallpaper,
        Icon::Speaker,
    ];

    /// Looks an icon up by its manifest name (`apps/*.app` `icon=` field).
    pub fn by_name(name: &str) -> Option<Icon> {
        Some(match name {
            "folder" | "files" => Icon::Folder,
            "file" => Icon::File,
            "document" | "editor" | "text" => Icon::Document,
            "image" | "photos" => Icon::Image,
            "music" => Icon::Music,
            "terminal" => Icon::Terminal,
            "settings" => Icon::Settings,
            "monitor" | "sysmon" | "taskmgr" => Icon::Chart,
            "gamepad" | "game" => Icon::Gamepad,
            "car" | "racer" => Icon::Car,
            "rocket" => Icon::Rocket,
            "chess" => Icon::Chess,
            "cube" => Icon::Cube,
            "info" | "about" => Icon::Info,
            "clock" => Icon::Clock,
            "calendar" => Icon::Calendar,
            "palette" => Icon::Palette,
            "warning" => Icon::Warning,
            _ => return None,
        })
    }
}
