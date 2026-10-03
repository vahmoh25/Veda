//! A simple particle system (sparks, smoke, explosions, exhaust) rendered
//! as camera-facing billboards.

use alloc::vec::Vec;

use vmath::{Rng, Vec2, Vec3};

use crate::renderer::Billboard;
use crate::texture::lerp_color;

/// One particle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Particle {
    /// World-space position.
    pub position: Vec3,
    /// Velocity in units per second.
    pub velocity: Vec3,
    /// Seconds lived so far.
    pub age: f32,
    /// Total lifetime in seconds.
    pub life: f32,
    /// Size at birth (world units).
    pub size_start: f32,
    /// Size at death (world units).
    pub size_end: f32,
    /// Straight-alpha colour at birth.
    pub color_start: u32,
    /// Straight-alpha colour at death.
    pub color_end: u32,
    /// Rotation around the view axis in radians.
    pub rotation: f32,
    /// Rotation speed in radians per second.
    pub spin: f32,
}

impl Particle {
    /// A particle of constant size that fades out over `life` seconds.
    pub fn new(position: Vec3, velocity: Vec3, life: f32, size: f32, color: u32) -> Particle {
        Particle {
            position,
            velocity,
            age: 0.0,
            life: life.max(1e-3),
            size_start: size,
            size_end: size,
            color_start: color,
            color_end: color & 0x00FF_FFFF,
            rotation: 0.0,
            spin: 0.0,
        }
    }
}

/// Parameters for [`ParticleSystem::burst`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Burst {
    /// Number of particles.
    pub count: u32,
    /// Speed range of the random directions.
    pub speed: (f32, f32),
    /// Lifetime range in seconds.
    pub life: (f32, f32),
    /// Size at birth (world units).
    pub size_start: f32,
    /// Size at death (world units).
    pub size_end: f32,
    /// Straight-alpha colour at birth.
    pub color_start: u32,
    /// Straight-alpha colour at death.
    pub color_end: u32,
    /// Random positional spread around the centre.
    pub spread: f32,
}

/// A pool of particles with gravity and drag.
#[derive(Clone, Debug)]
pub struct ParticleSystem {
    /// The living particles.
    pub particles: Vec<Particle>,
    /// Acceleration applied to every particle.
    pub gravity: Vec3,
    /// Fraction of velocity lost per second (0 = none).
    pub drag: f32,
    /// Capacity; when full, new particles replace the oldest.
    pub max_particles: usize,
}

impl ParticleSystem {
    /// An empty system holding at most `max_particles`.
    pub fn new(max_particles: usize) -> ParticleSystem {
        ParticleSystem {
            particles: Vec::with_capacity(max_particles.min(4096)),
            gravity: Vec3::ZERO,
            drag: 0.0,
            max_particles,
        }
    }

    /// Adds a particle (replacing the oldest one when full).
    pub fn emit(&mut self, p: Particle) {
        if self.particles.len() < self.max_particles {
            self.particles.push(p);
        } else if let Some((i, _)) = self.particles.iter().enumerate().max_by(|a, b| {
            (a.1.age / a.1.life).partial_cmp(&(b.1.age / b.1.life)).unwrap_or(core::cmp::Ordering::Equal)
        }) {
            self.particles[i] = p;
        }
    }

    /// Emits `b.count` particles from `center` in random directions, plus
    /// `inherit` velocity.
    pub fn burst(&mut self, rng: &mut Rng, center: Vec3, inherit: Vec3, b: &Burst) {
        for _ in 0..b.count {
            let dir = rng.unit_vec3();
            let speed = rng.range_f32(b.speed.0, b.speed.1);
            let mut p = Particle::new(
                center + rng.in_unit_sphere() * b.spread,
                inherit + dir * speed,
                rng.range_f32(b.life.0, b.life.1),
                b.size_start,
                b.color_start,
            );
            p.size_end = b.size_end;
            p.color_end = b.color_end;
            p.rotation = rng.range_f32(0.0, vmath::TAU);
            p.spin = rng.range_f32(-2.0, 2.0);
            self.emit(p);
        }
    }

    /// Advances the simulation by `dt` seconds and removes dead particles.
    pub fn update(&mut self, dt: f32) {
        let damp = (1.0 - self.drag * dt).clamp(0.0, 1.0);
        let g = self.gravity * dt;
        self.particles.retain_mut(|p| {
            p.age += dt;
            if p.age >= p.life {
                return false;
            }
            p.velocity = p.velocity * damp + g;
            p.position += p.velocity * dt;
            p.rotation += p.spin * dt;
            true
        });
    }

    /// Number of living particles.
    pub fn len(&self) -> usize {
        self.particles.len()
    }

    /// True when no particle is alive.
    pub fn is_empty(&self) -> bool {
        self.particles.is_empty()
    }

    /// Removes every particle.
    pub fn clear(&mut self) {
        self.particles.clear();
    }

    /// Appends one billboard per particle (size and colour interpolated over
    /// its life) to `out`.
    pub fn billboards(&self, out: &mut Vec<Billboard>) {
        for p in &self.particles {
            let t = (p.age / p.life).clamp(0.0, 1.0);
            let size = p.size_start + (p.size_end - p.size_start) * t;
            let color = lerp_color(p.color_start, p.color_end, (t * 256.0) as u32);
            out.push(Billboard {
                position: p.position,
                size: Vec2::splat(size),
                rotation: p.rotation,
                color,
                uv: [0.0, 0.0, 1.0, 1.0],
            });
        }
    }
}
