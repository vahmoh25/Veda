//! Starfall: a 3D space shooter.
//!
//! Fly through asteroid fields and waves of enemy ships against a nebula.
//! The world streams towards the player; the ship moves in a box in front of
//! the camera. Shields regenerate, the hull does not; pickups restore them
//! or upgrade the guns. Rendering uses the `v3d` software renderer.
//!
//! Controls: arrows/WASD fly, Space fire, Esc pause, Enter confirm, F3
//! performance overlay.

#![no_std]
#![no_main]

extern crate alloc;

mod hud;
mod models;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use v3d::app::{AppState, Game, Hud, Settings};
use v3d::{
    Background, Beam, Billboard, Blend, Burst, Camera, Environment, Fog, Material, Mesh, ParticleSystem, PointLight,
    Renderer, Texture, TextureId,
};
use vmath::{FloatExt, Mat4, Quat, Rng, Vec3};
use vproto::input::keys;

vrt::entry!(main);

/// Half extents of the area the ship can fly in.
const BOUND_X: f32 = 15.0;
const BOUND_Y: f32 = 8.5;
/// Speed at which the world streams past (m/s).
const SPEED: f32 = 46.0;
/// Where things appear and disappear.
const SPAWN_Z: f32 = -520.0;
const DESPAWN_Z: f32 = 40.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Title,
    Playing,
    GameOver,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Dart,
    Saucer,
    Heavy,
    Kamikaze,
}

impl Kind {
    fn radius(self) -> f32 {
        match self {
            Kind::Dart | Kind::Kamikaze => 2.7,
            Kind::Saucer => 3.4,
            Kind::Heavy => 5.0,
        }
    }

    fn score(self) -> u64 {
        match self {
            Kind::Dart => 100,
            Kind::Kamikaze => 150,
            Kind::Saucer => 250,
            Kind::Heavy => 600,
        }
    }
}

struct Enemy {
    kind: Kind,
    pos: Vec3,
    vel: Vec3,
    hp: f32,
    age: f32,
    fire: f32,
    phase: f32,
    /// Hover depth (saucers, heavies) and lateral anchor.
    anchor: Vec3,
    flash: f32,
    leave: f32,
}

struct Rock {
    pos: Vec3,
    vel: Vec3,
    rot: Quat,
    axis: Vec3,
    spin: f32,
    scale: f32,
    hp: f32,
    mesh: usize,
}

struct Shot {
    pos: Vec3,
    vel: Vec3,
    enemy: bool,
    life: f32,
    damage: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PickupKind {
    Shield,
    Repair,
    Weapon,
}

struct Pickup {
    kind: PickupKind,
    pos: Vec3,
    age: f32,
}

struct Flash {
    pos: Vec3,
    color: Vec3,
    radius: f32,
    life: f32,
    max: f32,
}

struct Ring {
    pos: Vec3,
    age: f32,
    life: f32,
    size: f32,
    color: u32,
}

struct Popup {
    pos: Vec3,
    text: String,
    age: f32,
}

struct Ship {
    pos: Vec3,
    vel: Vec3,
    roll: f32,
    pitch: f32,
    shield: f32,
    hull: f32,
    weapon: u32,
    fire: f32,
    gun: bool,
    since_hit: f32,
    hit_flash: f32,
    shield_flash: f32,
    alive: bool,
}

impl Ship {
    fn new() -> Ship {
        Ship {
            pos: Vec3::new(0.0, -1.0, 0.0),
            vel: Vec3::ZERO,
            roll: 0.0,
            pitch: 0.0,
            shield: 100.0,
            hull: 100.0,
            weapon: 1,
            fire: 0.0,
            gun: false,
            since_hit: 10.0,
            hit_flash: 0.0,
            shield_flash: 0.0,
            alive: true,
        }
    }

    fn matrix(&self) -> Mat4 {
        Mat4::from_rotation_translation(Quat::from_rotation_z(self.roll) * Quat::from_rotation_x(self.pitch), self.pos)
    }
}

/// One scheduled spawn of a wave.
#[derive(Clone, Copy, Debug)]
enum Spawn {
    Darts { n: u32, x: f32, y: f32 },
    Saucer { x: f32, y: f32 },
    Heavy { x: f32 },
    Kamikaze { x: f32, y: f32 },
    Rocks { seconds: f32, rate: f32 },
}

struct Meshes {
    player: Mesh,
    dart: Mesh,
    saucer: Mesh,
    heavy: Mesh,
    rocks: Vec<Mesh>,
    crystal: Mesh,
    sky: Mesh,
    planet: Mesh,
    bubble: Mesh,
}

struct Textures {
    nebula: TextureId,
    glow: TextureId,
    ring: TextureId,
    bolt: TextureId,
    smoke: TextureId,
    planet: TextureId,
}

struct Star {
    dir: Vec3,
    size: f32,
    color: u32,
    twinkle: f32,
}

struct Starfall {
    m: Meshes,
    t: Textures,
    phase: Phase,
    paused: bool,
    menu: usize,
    ship: Ship,
    enemies: Vec<Enemy>,
    rocks: Vec<Rock>,
    shots: Vec<Shot>,
    pickups: Vec<Pickup>,
    flashes: Vec<Flash>,
    rings: Vec<Ring>,
    popups: Vec<Popup>,
    fire: ParticleSystem,
    smoke: ParticleSystem,
    sparks: ParticleSystem,
    stars: Vec<Star>,
    dust: Vec<Vec3>,
    score: u64,
    high: u64,
    combo: u32,
    combo_timer: f32,
    level: u32,
    wave: u32,
    queue: Vec<(f32, Spawn)>,
    wave_time: f32,
    rock_stream: (f32, f32),
    time: f32,
    phase_time: f32,
    message: Option<(String, f32)>,
    shake: f32,
    rng: Rng,
    cam_pos: Vec3,
}

const PAUSE_ITEMS: [&str; 3] = ["Resume", "Restart", "Quit to title"];

impl Starfall {
    fn new(r: &mut Renderer) -> Starfall {
        let t = Textures {
            nebula: r.add_texture(models::nebula_texture(5)),
            glow: r.add_texture(Texture::radial(64, 0xFFFFFFFF, 0.25)),
            ring: r.add_texture(models::ring_texture()),
            bolt: r.add_texture(models::bolt_texture()),
            smoke: r.add_texture(Texture::radial(32, 0xFFFFFFFF, 0.0)),
            planet: r.add_texture(models::planet_texture()),
        };
        let m = Meshes {
            player: models::player_ship(),
            dart: models::dart(),
            saucer: models::saucer(),
            heavy: models::heavy(),
            rocks: (0..5).map(|k| models::asteroid(100 + k)).collect(),
            crystal: models::crystal(),
            sky: models::sky_sphere(900.0),
            planet: v3d::shapes::sphere(1.0, 24, 16, 0xFFFFFFFF).build(),
            bubble: v3d::shapes::icosphere(1.0, 2, 0xFFFFFFFF).build(),
        };
        let mut rng = Rng::new(2024);
        let stars = (0..260)
            .map(|_| {
                let dir = rng.unit_vec3();
                let tint = [0xFFFFFFFF, 0xFFCCE0FF, 0xFFFFF0D0, 0xFFFFD8D8][rng.below(4) as usize];
                Star { dir, size: rng.range_f32(2.6, 6.0), color: tint, twinkle: rng.range_f32(0.0, vmath::TAU) }
            })
            .collect();
        let dust = (0..70)
            .map(|_| Vec3::new(rng.range_f32(-40.0, 40.0), rng.range_f32(-25.0, 25.0), rng.range_f32(-300.0, 20.0)))
            .collect();
        let mut fire = ParticleSystem::new(700);
        fire.drag = 1.6;
        let mut smoke = ParticleSystem::new(300);
        smoke.drag = 1.0;
        let mut sparks = ParticleSystem::new(500);
        sparks.drag = 0.6;
        let mut g = Starfall {
            m,
            t,
            phase: Phase::Title,
            paused: false,
            menu: 0,
            ship: Ship::new(),
            enemies: Vec::new(),
            rocks: Vec::new(),
            shots: Vec::new(),
            pickups: Vec::new(),
            flashes: Vec::new(),
            rings: Vec::new(),
            popups: Vec::new(),
            fire,
            smoke,
            sparks,
            stars,
            dust,
            score: 0,
            high: 0,
            combo: 0,
            combo_timer: 0.0,
            level: 1,
            wave: 0,
            queue: Vec::new(),
            wave_time: 0.0,
            rock_stream: (0.0, 0.0),
            time: 0.0,
            phase_time: 0.0,
            message: None,
            shake: 0.0,
            rng,
            cam_pos: Vec3::new(0.0, 3.0, 15.0),
        };
        // Some scenery for the title screen.
        g.rock_stream = (1.0e9, 0.6);
        for _ in 0..14 {
            let z = g.rng.range_f32(SPAWN_Z, -40.0);
            g.spawn_rock(z, 0.0);
        }
        g
    }

    fn reset(&mut self) {
        self.ship = Ship::new();
        self.enemies.clear();
        self.rocks.clear();
        self.shots.clear();
        self.pickups.clear();
        self.flashes.clear();
        self.rings.clear();
        self.popups.clear();
        self.fire.clear();
        self.smoke.clear();
        self.sparks.clear();
        self.score = 0;
        self.combo = 0;
        self.combo_timer = 0.0;
        self.level = 1;
        self.wave = 0;
        self.queue.clear();
        self.rock_stream = (0.0, 0.0);
        self.time = 0.0;
        self.phase = Phase::Playing;
        self.phase_time = 0.0;
        self.paused = false;
        self.say("GET READY");
    }

    fn say(&mut self, s: &str) {
        self.message = Some((String::from(s), 2.6));
    }

    // -- Spawning ------------------------------------------------------------

    fn spawn_rock(&mut self, z: f32, drift: f32) {
        let rng = &mut self.rng;
        let scale = rng.range_f32(1.2, 4.6);
        let pos = Vec3::new(rng.range_f32(-34.0, 34.0), rng.range_f32(-20.0, 20.0), z);
        let vel =
            Vec3::new(rng.range_f32(-1.0, 1.0) * drift, rng.range_f32(-1.0, 1.0) * drift, rng.range_f32(0.0, 8.0));
        self.rocks.push(Rock {
            pos,
            vel,
            rot: Quat::from_euler(rng.range_f32(0.0, 6.0), rng.range_f32(0.0, 6.0), 0.0),
            axis: rng.unit_vec3(),
            spin: rng.range_f32(0.2, 1.4),
            scale,
            hp: scale * 1.6,
            mesh: rng.below(5) as usize,
        });
    }

    fn spawn_enemy(&mut self, kind: Kind, pos: Vec3) {
        let hp = match kind {
            Kind::Dart => 2.0,
            Kind::Kamikaze => 2.0,
            Kind::Saucer => 5.0 + self.level as f32,
            Kind::Heavy => 16.0 + 3.0 * self.level as f32,
        };
        let hover = match kind {
            Kind::Saucer => self.rng.range_f32(-95.0, -70.0),
            Kind::Heavy => -120.0,
            _ => 0.0,
        };
        let phase = self.rng.range_f32(0.0, vmath::TAU);
        let fire = self.rng.range_f32(1.0, 3.0);
        self.enemies.push(Enemy {
            kind,
            pos,
            vel: Vec3::ZERO,
            hp,
            age: 0.0,
            fire,
            phase,
            anchor: Vec3::new(pos.x, pos.y, hover),
            flash: 0.0,
            leave: 0.0,
        });
    }

    /// Queues the next wave.
    fn next_wave(&mut self) {
        self.wave += 1;
        if self.wave > 5 {
            self.wave = 1;
            self.level += 1;
            self.ship.hull = (self.ship.hull + 25.0).min(100.0);
            let msg = format!("LEVEL {}", self.level);
            self.say(&msg);
        } else if self.wave > 1 || self.level > 1 {
            let msg = format!("WAVE {}", self.wave);
            self.say(&msg);
        }
        vrt::println!("starfall: level {} wave {} (score {})", self.level, self.wave, self.score);
        let l = self.level as f32;
        let rng = &mut self.rng;
        let mut q: Vec<(f32, Spawn)> = Vec::new();
        match self.wave {
            1 => {
                for k in 0..(3 + self.level.min(4)) {
                    q.push((
                        k as f32 * 2.2,
                        Spawn::Darts { n: 3 + (k % 3), x: rng.range_f32(-10.0, 10.0), y: rng.range_f32(-5.0, 5.0) },
                    ));
                }
            }
            2 => {
                q.push((0.0, Spawn::Rocks { seconds: 14.0, rate: 2.2 + l * 0.4 }));
                for k in 0..self.level.min(5) {
                    q.push((
                        3.0 + k as f32 * 2.5,
                        Spawn::Kamikaze { x: rng.range_f32(-12.0, 12.0), y: rng.range_f32(-6.0, 6.0) },
                    ));
                }
            }
            3 => {
                for k in 0..(1 + self.level.min(3)) {
                    q.push((
                        k as f32 * 3.0,
                        Spawn::Saucer { x: rng.range_f32(-11.0, 11.0), y: rng.range_f32(-5.0, 5.0) },
                    ));
                }
                q.push((2.0, Spawn::Darts { n: 4, x: 0.0, y: 2.0 }));
            }
            4 => {
                q.push((0.0, Spawn::Heavy { x: rng.range_f32(-6.0, 6.0) }));
                for k in 0..3 {
                    q.push((
                        2.0 + k as f32 * 3.0,
                        Spawn::Darts { n: 3, x: rng.range_f32(-10.0, 10.0), y: rng.range_f32(-5.0, 5.0) },
                    ));
                }
                if self.level >= 2 {
                    q.push((6.0, Spawn::Saucer { x: 9.0, y: 3.0 }));
                }
            }
            _ => {
                q.push((0.0, Spawn::Rocks { seconds: 10.0, rate: 3.0 + l * 0.5 }));
                for k in 0..(2 + self.level) {
                    q.push((
                        1.0 + k as f32 * 1.6,
                        Spawn::Kamikaze { x: rng.range_f32(-12.0, 12.0), y: rng.range_f32(-6.0, 6.0) },
                    ));
                }
                q.push((4.0, Spawn::Darts { n: 5, x: 0.0, y: 0.0 }));
                if self.level >= 3 {
                    q.push((8.0, Spawn::Heavy { x: 0.0 }));
                }
            }
        }
        q.reverse();
        self.queue = q;
        self.wave_time = 0.0;
    }

    fn run_queue(&mut self, dt: f32) {
        self.wave_time += dt;
        while let Some(&(t, s)) = self.queue.last() {
            if t > self.wave_time {
                break;
            }
            self.queue.pop();
            match s {
                Spawn::Darts { n, x, y } => {
                    for k in 0..n {
                        let off = k as f32 - (n - 1) as f32 / 2.0;
                        let p = Vec3::new(x + off * 3.2, y + off.abs() * -1.2, SPAWN_Z - off.abs() * 10.0);
                        self.spawn_enemy(Kind::Dart, p);
                    }
                }
                Spawn::Saucer { x, y } => self.spawn_enemy(Kind::Saucer, Vec3::new(x, y, SPAWN_Z)),
                Spawn::Heavy { x } => self.spawn_enemy(Kind::Heavy, Vec3::new(x, 1.0, SPAWN_Z)),
                Spawn::Kamikaze { x, y } => self.spawn_enemy(Kind::Kamikaze, Vec3::new(x, y, SPAWN_Z)),
                Spawn::Rocks { seconds, rate } => self.rock_stream = (seconds, rate),
            }
        }
        // Asteroid stream.
        if self.rock_stream.0 > 0.0 {
            self.rock_stream.0 -= dt;
            if self.rng.next_f32() < self.rock_stream.1 * dt {
                self.spawn_rock(SPAWN_Z, 2.0);
            }
        }
        let done = self.queue.is_empty() && self.enemies.is_empty() && self.rock_stream.0 <= 0.0;
        if self.phase == Phase::Playing && (done || self.wave_time > 45.0) {
            self.next_wave();
        }
    }

    // -- Effects --------------------------------------------------------------

    fn explode(&mut self, pos: Vec3, size: f32, color: u32) {
        let rng = &mut self.rng;
        let n = (14.0 * size) as u32 + 6;
        self.fire.burst(
            rng,
            pos,
            Vec3::new(0.0, 0.0, SPEED * 0.7),
            &Burst {
                count: n,
                speed: (3.0 * size, 14.0 * size),
                life: (0.35, 0.9),
                size_start: 1.6 * size,
                size_end: 3.6 * size,
                color_start: color,
                color_end: 0x00A02008,
                spread: 0.6 * size,
            },
        );
        self.sparks.burst(
            rng,
            pos,
            Vec3::new(0.0, 0.0, SPEED * 0.7),
            &Burst {
                count: n,
                speed: (16.0, 42.0),
                life: (0.3, 0.8),
                size_start: 0.5,
                size_end: 0.15,
                color_start: 0xFFFFF0B0,
                color_end: 0x00FF8020,
                spread: 0.3 * size,
            },
        );
        self.smoke.burst(
            rng,
            pos,
            Vec3::new(0.0, 0.0, SPEED * 0.8),
            &Burst {
                count: (6.0 * size) as u32 + 2,
                speed: (1.0, 5.0 * size),
                life: (0.9, 1.8),
                size_start: 2.0 * size,
                size_end: 6.0 * size,
                color_start: 0x7040383A,
                color_end: 0x00201C1E,
                spread: size,
            },
        );
        self.rings.push(Ring { pos, age: 0.0, life: 0.45, size: 9.0 * size, color: color | 0xFF00_0000 });
        let c = v3d::rgb_of(color);
        self.flashes.push(Flash { pos, color: c * 3.0, radius: 26.0 * size, life: 0.45, max: 0.45 });
        let d = pos.distance(self.ship.pos);
        self.shake = self.shake.max((size * 6.0 / (d + 6.0)).min(1.0));
    }

    fn add_score(&mut self, points: u64, at: Vec3) {
        if self.phase != Phase::Playing {
            return;
        }
        self.combo = (self.combo + 1).min(40);
        self.combo_timer = 2.5;
        let mult = 1 + (self.combo / 5).min(3) as u64;
        let p = points * mult;
        self.score += p;
        self.high = self.high.max(self.score);
        self.popups.push(Popup { pos: at, text: format!("+{}", p), age: 0.0 });
    }

    fn damage_ship(&mut self, amount: f32) {
        if !self.ship.alive || self.phase != Phase::Playing {
            return;
        }
        let s = &mut self.ship;
        s.since_hit = 0.0;
        self.combo = 0;
        if s.shield > 0.0 {
            let absorbed = amount.min(s.shield);
            s.shield -= absorbed;
            s.shield_flash = 1.0;
            s.hull -= (amount - absorbed) * 1.0;
        } else {
            s.hull -= amount;
        }
        s.hit_flash = 1.0;
        self.shake = self.shake.max(0.6);
        if s.hull <= 0.0 {
            s.hull = 0.0;
            s.alive = false;
            let p = s.pos;
            // The ship is close to the camera: modest sizes still fill the view.
            self.explode(p, 2.0, 0xFFFFC060);
            self.explode(p + Vec3::new(1.5, 0.5, 0.0), 1.2, 0xFFFF8040);
            self.phase = Phase::GameOver;
            vrt::println!("starfall: game over (score {}, level {}, wave {})", self.score, self.level, self.wave);
            self.phase_time = 0.0;
        }
    }

    fn drop_pickup(&mut self, pos: Vec3, chance: f32) {
        if self.rng.next_f32() > chance {
            return;
        }
        let kind = match self.rng.below(10) {
            0..=3 => PickupKind::Shield,
            4..=6 => PickupKind::Repair,
            _ => PickupKind::Weapon,
        };
        self.pickups.push(Pickup { kind, pos, age: 0.0 });
    }

    // -- Simulation -----------------------------------------------------------

    fn update_ship(&mut self, app: &AppState, dt: f32, autopilot: bool) {
        let s = &mut self.ship;
        s.hit_flash = (s.hit_flash - dt * 2.5).max(0.0);
        s.shield_flash = (s.shield_flash - dt * 3.0).max(0.0);
        if !s.alive {
            return;
        }
        let (mut ix, mut iy) = (0.0f32, 0.0f32);
        let fire;
        if autopilot {
            // Chase the nearest enemy (or rock) ahead and keep firing.
            let ahead = |p: Vec3| p.z < -15.0 && p.z > -260.0;
            let target = self
                .enemies
                .iter()
                .filter(|e| ahead(e.pos))
                .map(|e| e.pos)
                .chain(self.rocks.iter().filter(|r| ahead(r.pos) && r.pos.x.abs() < BOUND_X + 3.0).map(|r| r.pos))
                .min_by(|a, b| (-a.z).partial_cmp(&(-b.z)).unwrap_or(core::cmp::Ordering::Equal));
            match target {
                Some(p) => {
                    ix = ((p.x - s.pos.x) * 0.4).clamp(-1.0, 1.0);
                    iy = ((p.y - s.pos.y) * 0.4).clamp(-1.0, 1.0);
                    fire = true;
                }
                None => {
                    ix = (self.time * 0.7).sin() * 0.6 - s.pos.x * 0.05;
                    iy = (self.time * 0.45).sin() * 0.5 - s.pos.y * 0.05;
                    fire = false;
                }
            }
        } else {
            let k = &app.keys;
            if k.any_down(&[keys::LEFT, keys::A]) {
                ix -= 1.0;
            }
            if k.any_down(&[keys::RIGHT, keys::D]) {
                ix += 1.0;
            }
            if k.any_down(&[keys::UP, keys::W]) {
                iy += 1.0;
            }
            if k.any_down(&[keys::DOWN, keys::S]) {
                iy -= 1.0;
            }
            fire = k.down(keys::SPACE);
        }
        let want = Vec3::new(ix * 24.0, iy * 18.0, 0.0);
        s.vel += (want - s.vel) * (1.0 - (-dt * 7.0).exp());
        s.pos += s.vel * dt;
        if s.pos.x.abs() > BOUND_X {
            s.pos.x = s.pos.x.clamp(-BOUND_X, BOUND_X);
            s.vel.x = 0.0;
        }
        if s.pos.y.abs() > BOUND_Y {
            s.pos.y = s.pos.y.clamp(-BOUND_Y, BOUND_Y);
            s.vel.y = 0.0;
        }
        let k = 1.0 - (-dt * 8.0).exp();
        s.roll += ((-s.vel.x * 0.035).clamp(-0.8, 0.8) - s.roll) * k;
        s.pitch += ((s.vel.y * 0.02).clamp(-0.35, 0.35) - s.pitch) * k;
        s.since_hit += dt;
        if s.since_hit > 2.5 {
            s.shield = (s.shield + 9.0 * dt).min(100.0);
        }
        // Guns.
        s.fire -= dt;
        if fire && s.fire <= 0.0 {
            s.fire = 0.11;
            s.gun = !s.gun;
            let side = if s.gun { 1.0 } else { -1.0 };
            let m = s.matrix();
            let mut guns: Vec<(Vec3, Vec3)> = Vec::new();
            let fwd = Vec3::new(0.0, 0.0, -170.0);
            match s.weapon {
                1 => guns.push((Vec3::new(1.5 * side, -0.1, -1.0), fwd)),
                2 => {
                    guns.push((Vec3::new(1.5, -0.1, -1.0), fwd));
                    guns.push((Vec3::new(-1.5, -0.1, -1.0), fwd));
                }
                _ => {
                    guns.push((Vec3::new(1.5, -0.1, -1.0), fwd));
                    guns.push((Vec3::new(-1.5, -0.1, -1.0), fwd));
                    guns.push((Vec3::new(3.3, -0.1, 0.5), Vec3::new(14.0, 0.0, -168.0)));
                    guns.push((Vec3::new(-3.3, -0.1, 0.5), Vec3::new(-14.0, 0.0, -168.0)));
                }
            }
            for (p, v) in guns {
                let pos = m.transform_point3(p);
                self.shots.push(Shot { pos, vel: v, enemy: false, life: 1.7, damage: 1.0 });
                self.flashes.push(Flash { pos, color: Vec3::new(0.3, 1.2, 1.6), radius: 9.0, life: 0.06, max: 0.06 });
            }
        }
    }

    fn update_world(&mut self, dt: f32) {
        let ship = self.ship.pos;
        let alive = self.ship.alive && self.phase == Phase::Playing;
        // Asteroids.
        for r in &mut self.rocks {
            r.pos += (r.vel + Vec3::new(0.0, 0.0, SPEED)) * dt;
            r.rot = (Quat::from_axis_angle(r.axis, r.spin * dt) * r.rot).normalize();
        }
        // Enemies.
        let level = self.level as f32;
        let mut enemy_shots: Vec<Shot> = Vec::new();
        for e in &mut self.enemies {
            e.age += dt;
            e.flash = (e.flash - dt * 8.0).max(0.0);
            match e.kind {
                Kind::Dart => {
                    let sway = (e.age * 1.6 + e.phase).sin() * 7.0;
                    e.vel = Vec3::new((e.anchor.x + sway - e.pos.x) * 1.5, (e.anchor.y - e.pos.y) * 1.5, SPEED + 14.0);
                }
                Kind::Kamikaze => {
                    let to = ship - e.pos;
                    let k = if e.pos.z < -60.0 { 0.9 } else { 0.25 };
                    e.vel = Vec3::new(to.x * k, to.y * k, SPEED + 30.0 + level * 3.0);
                }
                Kind::Saucer | Kind::Heavy => {
                    let hover = e.anchor.z;
                    let strafe = (e.age * 0.7 + e.phase).sin() * if e.kind == Kind::Saucer { 9.0 } else { 5.0 };
                    let stay = if e.kind == Kind::Saucer { 11.0 } else { 20.0 };
                    let target_z = if e.age > stay {
                        e.leave += dt;
                        hover + e.leave * e.leave * 30.0
                    } else {
                        hover
                    };
                    let vz = ((target_z - e.pos.z) * 1.2).clamp(-30.0, 140.0) + 0.0;
                    e.vel = Vec3::new((e.anchor.x + strafe - e.pos.x) * 1.2, (e.anchor.y - e.pos.y) * 1.2, vz);
                    if e.age > stay {
                        e.vel.y += 4.0;
                    }
                }
            }
            e.pos += e.vel * dt;
            // Shooting.
            e.fire -= dt;
            if e.fire <= 0.0 && alive && e.pos.z < -25.0 && e.pos.z > -300.0 {
                let aim = (ship - e.pos).normalize_or(Vec3::Z);
                let speed = 70.0 + level * 6.0;
                match e.kind {
                    Kind::Dart => {
                        e.fire = 2.6 - (level * 0.15).min(1.2);
                        enemy_shots.push(Shot {
                            pos: e.pos,
                            vel: aim * speed + Vec3::new(0.0, 0.0, SPEED * 0.5),
                            enemy: true,
                            life: 4.0,
                            damage: 10.0,
                        });
                    }
                    Kind::Saucer => {
                        e.fire = 1.7;
                        for k in -1..=1 {
                            let dir = (aim + Vec3::new(k as f32 * 0.06, 0.0, 0.0)).normalize();
                            enemy_shots.push(Shot {
                                pos: e.pos,
                                vel: dir * speed,
                                enemy: true,
                                life: 4.0,
                                damage: 9.0,
                            });
                        }
                    }
                    Kind::Heavy => {
                        e.fire = 1.2;
                        for k in -2..=2 {
                            let dir =
                                (aim + Vec3::new(k as f32 * 0.09, (k as f32 * 0.7).sin() * 0.03, 0.0)).normalize();
                            enemy_shots.push(Shot {
                                pos: e.pos + Vec3::new(k as f32 * 0.8, 0.0, 2.0),
                                vel: dir * speed * 0.9,
                                enemy: true,
                                life: 4.0,
                                damage: 12.0,
                            });
                        }
                    }
                    Kind::Kamikaze => e.fire = 99.0,
                }
            }
        }
        self.shots.extend(enemy_shots);
        // Shots.
        for s in &mut self.shots {
            s.pos += s.vel * dt;
            s.life -= dt;
        }
        self.collisions(dt);
        // Pickups.
        for p in &mut self.pickups {
            p.age += dt;
            p.pos.z += SPEED * 0.8 * dt;
        }
        let ship = self.ship.pos;
        let mut collected = Vec::new();
        self.pickups.retain(|p| {
            if alive && p.pos.distance(ship) < 3.2 {
                collected.push(p.kind);
                return false;
            }
            p.pos.z < DESPAWN_Z
        });
        for k in collected {
            match k {
                PickupKind::Shield => {
                    self.ship.shield = (self.ship.shield + 50.0).min(100.0);
                    self.say("SHIELD +50");
                }
                PickupKind::Repair => {
                    self.ship.hull = (self.ship.hull + 30.0).min(100.0);
                    self.say("HULL REPAIRED");
                }
                PickupKind::Weapon => {
                    if self.ship.weapon < 3 {
                        self.ship.weapon += 1;
                        self.say("WEAPON UPGRADE");
                    } else {
                        self.add_score(500, ship);
                        self.say("BONUS +500");
                    }
                }
            }
            let p = self.ship.pos;
            self.flashes.push(Flash { pos: p, color: Vec3::new(0.6, 1.6, 2.0), radius: 20.0, life: 0.4, max: 0.4 });
        }
        // Clean up.
        self.rocks.retain(|r| r.pos.z < DESPAWN_Z && r.hp > 0.0);
        self.enemies.retain(|e| e.pos.z < DESPAWN_Z && e.hp > 0.0 && e.pos.y < 60.0);
        self.shots.retain(|s| s.life > 0.0 && s.pos.z < DESPAWN_Z && s.pos.z > SPAWN_Z - 50.0);
        for f in &mut self.flashes {
            f.life -= dt;
        }
        self.flashes.retain(|f| f.life > 0.0);
        for r in &mut self.rings {
            r.age += dt;
            r.pos.z += SPEED * 0.7 * dt;
        }
        self.rings.retain(|r| r.age < r.life);
        for p in &mut self.popups {
            p.age += dt;
            p.pos.y += dt * 3.0;
            p.pos.z += SPEED * 0.5 * dt;
        }
        self.popups.retain(|p| p.age < 1.0);
        self.fire.update(dt);
        self.smoke.update(dt);
        self.sparks.update(dt);
        // Space dust streams past.
        for d in &mut self.dust {
            d.z += SPEED * 2.0 * dt;
            if d.z > 20.0 {
                d.z -= 320.0;
            }
        }
        self.combo_timer -= dt;
        if self.combo_timer <= 0.0 {
            self.combo = 0;
        }
        self.shake = (self.shake - dt * 2.0).max(0.0);
    }

    fn collisions(&mut self, dt: f32) {
        let alive = self.ship.alive && self.phase == Phase::Playing;
        let ship = self.ship.pos;
        let mut kills: Vec<(Vec3, f32, u32, u64, f32)> = Vec::new(); // pos, size, color, score, pickup chance
        let mut hits: Vec<Vec3> = Vec::new();
        let mut ship_damage = 0.0;
        let mut new_rocks: Vec<(Vec3, f32)> = Vec::new();
        for s in &mut self.shots {
            if s.life <= 0.0 {
                continue;
            }
            // Swept test over this frame's motion.
            let a = s.pos - s.vel * dt;
            let b = s.pos;
            if s.enemy {
                if alive && segment_hits(a, b, ship, 1.5) {
                    s.life = 0.0;
                    ship_damage += s.damage;
                }
                continue;
            }
            for e in &mut self.enemies {
                if e.hp > 0.0 && segment_hits(a, b, e.pos, e.kind.radius()) {
                    s.life = 0.0;
                    e.hp -= s.damage;
                    e.flash = 1.0;
                    hits.push(s.pos);
                    if e.hp <= 0.0 {
                        let (size, color) = match e.kind {
                            Kind::Dart | Kind::Kamikaze => (1.1, 0xFFFFA040),
                            Kind::Saucer => (1.5, 0xFFC080FF),
                            Kind::Heavy => (2.4, 0xFFFFB050),
                        };
                        let chance = match e.kind {
                            Kind::Heavy => 1.0,
                            Kind::Saucer => 0.45,
                            _ => 0.12,
                        };
                        kills.push((e.pos, size, color, e.kind.score(), chance));
                    }
                    break;
                }
            }
            if s.life <= 0.0 {
                continue;
            }
            for r in &mut self.rocks {
                if r.hp > 0.0 && segment_hits(a, b, r.pos, r.scale * 1.05) {
                    s.life = 0.0;
                    r.hp -= s.damage;
                    hits.push(s.pos);
                    if r.hp <= 0.0 {
                        kills.push((r.pos, r.scale * 0.45, 0xFFD8B080, (60.0 / r.scale) as u64 * 10 + 10, 0.04));
                        if r.scale > 2.6 {
                            new_rocks.push((r.pos, r.scale * 0.5));
                        }
                    }
                    break;
                }
            }
        }
        // Ramming.
        if alive {
            for e in &mut self.enemies {
                if e.hp > 0.0 && e.pos.distance(ship) < e.kind.radius() + 1.4 {
                    ship_damage += if e.kind == Kind::Heavy { 40.0 } else { 25.0 };
                    e.hp = 0.0;
                    kills.push((e.pos, 1.3, 0xFFFFA040, e.kind.score() / 2, 0.0));
                }
            }
            for r in &mut self.rocks {
                if r.hp > 0.0 && r.pos.distance(ship) < r.scale * 0.95 + 1.3 {
                    ship_damage += 12.0 + r.scale * 6.0;
                    r.hp = 0.0;
                    kills.push((r.pos, r.scale * 0.45, 0xFFD8B080, 0, 0.0));
                }
            }
        }
        for p in hits {
            self.sparks.burst(
                &mut self.rng,
                p,
                Vec3::new(0.0, 0.0, SPEED * 0.6),
                &Burst {
                    count: 6,
                    speed: (6.0, 18.0),
                    life: (0.15, 0.35),
                    size_start: 0.4,
                    size_end: 0.1,
                    color_start: 0xFFC0F0FF,
                    color_end: 0x0040A0FF,
                    spread: 0.2,
                },
            );
            self.flashes.push(Flash { pos: p, color: Vec3::new(0.6, 1.2, 1.6), radius: 10.0, life: 0.1, max: 0.1 });
        }
        for (pos, size, color, score, chance) in kills {
            self.explode(pos, size, color);
            if score > 0 {
                self.add_score(score, pos);
            }
            self.drop_pickup(pos, chance);
        }
        for (pos, scale) in new_rocks {
            for k in 0..2 {
                let dir = if k == 0 { 1.0 } else { -1.0 };
                let mesh = self.rng.below(5) as usize;
                self.rocks.push(Rock {
                    pos: pos + Vec3::new(dir * scale, 0.0, 0.0),
                    vel: Vec3::new(dir * 6.0, self.rng.range_f32(-3.0, 3.0), 4.0),
                    rot: Quat::from_rotation_y(self.rng.range_f32(0.0, 6.0)),
                    axis: self.rng.unit_vec3(),
                    spin: self.rng.range_f32(0.8, 2.0),
                    scale,
                    hp: scale * 1.6,
                    mesh,
                });
            }
        }
        if ship_damage > 0.0 {
            self.damage_ship(ship_damage);
        }
    }

    fn camera(&self) -> Camera {
        let s = &self.ship;
        let shake = if self.shake > 0.0 {
            let t = self.time * 47.0;
            Vec3::new(t.sin(), (t * 1.3).cos(), 0.0) * (self.shake * 0.5)
        } else {
            Vec3::ZERO
        };
        let pos = self.cam_pos + shake;
        let look = Vec3::new(s.pos.x * 0.72, s.pos.y * 0.72 + 0.6, -40.0);
        let up = Vec3::new((-s.roll * 0.18).sin(), 1.0, 0.0).normalize();
        Camera { position: pos, forward: (look - pos).normalize(), up, fov_y: 1.05, near: 0.5, far: 1200.0 }
    }
}

/// Does the segment a-b pass within `r` of `c`?
fn segment_hits(a: Vec3, b: Vec3, c: Vec3, r: f32) -> bool {
    let ab = b - a;
    let t = ((c - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
    (a + ab * t).distance_squared(c) < r * r
}

impl Game for Starfall {
    fn update(&mut self, app: &mut AppState, dt: f32) {
        let k = &app.keys;
        let enter = k.any_pressed(&[keys::ENTER, keys::KPENTER]);
        let esc = k.pressed(keys::ESC);
        let up = k.any_pressed(&[keys::UP, keys::W]);
        let down = k.any_pressed(&[keys::DOWN, keys::S]);
        if let Some((_, t)) = &mut self.message {
            *t -= dt;
            if *t <= 0.0 {
                self.message = None;
            }
        }
        if self.paused {
            if up {
                self.menu = (self.menu + PAUSE_ITEMS.len() - 1) % PAUSE_ITEMS.len();
            }
            if down {
                self.menu = (self.menu + 1) % PAUSE_ITEMS.len();
            }
            if esc {
                self.paused = false;
            } else if enter {
                match self.menu {
                    0 => self.paused = false,
                    1 => self.reset(),
                    _ => {
                        self.paused = false;
                        self.phase = Phase::Title;
                        self.ship = Ship::new();
                        self.enemies.clear();
                        self.shots.clear();
                        self.rock_stream = (1.0e9, 0.6);
                    }
                }
            }
            return;
        }
        self.time += dt;
        self.phase_time += dt;
        match self.phase {
            Phase::Title => {
                if enter || k.pressed(keys::SPACE) {
                    self.reset();
                    return;
                }
                if esc {
                    app.quit = true;
                }
                self.update_ship(app, dt, true);
                self.run_queue(dt);
            }
            Phase::Playing => {
                if esc || !app.focused {
                    self.paused = true;
                    self.menu = 0;
                    return;
                }
                if self.wave == 0 && self.phase_time > 2.0 {
                    self.next_wave();
                }
                self.update_ship(app, dt, app.autopilot);
                self.run_queue(dt);
            }
            Phase::GameOver => {
                if self.phase_time > 1.0 && enter {
                    self.reset();
                    return;
                }
                if esc {
                    self.phase = Phase::Title;
                    self.ship = Ship::new();
                    self.rock_stream = (1.0e9, 0.6);
                }
                // Only fades the damage flashes now that the ship is gone.
                self.update_ship(app, dt, false);
                self.run_queue(dt);
            }
        }
        self.update_world(dt);
        // Camera follows the ship partly.
        let s = &self.ship;
        let want = Vec3::new(s.pos.x * 0.55, s.pos.y * 0.55 + 3.4, 15.0);
        self.cam_pos += (want - self.cam_pos) * (1.0 - (-dt * 5.0).exp());
    }

    fn render(&mut self, r: &mut Renderer) {
        let camera = self.camera();
        let t = &self.t;
        let mut env = Environment {
            sun_direction: Vec3::new(0.55, 0.65, 0.35).normalize(),
            sun_color: Vec3::new(1.15, 1.02, 0.9),
            sky_ambient: Vec3::new(0.28, 0.24, 0.42),
            ground_ambient: Vec3::new(0.08, 0.1, 0.16),
            point_lights: Vec::new(),
            fog: Some(Fog { color: Vec3::new(0.035, 0.04, 0.08), start: 220.0, end: 560.0, max: 1.0 }),
            background: Background::Solid(0xFF06070E),
        };
        for f in &self.flashes {
            let k = (f.life / f.max).clamp(0.0, 1.0);
            env.point_lights.push(PointLight { position: f.pos, color: f.color * k, radius: f.radius });
        }
        let ship_light = Material::phong(0xFFFFFFFF, 32.0, Vec3::splat(0.7));
        let sky =
            Material::unlit(0xFFFFFFFF).with_texture(t.nebula).without_fog().with_cull(v3d::Cull::Back).with_bilinear();
        let planet = Material::lambert(0xFFFFFFFF).with_texture(t.planet).without_fog().with_bilinear();
        let glow = Material::glow(0xFFFFFFFF).with_texture(t.glow).affine().with_bilinear();
        let bolt = Material::glow(0xFFFFFFFF).with_texture(t.bolt).affine();
        let ring = Material::glow(0xFFFFFFFF).with_texture(t.ring).affine().with_bilinear();
        let smoke =
            Material::lambert(0xFFFFFFFF).with_texture(t.smoke).with_blend(Blend::Alpha).affine().with_bilinear();
        let rock = Material::lambert(0xFFFFFFFF);

        let mut f = r.frame(&camera, &env);
        // Background: nebula, a distant planet, stars.
        let cam_t = Mat4::from_translation(camera.position);
        f.draw_sky(&self.m.sky, &cam_t, &sky);
        let planet_pos = camera.position + Vec3::new(-0.62, 0.22, -0.75).normalize() * 700.0;
        let planet_m = Mat4::from_scale_rotation_translation(
            Vec3::splat(150.0),
            Quat::from_rotation_y(self.time * 0.01) * Quat::from_rotation_z(0.3),
            planet_pos,
        );
        f.draw(&self.m.planet, &planet_m, &planet);
        let stars: Vec<Billboard> = self
            .stars
            .iter()
            .map(|s| {
                let tw = 0.75 + 0.25 * (self.time * 2.0 + s.twinkle).sin();
                let a = ((tw * 255.0) as u32) << 24;
                Billboard::new(camera.position + s.dir * 800.0, s.size, a | (s.color & 0xFFFFFF))
            })
            .collect();
        f.billboards(&Material::glow(0xFFFFFFFF).without_fog(), &stars);
        // Asteroids.
        for rk in &self.rocks {
            let m = Mat4::from_scale_rotation_translation(Vec3::splat(rk.scale), rk.rot, rk.pos);
            f.draw(&self.m.rocks[rk.mesh], &m, &rock);
        }
        // Enemies (flash white when hit).
        let mut engine_glows: Vec<Billboard> = Vec::new();
        for e in &self.enemies {
            let mesh = match e.kind {
                Kind::Dart | Kind::Kamikaze => &self.m.dart,
                Kind::Saucer => &self.m.saucer,
                Kind::Heavy => &self.m.heavy,
            };
            let bank = (-e.vel.x * 0.04).clamp(-0.7, 0.7);
            let spin =
                if e.kind == Kind::Saucer { Quat::from_rotation_y(e.age * 1.5) } else { Quat::from_rotation_z(bank) };
            // Enemies face the player (+Z).
            let m = Mat4::from_rotation_translation(Quat::from_rotation_y(vmath::PI) * spin, e.pos);
            let mat = if e.flash > 0.0 { ship_light.with_emissive(Vec3::splat(e.flash * 0.9)) } else { ship_light };
            f.draw(mesh, &m, &mat);
            let (color, size, off) = match e.kind {
                Kind::Dart => (0xFFFF8030, 2.2, 1.7),
                Kind::Kamikaze => (0xFFFF4020, 2.6, 1.7),
                Kind::Saucer => (0xC060E0FF, 2.6, 0.0),
                Kind::Heavy => (0xFFFFA040, 3.6, 3.5),
            };
            engine_glows.push(Billboard::new(e.pos - Vec3::new(0.0, 0.0, off), size, color));
        }
        // Player.
        if self.ship.alive {
            let m = self.ship.matrix();
            f.draw(&self.m.player, &m, &ship_light);
            let flicker = 0.85 + 0.15 * (self.time * 40.0).sin();
            for e in models::PLAYER_ENGINES {
                let p = m.transform_point3(e);
                engine_glows.push(Billboard::new(p, 1.35 * flicker, 0xC060C8FF));
                engine_glows.push(Billboard::new(p + Vec3::new(0.0, 0.0, 0.5), 0.6, 0xE0FFFFFF));
            }
            if self.ship.shield_flash > 0.0 {
                let a = (self.ship.shield_flash * 120.0) as u32;
                let bubble =
                    Mat4::from_scale_rotation_translation(Vec3::new(4.4, 2.6, 4.8), Quat::IDENTITY, self.ship.pos);
                f.draw(&self.m.bubble, &bubble, &Material::glow((a << 24) | 0x3AB0FF).with_cull(v3d::Cull::Back));
            }
        }
        // Pickups.
        let mut pickup_glows = Vec::new();
        for p in &self.pickups {
            let color = match p.kind {
                PickupKind::Shield => 0xFF40A8FF,
                PickupKind::Repair => 0xFF50F080,
                PickupKind::Weapon => 0xFFFFD040,
            };
            let m = Mat4::from_scale_rotation_translation(Vec3::splat(1.2), Quat::from_rotation_y(p.age * 2.5), p.pos);
            f.draw(&self.m.crystal, &m, &Material::lambert(color).with_emissive(v3d::rgb_of(color) * 0.5));
            pickup_glows.push(Billboard::new(p.pos, 5.0 + (p.age * 5.0).sin(), (color & 0xFFFFFF) | 0xA0000000));
        }
        f.billboards(&glow, &engine_glows);
        f.billboards(&glow, &pickup_glows);
        // Shots.
        let mut player_beams = Vec::new();
        let mut enemy_beams = Vec::new();
        for s in &self.shots {
            let tail = s.pos - s.vel.normalize_or(Vec3::Z) * if s.enemy { 3.0 } else { 6.5 };
            if s.enemy {
                enemy_beams.push(Beam { start: tail, end: s.pos, width: 1.0, color: 0xFFFF5030 });
            } else {
                player_beams.push(Beam { start: tail, end: s.pos, width: 0.7, color: 0xFF50E8FF });
            }
        }
        f.beams(&bolt, &player_beams);
        f.beams(&bolt, &enemy_beams);
        // Explosions and particles.
        let mut b = Vec::new();
        self.smoke.billboards(&mut b);
        f.billboards(&smoke, &b);
        b.clear();
        self.fire.billboards(&mut b);
        self.sparks.billboards(&mut b);
        f.billboards(&glow, &b);
        let rings: Vec<Billboard> = self
            .rings
            .iter()
            .map(|r| {
                let k = r.age / r.life;
                let a = ((1.0 - k) * 255.0) as u32;
                Billboard::new(r.pos, r.size * (0.3 + k), (a << 24) | (r.color & 0xFFFFFF))
            })
            .collect();
        f.billboards(&ring, &rings);
        // Space dust streaks.
        let dust: Vec<Beam> = self
            .dust
            .iter()
            .map(|d| Beam { start: *d, end: *d + Vec3::new(0.0, 0.0, 9.0), width: 0.12, color: 0x60B0C8FF })
            .collect();
        f.beams(&bolt, &dust);
        f.finish();
        // Menus sit on a darkened scene (darkening the internal image is much
        // cheaper than dimming the window).
        let shade = if self.paused {
            0x90
        } else {
            match self.phase {
                Phase::Title => 0x40,
                Phase::Playing => 0,
                Phase::GameOver => ((self.phase_time / 0.8).clamp(0.0, 1.0) * 150.0) as u8,
            }
        };
        r.darken(shade);
    }

    fn hud(&mut self, h: &mut Hud) {
        let camera = self.camera();
        let (w, hh) = (h.width as f32, h.height as f32);
        let aspect = w / hh;
        let project = |p: Vec3| camera.project(p, aspect).map(|n| ((n.x + 1.0) * 0.5 * w, (1.0 - n.y) * 0.5 * hh));
        match self.phase {
            Phase::Title => {
                hud::title(h, self.high, self.time as f64);
            }
            // The pause menu replaces the in-game HUD, and so does the game
            // over panel once it is shown.
            Phase::Playing | Phase::GameOver if !self.paused => {
                // Score popups.
                for p in &self.popups {
                    if let Some((x, y)) = project(p.pos) {
                        let a = ((1.0 - p.age) * 255.0) as u32;
                        h.text_shadow(x, y, 16.0, v3d::app::HudFont::Bold, (a << 24) | 0xFFE070, 1, &p.text);
                    }
                }
                if self.phase == Phase::Playing && self.ship.alive {
                    let aim = self.ship.pos + Vec3::new(0.0, 0.0, -70.0);
                    if let Some((x, y)) = project(aim) {
                        hud::reticle(h, x, y);
                    }
                }
                let info = hud::Info {
                    score: self.score,
                    high: self.high,
                    multiplier: 1 + (self.combo / 5).min(3),
                    level: self.level,
                    wave: self.wave.max(1),
                    shield: self.ship.shield / 100.0,
                    hull: self.ship.hull / 100.0,
                    weapon: self.ship.weapon,
                    hit: self.ship.hit_flash,
                    message: self.message.as_ref().map(|(m, t)| (m.as_str(), t.min(1.0))),
                };
                if self.phase == Phase::Playing || self.phase_time < hud::GAME_OVER_PANEL_DELAY {
                    hud::playing(h, &info);
                }
                if self.phase == Phase::GameOver {
                    hud::game_over(h, self.score, self.high, self.level, self.phase_time);
                }
            }
            Phase::Playing | Phase::GameOver => {}
        }
        if self.paused {
            let items: Vec<String> = PAUSE_ITEMS.iter().map(|s| String::from(*s)).collect();
            hud::pause(h, &items, self.menu);
        }
    }
}

fn main() -> i32 {
    let mut s = Settings::new("Starfall", "starfall");
    s.log_name = "starfall";
    v3d::app::run(s, Starfall::new)
}
