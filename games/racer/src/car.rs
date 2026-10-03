//! Cars: the arcade driving physics.
//!
//! Car space: +Z forward, +Y up, +X to the left; the origin is on the ground
//! between the axles.

use vmath::{FloatExt, Mat4, Quat, Vec2, Vec3};

pub use crate::model::{WHEEL_RADIUS, WHEELS};
use crate::track::{BARRIER, CURB, HALF_WIDTH, Track, VERGE};

/// Collision radius.
pub const RADIUS: f32 = 1.15;
/// Top speed on tarmac (m/s, ~225 km/h).
pub const TOP_SPEED: f32 = 62.0;

/// Driver input for one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Controls {
    pub throttle: f32,
    pub brake: f32,
    /// -1 (right) .. 1 (left).
    pub steer: f32,
    pub handbrake: bool,
}

/// Physical state of a car.
#[derive(Clone, Debug)]
pub struct Car {
    pub pos: Vec3,
    /// Heading: 0 = +Z, increasing to the left (towards +X).
    pub yaw: f32,
    /// Horizontal velocity (x, z).
    pub vel: Vec2,
    pub yaw_rate: f32,
    pub steer: f32,
    /// Smoothed surface normal for the visual orientation.
    pub up: Vec3,
    /// Visual body roll / pitch (radians).
    pub roll: f32,
    pub pitch: f32,
    pub wheel_spin: f32,
    pub braking: bool,
    pub on_grass: bool,
    /// Track position.
    pub index: usize,
    pub s: f32,
    pub lateral: f32,
    /// Sliding sideways (drift amount, m/s).
    pub slide: f32,
    pub last_accel: f32,
    /// Recent wall contact (0..1, decays), for sparks and camera shake.
    pub scrape: f32,
}

impl Car {
    /// A car standing on the track at distance `s` and lateral offset.
    pub fn new(track: &Track, s: f32, lateral: f32) -> Car {
        let (p, dir, left, up) = track.frame(s);
        let pos = p + left * lateral;
        let yaw = dir.x.atan2(dir.z);
        let loc = track.locate(pos, None);
        Car {
            pos: Vec3::new(pos.x, loc.height, pos.z),
            yaw,
            vel: Vec2::ZERO,
            yaw_rate: 0.0,
            steer: 0.0,
            up,
            roll: 0.0,
            pitch: 0.0,
            wheel_spin: 0.0,
            braking: false,
            on_grass: false,
            index: loc.index,
            s: loc.s,
            lateral: loc.lateral,
            slide: 0.0,
            last_accel: 0.0,
            scrape: 0.0,
        }
    }

    pub fn forward2(&self) -> Vec2 {
        Vec2::new(self.yaw.sin(), self.yaw.cos())
    }

    pub fn left2(&self) -> Vec2 {
        Vec2::new(self.yaw.cos(), -self.yaw.sin())
    }

    /// Signed speed along the heading (m/s).
    pub fn speed(&self) -> f32 {
        self.vel.dot(self.forward2())
    }

    /// Advances the physics by `dt` seconds.
    pub fn step(&mut self, c: &Controls, dt: f32, track: &Track, ground: &dyn Fn(f32, f32) -> f32) {
        let f = self.forward2();
        let l = self.left2();
        let mut vf = self.vel.dot(f);
        let mut vl = self.vel.dot(l);
        let grass = self.on_grass;

        // Engine, brakes, drag.
        let top = if grass { 30.0 } else { TOP_SPEED };
        let mut accel = 0.0;
        if c.throttle > 0.0 {
            let k = (vf.max(0.0) / top).min(1.0);
            accel += c.throttle * 15.0 * (1.0 - k * k);
        }
        self.braking = c.brake > 0.0 && vf > 0.5;
        if c.brake > 0.0 {
            if vf > 0.5 {
                accel -= c.brake * 26.0;
            } else if vf > -12.0 {
                accel -= c.brake * 7.0; // reverse
            }
        }
        if c.handbrake {
            accel -= vf.signum() * 6.0;
        }
        let drag = 0.0009 * vf * vf.abs() + 0.25 * vf.signum() + if grass { 0.55 * vf } else { 0.0 };
        let new_vf = vf + (accel - drag) * dt;
        // Don't let drag or brakes flip the direction.
        vf = if vf > 0.0 && new_vf < 0.0 && c.brake <= 0.0 { 0.0 } else { new_vf };
        if vf.abs() < 0.05 && c.throttle <= 0.0 && c.brake <= 0.0 {
            vf = 0.0;
        }
        self.last_accel = accel - drag;

        // Steering: less lock at speed, smoothed.
        let speed = vf.abs();
        let max_steer = 0.55 / (1.0 + speed * 0.045);
        let target = c.steer * max_steer;
        self.steer += (target - self.steer) * (1.0 - (-dt * 10.0).exp());
        // Kinematic yaw rate of a bicycle model, with extra rotation when the
        // rear is loose (handbrake) for drifting.
        let wheelbase = 2.66;
        let mut target_rate = vf * self.steer.tan() / wheelbase;
        if c.handbrake && speed > 8.0 {
            target_rate *= 1.6;
        }
        let rate_k = if c.handbrake { 5.0 } else { 9.0 };
        self.yaw_rate += (target_rate - self.yaw_rate) * (1.0 - (-dt * rate_k).exp());
        self.yaw += self.yaw_rate * dt;

        // Lateral grip: tyres kill sideways speed quickly unless sliding.
        let grip = if c.handbrake {
            1.3
        } else if grass {
            3.0
        } else {
            // Grip drops a little while sliding fast, which keeps drifts alive.
            7.5 / (1.0 + (vl.abs() - 3.0).max(0.0) * 0.12)
        };
        let scrub = vl.abs() * 0.35 * dt;
        vl *= (-grip * dt).exp();
        if vf > 0.0 {
            vf = (vf - scrub).max(0.0);
        }
        self.slide = vl.abs();

        // New heading basis (the car rotated this frame).
        let f2 = self.forward2();
        let l2 = self.left2();
        self.vel = f2 * vf + l2 * vl;
        self.pos.x += self.vel.x * dt;
        self.pos.z += self.vel.y * dt;
        self.wheel_spin += vf * dt / WHEEL_RADIUS;
        self.scrape = (self.scrape - dt * 3.0).max(0.0);

        self.follow_ground(track, ground, dt);

        // Body motion for the looks: lean out of corners, squat and dive.
        let target_roll = (-self.yaw_rate * vf * 0.012 - vl * 0.01).clamp(-0.09, 0.09);
        let target_pitch = (-self.last_accel * 0.004).clamp(-0.05, 0.05);
        let k = 1.0 - (-dt * 6.0).exp();
        self.roll += (target_roll - self.roll) * k;
        self.pitch += (target_pitch - self.pitch) * k;
    }

    /// Puts the car on the road or terrain and keeps it inside the world.
    pub fn follow_ground(&mut self, track: &Track, ground: &dyn Fn(f32, f32) -> f32, dt: f32) {
        let loc = track.locate(self.pos, Some(self.index));
        self.index = loc.index;
        self.s = loc.s;
        self.lateral = loc.lateral;
        let edge = HALF_WIDTH + CURB;
        let (h, n) = if loc.lateral.abs() <= edge + 0.5 {
            let smp = track.at(loc.index as isize);
            (loc.height, smp.up)
        } else {
            let h = ground(self.pos.x, self.pos.z);
            let e = 1.5;
            let n = Vec3::new(
                ground(self.pos.x - e, self.pos.z) - ground(self.pos.x + e, self.pos.z),
                2.0 * e,
                ground(self.pos.x, self.pos.z - e) - ground(self.pos.x, self.pos.z + e),
            )
            .normalize_or(Vec3::Y);
            // Blend into the verge so there is no step at the road edge.
            let t = ((loc.lateral.abs() - edge - 0.5) / 3.0).clamp(0.0, 1.0);
            (loc.height + (h - loc.height) * t, n)
        };
        self.on_grass = loc.lateral.abs() > HALF_WIDTH + CURB * 0.8;
        // Corner barriers: slide along them, losing speed.
        let smp = track.at(loc.index as isize);
        if smp.barrier != 0 {
            let side = smp.barrier as f32;
            let into_wall = loc.lateral * side - (BARRIER - 1.05);
            if into_wall > 0.0 && into_wall < 2.5 {
                let n = Vec2::new(smp.left.x, smp.left.z).normalize_or(Vec2::X) * side;
                self.pos.x -= n.x * into_wall;
                self.pos.z -= n.y * into_wall;
                let v_in = self.vel.dot(n);
                if v_in > 0.0 {
                    self.vel -= n * (v_in * 1.3);
                    self.vel *= 0.985;
                    self.scrape = (self.scrape + v_in * 0.1).min(1.0);
                }
            }
        }
        self.pos.y = h;
        self.up = (self.up + (n - self.up) * (1.0 - (-dt * 8.0).exp())).normalize_or(Vec3::Y);
        // Soft walls far from the road.
        let limit = HALF_WIDTH + CURB + VERGE + 22.0;
        if loc.lateral.abs() > limit {
            let smp = track.at(loc.index as isize);
            let back = Vec2::new(smp.left.x, smp.left.z).normalize_or(Vec2::X) * -loc.lateral.signum();
            let over = loc.lateral.abs() - limit;
            self.pos.x += back.x * over * 0.5;
            self.pos.z += back.y * over * 0.5;
            let into = self.vel.dot(-back);
            if into > 0.0 {
                self.vel += back * (into * 1.5);
            }
        }
    }

    /// The model matrix (with body roll and pitch).
    pub fn matrix(&self) -> Mat4 {
        let fwd = Vec3::new(self.yaw.sin(), 0.0, self.yaw.cos());
        let up = self.up;
        let f = (fwd - up * fwd.dot(up)).normalize_or(fwd);
        let left = up.cross(f).normalize_or(Vec3::X);
        let basis = Mat4::from_cols(left.extend(0.0), up.extend(0.0), f.extend(0.0), self.pos.extend(1.0));
        basis * Mat4::from_quat(Quat::from_rotation_z(self.roll) * Quat::from_rotation_x(self.pitch))
    }

    /// The model matrix of wheel `i` (0..4: front left, front right, rear
    /// left, rear right) given the car matrix.
    pub fn wheel_matrix(&self, car: &Mat4, i: usize) -> Mat4 {
        let steer = if i < 2 { self.steer } else { 0.0 };
        *car * Mat4::from_translation(WHEELS[i]) * Mat4::from_rotation_y(steer) * Mat4::from_rotation_x(self.wheel_spin)
    }
}

/// Pushes overlapping cars apart and exchanges momentum (bumps).
pub fn collide(cars: &mut [Car]) {
    let n = cars.len();
    for i in 0..n {
        for j in i + 1..n {
            let d = Vec2::new(cars[j].pos.x - cars[i].pos.x, cars[j].pos.z - cars[i].pos.z);
            let dist = d.length();
            let min = RADIUS * 2.0;
            if dist >= min || dist < 1e-4 {
                continue;
            }
            let nrm = d / dist;
            let push = (min - dist) * 0.5;
            cars[i].pos.x -= nrm.x * push;
            cars[i].pos.z -= nrm.y * push;
            cars[j].pos.x += nrm.x * push;
            cars[j].pos.z += nrm.y * push;
            let rel = (cars[j].vel - cars[i].vel).dot(nrm);
            if rel < 0.0 {
                let impulse = nrm * (-rel * 0.65);
                cars[i].vel -= impulse;
                cars[j].vel += impulse;
            }
        }
    }
}
