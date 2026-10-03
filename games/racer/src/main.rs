//! Velocity: an arcade 3D racing game.
//!
//! A procedurally generated circuit with hills, banked corners, curbs and
//! scenery; four cars (one player, computer opponents following a racing
//! line); countdown start, laps and timing, positions, mini-map, pause menu
//! and results. Rendering uses the `v3d` software renderer.
//!
//! Controls: arrows/WASD drive, Space handbrake, Esc pause, Enter confirm,
//! F3 performance overlay.

#![no_std]
#![no_main]

extern crate alloc;

mod ai;
mod car;
mod hud;
mod model;
mod track;
mod world;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use v3d::app::{AppState, Game, Hud, Settings};
use v3d::{Billboard, Blend, Camera, Material, Mesh, Particle, ParticleSystem, Renderer, Texture, TextureId, Vertex};
use vgfx::Rect;
use vmath::{FloatExt, Mat4, Rng, Vec2, Vec3};
use vproto::input::keys;

use ai::Driver;
use car::{Car, Controls, TOP_SPEED};
use hud::{MiniMap, RaceInfo, ResultRow, Standing};
use model::CarModel;
use track::{HALF_WIDTH, Segment, Track};
use world::World;

vrt::entry!(main);

const NAMES: [&str; 6] = ["You", "Rossi", "Kimura", "Novak", "Silva", "Berg"];
const PAINTS: [(u32, u32); 6] = [
    (0xFFD8262A, 0xFFF4F4F4),
    (0xFF2A64DA, 0xFFF4F4F4),
    (0xFFEAB81E, 0xFF1E2024),
    (0xFF26A85A, 0xFFF4F4F4),
    (0xFF8A3AC8, 0xFFF2C23A),
    (0xFFEC7020, 0xFF1E2024),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Title,
    Countdown,
    Racing,
    Results,
}

/// Lap bookkeeping of one car.
#[derive(Clone, Copy, Debug)]
struct Progress {
    lap: u32,
    last_s: f32,
    halfway: bool,
    lap_start: f32,
    best: Option<f32>,
    finish: Option<f32>,
}

impl Progress {
    fn new(s: f32) -> Progress {
        Progress { lap: 0, last_s: s, halfway: true, lap_start: 0.0, best: None, finish: None }
    }
}

struct Textures {
    asphalt: TextureId,
    grass: TextureId,
    checker: TextureId,
    banner: TextureId,
    boards: Vec<TextureId>,
    crowd: TextureId,
    glow: TextureId,
    cloud: TextureId,
    shadow: TextureId,
    smoke: TextureId,
}

struct CamState {
    pos: Vec3,
    target: Vec3,
    fov: f32,
    yaw: f32,
}

struct Racer {
    variant: usize,
    track: Track,
    world: World,
    segments: Vec<Segment>,
    start_line: Mesh,
    models: Vec<CarModel>,
    cars: Vec<Car>,
    drivers: Vec<Driver>,
    progress: Vec<Progress>,
    tex: Textures,
    phase: Phase,
    phase_time: f32,
    paused: bool,
    menu: usize,
    laps: u32,
    opponents: usize,
    race_time: f32,
    cam: CamState,
    attract_car: usize,
    attract_timer: f32,
    attract_mode: u32,
    particles: ParticleSystem,
    sparks: ParticleSystem,
    smoke_timer: f32,
    message: Option<(String, f32)>,
    wrong_way: f32,
    map: MiniMap,
    rng: Rng,
    /// A circuit to generate at the next update (after a frame showed the
    /// "generating" note).
    pending_track: Option<usize>,
}

fn asphalt_texture() -> Texture {
    Texture::from_fn(128, 128, |x, y| {
        let n = v3d::texture::fbm_tiled(x, y, 128, 16, 3, 99);
        let g = 74 + (n * 34 / 256) as u32;
        let base = 0xFF00_0000 | g << 16 | g << 8 | (g + 4);
        let u = x as f32 / 128.0;
        // Edge lines and a dashed centre line.
        let edge = (0.035..0.07).contains(&u) || (0.93..0.965).contains(&u);
        let centre = (0.49..0.51).contains(&u) && y < 56;
        if edge || centre { 0xFFE8E8E0 } else { base }
    })
}

fn shadow_texture() -> Texture {
    Texture::from_fn(32, 64, |x, y| {
        // A soft rounded rectangle.
        let dx = ((x as f32 + 0.5) / 16.0 - 1.0).abs();
        let dy = ((y as f32 + 0.5) / 32.0 - 1.0).abs();
        let d = (dx.max(0.55) - 0.55) / 0.45;
        let e = (dy.max(0.75) - 0.75) / 0.25;
        let r = (d * d + e * e).sqrt();
        let a = (1.0 - r).clamp(0.0, 1.0);
        ((a * a * 200.0) as u32) << 24
    })
}

fn cloud_texture() -> Texture {
    Texture::from_fn(128, 64, |x, y| {
        let dx = (x as f32 + 0.5) / 64.0 - 1.0;
        let dy = (y as f32 + 0.5) / 32.0 - 1.0;
        let r = (dx * dx + dy * dy * 1.3).sqrt();
        let n = v3d::texture::fbm_tiled(x, y, 128, 8, 4, 5) as f32 / 256.0;
        let a = ((1.0 - r) * 1.6 + (n - 0.5) * 1.2).clamp(0.0, 1.0);
        ((a * a * 230.0) as u32) << 24 | 0x00FF_FFFF
    })
}

impl Racer {
    fn new(r: &mut Renderer) -> Racer {
        let (mut text, fonts) = vui::load_fonts();
        let bold = fonts[1];
        let tex = Textures {
            asphalt: r.add_texture(asphalt_texture()),
            grass: r.add_texture(Texture::noise(64, 3, 8, 3, 0xFFFFFFFF, 0xFFB4B4AC)),
            checker: r.add_texture(Texture::checker(16, 2, 0xFFF4F4F4, 0xFF141414)),
            banner: r.add_texture(world::text_texture(
                &mut text,
                bold,
                "VELOCITY",
                0xFF141820,
                0xFFFFFFFF,
                hud::ACCENT,
            )),
            boards: vec![
                r.add_texture(world::text_texture(&mut text, bold, "VINDOWS", 0xFF1E5AD8, 0xFFFFFFFF, 0xFFFFFFFF)),
                r.add_texture(world::text_texture(&mut text, bold, "V3D ENGINE", 0xFFD83A2A, 0xFFFFFFFF, 0xFFF2C23A)),
                r.add_texture(world::text_texture(&mut text, bold, "RUST + C++", 0xFF1A1C20, 0xFFF2C23A, 0xFFF2C23A)),
                r.add_texture(world::text_texture(&mut text, bold, "FIXED POINT", 0xFF26A85A, 0xFFFFFFFF, 0xFFFFFFFF)),
            ],
            crowd: r.add_texture(world::crowd_texture(17)),
            glow: r.add_texture(Texture::radial(64, 0xFFFFF4E0, 0.15)),
            cloud: r.add_texture(cloud_texture()),
            shadow: r.add_texture(shadow_texture()),
            smoke: r.add_texture(Texture::radial(32, 0xFFFFFFFF, 0.0)),
        };
        let models = PAINTS.iter().map(|&(p, s)| CarModel::new(p, s)).collect();
        let track = Track::generate(0);
        let world = World::new(&track, 11);
        let segments = track.build_segments(&|x, z| world.height(x, z));
        let start_line = track.start_line_mesh();
        let map = MiniMap::new(&track.outline, 0);
        vrt::println!("racer: track {} ({:.0} m, {} samples)", track.name, track.length, track.samples.len());
        let mut g = Racer {
            variant: 0,
            track,
            world,
            segments,
            start_line,
            models,
            cars: Vec::new(),
            drivers: Vec::new(),
            progress: Vec::new(),
            tex,
            phase: Phase::Title,
            phase_time: 0.0,
            paused: false,
            menu: 0,
            laps: 3,
            opponents: 3,
            race_time: 0.0,
            cam: CamState { pos: Vec3::new(0.0, 50.0, 0.0), target: Vec3::ZERO, fov: 1.0, yaw: 0.0 },
            attract_car: 1,
            attract_timer: 0.0,
            attract_mode: 0,
            particles: ParticleSystem::new(400),
            sparks: {
                let mut s = ParticleSystem::new(200);
                s.gravity = Vec3::new(0.0, -9.0, 0.0);
                s
            },
            smoke_timer: 0.0,
            message: None,
            wrong_way: 0.0,
            map,
            rng: Rng::new(42),
            pending_track: None,
        };
        g.place_grid();
        // Let the attract mode start with the cars already spread out.
        let n = g.cars.len();
        for i in 0..n {
            let s = g.track.length * i as f32 / n as f32;
            g.cars[i] = Car::new(&g.track, s, if i % 2 == 0 { 2.0 } else { -2.0 });
            g.progress[i] = Progress::new(s);
        }
        g.snap_camera();
        g
    }

    /// Generates another circuit.
    fn load_track(&mut self, variant: usize) {
        self.variant = variant;
        let t0 = vrt::time::now_ns();
        self.track = Track::generate(variant);
        self.world = World::new(&self.track, 11 + variant as u64 * 31);
        let world = &self.world;
        self.segments = self.track.build_segments(&|x, z| world.height(x, z));
        self.start_line = self.track.start_line_mesh();
        self.map = MiniMap::new(&self.track.outline, variant as u64);
        vrt::println!(
            "racer: track {} ({:.0} m) generated in {} ms",
            self.track.name,
            self.track.length,
            (vrt::time::now_ns() - t0) / 1_000_000
        );
        self.particles.clear();
        self.place_grid();
        let n = self.cars.len();
        for i in 0..n {
            let s = self.track.length * i as f32 / n as f32;
            self.cars[i] = Car::new(&self.track, s, 0.0);
            self.progress[i] = Progress::new(s);
        }
        self.snap_camera();
    }

    /// Puts the cars on the starting grid (player at the back).
    fn place_grid(&mut self) {
        let count = 1 + self.opponents;
        self.cars.clear();
        self.drivers.clear();
        self.progress.clear();
        let mut rng = Rng::new(7 + self.variant as u64);
        for i in 0..count {
            // Grid slot: AI cars in front, the player last.
            let slot = if i == 0 { count - 1 } else { i - 1 };
            let row = slot / 2;
            let lateral = if slot % 2 == 0 { 3.3 } else { -3.3 };
            let s = self.track.length - 14.0 - row as f32 * 10.0 - (slot % 2) as f32 * 4.0;
            self.cars.push(Car::new(&self.track, s, lateral));
            let skill =
                if i == 0 { 1.0 } else { 0.95 + 0.05 * (count - i) as f32 / count as f32 + rng.range_f32(-0.01, 0.01) };
            self.drivers.push(Driver::new(skill, rng.range_f32(-1.2, 1.2)));
            self.progress.push(Progress::new(s));
        }
    }

    fn start_race(&mut self) {
        self.place_grid();
        self.phase = Phase::Countdown;
        self.phase_time = 0.0;
        self.race_time = 0.0;
        self.paused = false;
        self.message = None;
        self.particles.clear();
        self.snap_camera();
    }

    fn snap_camera(&mut self) {
        let i = if self.phase == Phase::Title { self.attract_car.min(self.cars.len() - 1) } else { 0 };
        self.cam.yaw = self.cars[i].yaw;
        let (p, t) = chase_target(&self.cars[i], self.cam.yaw);
        self.cam.pos = p;
        self.cam.target = t;
    }

    /// Race distance covered by car `i`.
    fn distance(&self, i: usize) -> f32 {
        self.progress[i].lap as f32 * self.track.length + self.cars[i].s
    }

    /// Car indices ordered by race position.
    fn order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.cars.len()).collect();
        idx.sort_by(|&a, &b| {
            let (pa, pb) = (&self.progress[a], &self.progress[b]);
            match (pa.finish, pb.finish) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(core::cmp::Ordering::Equal),
                (Some(_), None) => core::cmp::Ordering::Less,
                (None, Some(_)) => core::cmp::Ordering::Greater,
                (None, None) => self.distance(b).partial_cmp(&self.distance(a)).unwrap_or(core::cmp::Ordering::Equal),
            }
        });
        idx
    }

    fn say(&mut self, msg: &str) {
        self.message = Some((String::from(msg), 2.5));
    }

    /// Lap counting for car `i` after it moved.
    fn track_laps(&mut self, i: usize) {
        let len = self.track.length;
        let s = self.cars[i].s;
        let p = &mut self.progress[i];
        if s > len * 0.4 && s < len * 0.6 {
            p.halfway = true;
        }
        let crossed = p.last_s > len * 0.75 && s < len * 0.25;
        p.last_s = s;
        if !crossed || !p.halfway || p.finish.is_some() {
            return;
        }
        p.halfway = false;
        let now = self.race_time;
        if p.lap >= 1 {
            let lap_time = now - p.lap_start;
            let best = p.best.is_none_or(|b| lap_time < b);
            if best {
                p.best = Some(lap_time);
            }
            if i == 0 && best && p.lap >= 2 {
                self.message = Some((String::from("BEST LAP"), 2.5));
            }
        }
        let p = &mut self.progress[i];
        p.lap += 1;
        p.lap_start = now;
        if p.lap > self.laps {
            p.finish = Some(now);
            if i == 0 {
                let pos = self.order().iter().position(|&c| c == 0).unwrap_or(0) + 1;
                let msg = match pos {
                    1 => String::from("YOU WIN!"),
                    _ => format!(
                        "FINISHED {}{}",
                        pos,
                        match pos {
                            2 => "ND",
                            3 => "RD",
                            _ => "TH",
                        }
                    ),
                };
                self.message = Some((msg, 4.0));
                self.phase_time = 0.0;
            }
        } else if i == 0 && self.phase == Phase::Racing {
            if p.lap == self.laps && self.laps > 1 {
                self.say("FINAL LAP");
            } else if p.lap > 1 {
                let msg = format!("LAP {}", p.lap);
                self.say(&msg);
            }
        }
    }

    /// Steps every car: the player from the keyboard (unless `auto`), the
    /// others (and the player when `auto`) by their drivers.
    fn simulate(&mut self, dt: f32, player: Option<Controls>, frozen: bool) {
        let steps = ((dt / (1.0 / 90.0)).ceil() as usize).clamp(1, 8);
        let h = dt / steps as f32;
        let world = &self.world;
        let ground = |x: f32, z: f32| world.height(x, z);
        let player_dist = self.distance(0);
        let n = self.cars.len();
        let mut controls = vec![Controls::default(); n];
        for (i, control) in controls.iter_mut().enumerate() {
            *control = match (i, player) {
                (0, Some(c)) => c,
                _ => {
                    // Rubber banding towards the player during a race.
                    let pace = if self.phase == Phase::Racing && player.is_some() {
                        1.0 + ((player_dist - self.distance(i)) / 500.0).clamp(-0.07, 0.06)
                    } else {
                        1.0
                    };
                    let d = &mut self.drivers[i];
                    d.drive(i, &self.cars, &self.track, pace, dt)
                }
            };
            if self.progress[i].finish.is_some() && i != 0 {
                control.throttle *= 0.6;
            }
        }
        if frozen {
            return;
        }
        for _ in 0..steps {
            for (car, control) in self.cars.iter_mut().zip(&controls) {
                car.step(control, h, &self.track, &ground);
            }
            car::collide(&mut self.cars);
        }
        for i in 0..n {
            self.track_laps(i);
        }
        self.emit_smoke(dt, &controls);
    }

    /// Tyre smoke when sliding, dust on the grass.
    fn emit_smoke(&mut self, dt: f32, controls: &[Controls]) {
        self.smoke_timer += dt;
        if self.smoke_timer < 0.04 {
            return;
        }
        self.smoke_timer = 0.0;
        for (i, c) in self.cars.iter().enumerate() {
            let speed = c.speed().abs();
            let sliding = c.slide > 3.5 || (controls[i].handbrake && speed > 10.0);
            let dusty = c.on_grass && speed > 8.0;
            if c.scrape > 0.25 && speed > 6.0 {
                let side = if c.lateral > 0.0 { 1.0 } else { -1.0 };
                let m = c.matrix();
                for k in 0..3 {
                    let p = m.transform_point3(Vec3::new(side * 0.95, 0.4, 1.2 - k as f32 * 1.1));
                    let v = Vec3::new(c.vel.x, 2.0, c.vel.y) * 0.4 + self.rng.unit_vec3() * 4.0;
                    let mut s = Particle::new(p, v, 0.35, 0.35, 0xFFFFC060);
                    s.size_end = 0.1;
                    s.color_end = 0x00FF4010;
                    self.sparks.emit(s);
                }
            }
            if !sliding && !dusty {
                continue;
            }
            let m = c.matrix();
            for w in [2usize, 3] {
                let p = m.transform_point3(car::WHEELS[w] - Vec3::new(0.0, car::WHEEL_RADIUS - 0.15, 0.0));
                let vel = Vec3::new(c.vel.x, 0.0, c.vel.y) * 0.25 + Vec3::new(0.0, 1.2, 0.0);
                let (color, size) = if dusty { (0x90A08A60, 1.6) } else { (0x80D8D8DC, 1.2) };
                let mut p = Particle::new(p, vel, 1.4, size, color);
                p.size_end = size * 3.5;
                p.color_end = color & 0x00FF_FFFF;
                p.rotation = self.rng.range_f32(0.0, vmath::TAU);
                p.spin = self.rng.range_f32(-1.0, 1.0);
                self.particles.emit(p);
            }
        }
    }

    fn player_controls(app: &AppState) -> Controls {
        let k = &app.keys;
        let left = k.any_down(&[keys::LEFT, keys::A]);
        let right = k.any_down(&[keys::RIGHT, keys::D]);
        Controls {
            throttle: if k.any_down(&[keys::UP, keys::W]) { 1.0 } else { 0.0 },
            brake: if k.any_down(&[keys::DOWN, keys::S]) { 1.0 } else { 0.0 },
            steer: (left as i32 - right as i32) as f32,
            handbrake: k.down(keys::SPACE),
        }
    }

    fn update_camera(&mut self, dt: f32) {
        let follow = if self.phase == Phase::Title { self.attract_car.min(self.cars.len() - 1) } else { 0 };
        let car = &self.cars[follow];
        // The chase camera trails the car's heading with a lag (so drifts
        // and turns show the car's side) but never its position, so the
        // distance stays the same at any speed.
        let dyaw = vmath::wrap_angle(car.yaw - self.cam.yaw);
        self.cam.yaw += dyaw * (1.0 - (-dt * 4.5).exp());
        let (mut want_pos, want_target, snap) = match (self.phase, self.attract_mode % 3) {
            (Phase::Title, 1) => {
                // Trackside camera ahead of the car, looking back at it.
                let s = (car.s + 60.0) % self.track.length;
                let p = self.track.point(s, -HALF_WIDTH - 9.0) + Vec3::new(0.0, 3.5, 0.0);
                (p, car.pos + Vec3::new(0.0, 1.0, 0.0), true)
            }
            (Phase::Title, 2) => {
                // Slow orbit.
                let a = self.attract_timer * 0.15 + 1.0;
                (car.pos + Vec3::new(a.sin() * 26.0, 9.0, a.cos() * 26.0), car.pos + Vec3::new(0.0, 1.0, 0.0), true)
            }
            _ => {
                let (p, t) = chase_target(car, self.cam.yaw);
                (p, t, true)
            }
        };
        let ground = self.world.height(want_pos.x, want_pos.z).max(self.track.locate(want_pos, Some(car.index)).height);
        want_pos.y = want_pos.y.max(ground + 1.2);
        // Only the height is smoothed (bumps and crests).
        let ky = 1.0 - (-dt * 8.0).exp();
        let y = self.cam.pos.y + (want_pos.y - self.cam.pos.y) * ky;
        if snap {
            self.cam.pos = Vec3::new(want_pos.x, y, want_pos.z);
        }
        self.cam.target = want_target;
        let speed = car.speed().abs();
        let want_fov = 1.0 + (speed / TOP_SPEED).min(1.0) * 0.22;
        self.cam.fov += (want_fov - self.cam.fov) * (1.0 - (-dt * 3.0).exp());
    }
}

/// Chase camera position and look-at point for a car seen along `yaw`.
fn chase_target(car: &Car, yaw: f32) -> (Vec3, Vec3) {
    let fwd = Vec3::new(yaw.sin(), 0.0, yaw.cos());
    (car.pos - fwd * 7.0 + Vec3::new(0.0, 2.6, 0.0), car.pos + fwd * 6.0 + Vec3::new(0.0, 1.0, 0.0))
}

const TITLE_ITEMS: usize = 5;
const PAUSE_ITEMS: [&str; 3] = ["Resume", "Restart race", "Quit to menu"];

impl Game for Racer {
    fn update(&mut self, app: &mut AppState, dt: f32) {
        if let Some(v) = self.pending_track.take() {
            // The previous frame showed the "generating" note.
            self.load_track(v);
            return;
        }
        let k = &app.keys;
        let up = k.any_pressed(&[keys::UP, keys::W]);
        let down = k.any_pressed(&[keys::DOWN, keys::S]);
        let left = k.any_pressed(&[keys::LEFT, keys::A]);
        let right = k.any_pressed(&[keys::RIGHT, keys::D]);
        let enter = k.any_pressed(&[keys::ENTER, keys::KPENTER]);
        let esc = k.pressed(keys::ESC);
        if let Some((_, t)) = &mut self.message {
            *t -= dt;
            if *t <= 0.0 {
                self.message = None;
            }
        }
        match self.phase {
            Phase::Title => {
                if up {
                    self.menu = (self.menu + TITLE_ITEMS - 1) % TITLE_ITEMS;
                }
                if down {
                    self.menu = (self.menu + 1) % TITLE_ITEMS;
                }
                let delta = right as i32 - left as i32;
                if delta != 0 {
                    match self.menu {
                        1 => {
                            let v = (self.variant as i32 + delta).rem_euclid(3) as usize;
                            self.pending_track = Some(v);
                        }
                        2 => self.laps = (self.laps as i32 + delta).clamp(1, 9) as u32,
                        3 => {
                            self.opponents = (self.opponents as i32 + delta).clamp(1, 5) as usize;
                            self.place_grid();
                            let n = self.cars.len();
                            for i in 0..n {
                                let s = self.track.length * i as f32 / n as f32;
                                self.cars[i] = Car::new(&self.track, s, 0.0);
                                self.progress[i] = Progress::new(s);
                            }
                        }
                        _ => {}
                    }
                }
                if enter {
                    match self.menu {
                        0 => self.start_race(),
                        1 => {
                            let v = (self.variant + 1) % 3;
                            self.pending_track = Some(v);
                        }
                        4 => app.quit = true,
                        _ => {}
                    }
                }
                if esc {
                    app.quit = true;
                }
                if self.phase == Phase::Title {
                    // Attract mode: everybody is driven by the computer.
                    self.simulate(dt, None, false);
                    self.attract_timer += dt;
                    if self.attract_timer > 7.0 {
                        self.attract_timer = 0.0;
                        self.attract_mode += 1;
                        self.attract_car = (self.attract_car + 1) % self.cars.len();
                        self.snap_camera();
                    }
                }
            }
            Phase::Countdown | Phase::Racing if self.paused => {
                if up {
                    self.menu = (self.menu + PAUSE_ITEMS.len() - 1) % PAUSE_ITEMS.len();
                }
                if down {
                    self.menu = (self.menu + 1) % PAUSE_ITEMS.len();
                }
                if esc {
                    self.paused = false;
                }
                if enter {
                    match self.menu {
                        0 => self.paused = false,
                        1 => self.start_race(),
                        _ => {
                            self.paused = false;
                            self.phase = Phase::Title;
                            self.menu = 0;
                            self.snap_camera();
                        }
                    }
                }
                return;
            }
            Phase::Countdown => {
                if esc || !app.focused {
                    self.paused = true;
                    self.menu = 0;
                }
                self.phase_time += dt;
                self.simulate(dt, Some(Controls::default()), true);
                if self.phase_time >= 3.0 {
                    self.phase = Phase::Racing;
                    self.phase_time = 0.0;
                    for p in &mut self.progress {
                        p.lap_start = 0.0;
                    }
                }
            }
            Phase::Racing => {
                if esc || !app.focused {
                    self.paused = true;
                    self.menu = 0;
                    return;
                }
                self.race_time += dt;
                self.phase_time += dt;
                let finished = self.progress[0].finish.is_some();
                let input = if finished || app.autopilot { None } else { Some(Racer::player_controls(app)) };
                // After the finish the computer takes over the player's car.
                self.simulate(dt, input, false);
                // Wrong way warning.
                let c = &self.cars[0];
                let dir = self.track.at(c.index as isize).dir;
                let heading = Vec2::new(c.yaw.sin(), c.yaw.cos());
                let along = heading.dot(Vec2::new(dir.x, dir.z).normalize_or(Vec2::Y));
                if along < -0.3 && c.speed() > 3.0 {
                    self.wrong_way += dt;
                } else {
                    self.wrong_way = 0.0;
                }
                let all_done = self.progress.iter().all(|p| p.finish.is_some());
                if finished && (self.phase_time > 6.0 || all_done) {
                    self.phase = Phase::Results;
                    self.phase_time = 0.0;
                    let place = self.order().iter().position(|&i| i == 0).unwrap_or(0) + 1;
                    vrt::println!("racer: results (finished {} of {})", place, self.cars.len());
                }
            }
            Phase::Results => {
                self.race_time += dt;
                self.phase_time += dt;
                self.simulate(dt, None, false);
                if enter && self.phase_time > 0.5 {
                    self.phase = Phase::Title;
                    self.menu = 0;
                    self.snap_camera();
                } else if k.pressed(keys::R) {
                    self.start_race();
                } else if esc {
                    self.phase = Phase::Title;
                    self.menu = 0;
                    self.snap_camera();
                }
            }
        }
        self.particles.update(dt);
        self.sparks.update(dt);
        self.update_camera(dt);
    }

    fn render(&mut self, r: &mut Renderer) {
        let forward = (self.cam.target - self.cam.pos).normalize_or(Vec3::Z);
        let camera =
            Camera { position: self.cam.pos, forward, up: Vec3::Y, fov_y: self.cam.fov, near: 0.35, far: 3400.0 };
        let t = &self.tex;
        let terrain = Material::lambert(0xFFFFFFFF).with_texture(t.grass).with_bilinear().with_sort_offset(150.0);
        // Far away the grass texture averages out: plain vertex colours, tinted
        // like the texture's mean, are much cheaper.
        let terrain_far = Material::lambert(0xFFDADAD6).with_sort_offset(150.0);
        let scenery = Material::lambert(0xFFFFFFFF);
        let road = Material::lambert(0xFFFFFFFF).with_texture(t.asphalt).with_lod_bias(2).with_bilinear();
        let curb = Material::lambert(0xFFFFFFFF);
        let verge = Material::lambert(0xFFFFFFFF).with_texture(t.grass).with_bilinear();
        let start = Material::lambert(0xFFFFFFFF).with_texture(t.checker);
        let mountains = Material::lambert(0xFFFFFFFF).without_fog();
        let paint = Material::phong(0xFFFFFFFF, 32.0, Vec3::splat(0.6));
        let wheel = Material::lambert(0xFFFFFFFF);
        let brake = Material::unlit(0xFFFFFFFF);
        let shadow = Material::unlit(0xFFFFFFFF).with_texture(t.shadow).with_blend(Blend::Alpha).affine();
        let smoke =
            Material::lambert(0xFFFFFFFF).with_texture(t.smoke).with_blend(Blend::Alpha).affine().with_bilinear();
        let cloud = Material::unlit(0xFFFFFFFF)
            .with_texture(t.cloud)
            .with_blend(Blend::Alpha)
            .without_fog()
            .affine()
            .with_bilinear();
        let sun = Material::glow(0xFFFFFFFF).with_texture(t.glow).without_fog().affine().with_bilinear();
        let glow = Material::glow(0xFFFFFFFF).with_texture(t.glow).affine().with_bilinear();

        let mut f = r.frame(&camera, &self.world.env);
        let cam2 = Vec2::new(camera.position.x, camera.position.z);
        for tile in &self.world.tiles {
            let d = Vec2::new(tile.center.x, tile.center.z).distance(cam2);
            if d < 520.0 {
                f.draw(&tile.near, &Mat4::IDENTITY, &terrain);
            } else {
                f.draw(&tile.far, &Mat4::IDENTITY, &terrain_far);
            }
            let near_trees = d < 380.0;
            let trees = if near_trees { &tile.scenery } else { &tile.scenery_far };
            if let Some(s) = trees
                && d < 1150.0
            {
                f.draw(s, &Mat4::IDENTITY, &scenery);
            }
        }
        for seg in &self.segments {
            f.draw(&seg.road, &Mat4::IDENTITY, &road);
            if let Some(c) = &seg.curbs {
                f.draw(c, &Mat4::IDENTITY, &curb);
            }
            f.draw(&seg.verge, &Mat4::IDENTITY, &verge);
        }
        f.draw(&self.start_line, &Mat4::IDENTITY, &start);
        f.draw(&self.world.gantry, &Mat4::IDENTITY, &scenery);
        f.draw(&self.world.banner, &Mat4::IDENTITY, &Material::lambert(0xFFFFFFFF).with_texture(t.banner));
        f.draw(&self.world.stand, &Mat4::IDENTITY, &scenery);
        f.draw(&self.world.crowd, &Mat4::IDENTITY, &Material::lambert(0xFFFFFFFF).with_texture(t.crowd).double_sided());
        for (i, b) in self.world.boards.iter().enumerate() {
            f.draw(b, &Mat4::IDENTITY, &Material::lambert(0xFFFFFFFF).with_texture(t.boards[i % t.boards.len()]));
        }
        f.draw(&self.world.mountains, &Mat4::IDENTITY, &mountains);

        // Cars, their shadows and brake-light glows.
        let mut shadow_v: Vec<Vertex> = Vec::with_capacity(self.cars.len() * 4);
        let mut shadow_i: Vec<u32> = Vec::with_capacity(self.cars.len() * 6);
        let mut glows: Vec<Billboard> = Vec::new();
        for (i, c) in self.cars.iter().enumerate() {
            let m = c.matrix();
            let model = &self.models[i % self.models.len()];
            f.draw(&model.body, &m, &paint);
            for w in 0..4 {
                f.draw(&model.wheel, &c.wheel_matrix(&m, w), &wheel);
            }
            if c.braking {
                f.draw(&model.brake_lights, &m, &brake);
                for x in [-0.62f32, 0.62] {
                    glows.push(Billboard::new(m.transform_point3(Vec3::new(x, 0.64, -2.3)), 0.9, 0xC0FF3020));
                }
            }
            // Blob shadow on the ground under the car.
            let fwd = Vec3::new(c.yaw.sin(), 0.0, c.yaw.cos());
            let up = c.up;
            let f2 = (fwd - up * fwd.dot(up)).normalize_or(fwd);
            let l2 = up.cross(f2);
            let base = shadow_v.len() as u32;
            let ground = Vec3::new(c.pos.x, c.pos.y + 0.06, c.pos.z);
            let corners =
                [(-1.15, -2.45, 0.0, 1.0), (1.15, -2.45, 1.0, 1.0), (1.15, 2.45, 1.0, 0.0), (-1.15, 2.45, 0.0, 0.0)];
            for (x, z, u, v) in corners {
                shadow_v.push(Vertex::new(ground + l2 * x + f2 * z, up, Vec2::new(u, v), 0xFFFFFFFF));
            }
            shadow_i.extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
        }
        f.draw_triangles(&shadow_v, &shadow_i, &shadow);
        if !glows.is_empty() {
            f.billboards(&glow, &glows);
        }
        let mut smoke_b = Vec::new();
        self.particles.billboards(&mut smoke_b);
        f.billboards(&smoke, &smoke_b);
        smoke_b.clear();
        self.sparks.billboards(&mut smoke_b);
        f.billboards(&glow, &smoke_b);
        f.billboards(&cloud, &self.world.clouds);
        let sun_dir = self.world.env.sun_direction.normalize();
        let sun_pos = camera.position + sun_dir * 3000.0;
        f.billboards(&sun, &[Billboard::new(sun_pos, 520.0, 0xFFFFF0D0), Billboard::new(sun_pos, 1500.0, 0x60FFD8A0)]);
        f.finish();
        // Menus over the race sit on a darkened scene (darkening the internal
        // image is much cheaper than dimming the window).
        if self.paused {
            r.darken(0x90);
        } else if self.phase == Phase::Results {
            r.darken(0x60);
        }
    }

    fn hud(&mut self, h: &mut Hud) {
        match self.phase {
            Phase::Title => {
                let x = 56.0;
                // Short windows pull the logo and menu up so nothing overlaps.
                let dy = (h.height as f32 - 470.0).clamp(-60.0, 0.0);
                hud::logo(h, x, 118.0 + dy);
                let tag = "ARCADE RACING ON A SOFTWARE RASTERIZER";
                h.text(x + 2.0, 158.0 + dy, 14.0, v3d::app::HudFont::Regular, 0xFFE0E6EE, 0, tag);
                let items = [
                    String::from("Start race"),
                    format!("Track:  < {} >", self.track.name),
                    format!("Laps:  < {} >", self.laps),
                    format!("Opponents:  < {} >", self.opponents),
                    String::from("Quit"),
                ];
                hud::menu_items(h, &items, self.menu, x as i32, 196.0 + dy, 320);
                let menu_bottom = 196.0 + dy + items.len() as f32 * 46.0;
                let dots: Vec<(Vec2, u32, bool)> = self
                    .cars
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (Vec2::new(c.pos.x, c.pos.z), PAINTS[i].0, false))
                    .collect();
                let pr = Rect::new(h.width - 218, 70, 182, 182);
                hud::track_preview(h, pr, self.track.name, self.track.length, &self.map, &dots);
                if self.pending_track.is_some() {
                    hud::banner(h, "GENERATING TRACK", 1.0);
                }
                if h.height as f32 - 44.0 >= menu_bottom {
                    hud::hint(h, "Arrows/WASD drive   Space handbrake   Esc pause   Enter select   F3 stats");
                }
            }
            Phase::Countdown | Phase::Racing | Phase::Results => {
                // The pause menu and the results replace the race HUD.
                if !self.paused && self.phase != Phase::Results {
                    self.race_hud(h);
                    if self.phase == Phase::Countdown {
                        let lit = (self.phase_time as u32 + 1).min(3);
                        hud::countdown(h, lit, false, 1.0);
                    } else if self.phase == Phase::Racing && self.phase_time < 1.2 && self.progress[0].lap <= 1 {
                        hud::countdown(h, 3, true, 1.0 - (self.phase_time - 0.6).max(0.0) / 0.6);
                    }
                }
                if self.phase == Phase::Results && !self.paused {
                    let order = self.order();
                    let rows: Vec<ResultRow> = order
                        .iter()
                        .map(|&i| ResultRow {
                            name: NAMES[i],
                            color: PAINTS[i].0,
                            player: i == 0,
                            total: self.progress[i].finish,
                            best: self.progress[i].best,
                        })
                        .collect();
                    hud::results(h, &rows, self.track.name);
                    hud::hint(h, "Enter: main menu    R: race again");
                }
                if self.paused {
                    let items: Vec<String> = PAUSE_ITEMS.iter().map(|s| String::from(*s)).collect();
                    hud::menu(h, "PAUSED", self.track.name, &items, self.menu, h.height as f32 * 0.3);
                }
            }
        }
    }
}

impl Racer {
    fn race_hud(&mut self, h: &mut Hud) {
        let order = self.order();
        let standings: Vec<Standing> = order
            .iter()
            .map(|&i| Standing {
                name: NAMES[i],
                color: PAINTS[i].0,
                player: i == 0,
                finished: self.progress[i].finish.is_some(),
            })
            .collect();
        let dots: Vec<(Vec2, u32, bool)> =
            self.cars.iter().enumerate().map(|(i, c)| (Vec2::new(c.pos.x, c.pos.z), PAINTS[i].0, i == 0)).collect();
        let me = &self.cars[0];
        let kmh = me.speed().abs() * 3.6;
        let bands = [0.0, 45.0, 80.0, 115.0, 150.0, 190.0, 260.0];
        let mut gear = 1;
        while gear < 6 && kmh > bands[gear] {
            gear += 1;
        }
        let (lo, hi) = (bands[gear - 1], bands[gear]);
        let rpm = 0.3 + 0.7 * ((kmh - lo) / (hi - lo)).clamp(0.0, 1.0);
        let p = &self.progress[0];
        let lap_time = if self.phase == Phase::Countdown { 0.0 } else { (self.race_time - p.lap_start).max(0.0) };
        let race_time = p.finish.unwrap_or(self.race_time);
        let msg = self.message.as_ref().map(|(m, t)| (m.as_str(), t.min(1.0)));
        let info = RaceInfo {
            position: order.iter().position(|&i| i == 0).unwrap_or(0) + 1,
            cars: self.cars.len(),
            lap: p.lap.max(1),
            laps: self.laps,
            speed_kmh: kmh,
            rpm,
            gear: gear as u32,
            race_time: if self.phase == Phase::Countdown { 0.0 } else { race_time },
            lap_time: if p.finish.is_some() { 0.0 } else { lap_time },
            best_lap: p.best,
            standings: &standings,
            map: &self.map,
            dots: &dots,
            message: msg,
            wrong_way: self.wrong_way > 1.2 && self.phase == Phase::Racing,
        };
        hud::race(h, &info);
    }
}

fn main() -> i32 {
    let mut s = Settings::new("Velocity", "racer");
    s.log_name = "racer";
    v3d::app::run(s, Racer::new)
}
