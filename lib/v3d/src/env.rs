//! The lighting environment of a frame: sun, hemisphere ambient, point
//! lights, distance fog and the background.

use alloc::vec::Vec;

use vmath::Vec3;

/// A local light with a smooth falloff to zero at `radius`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointLight {
    /// World-space position.
    pub position: Vec3,
    /// Colour and intensity (may exceed 1).
    pub color: Vec3,
    /// Distance at which the light has faded to nothing.
    pub radius: f32,
}

/// Linear distance fog between `start` and `end` (view depth), reaching
/// `max` (0..1) opacity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fog {
    /// Fog colour, 0..1 per channel.
    pub color: Vec3,
    /// View depth where the fog begins.
    pub start: f32,
    /// View depth where the fog reaches `max`.
    pub end: f32,
    /// Largest fog opacity, 0..1.
    pub max: f32,
}

/// What fills pixels that no geometry covers.
#[derive(Clone, Debug, PartialEq)]
pub enum Background {
    /// One opaque `0xFFRRGGBB` colour.
    Solid(u32),
    /// A sky gradient by elevation of the view ray: `(sin(elevation),
    /// colour)` stops in increasing order (-1 = straight down, 1 = up).
    Sky(Vec<(f32, u32)>),
}

/// Lights, fog and background.
#[derive(Clone, Debug, PartialEq)]
pub struct Environment {
    /// Direction towards the sun (normalised when used).
    pub sun_direction: Vec3,
    /// Sun colour and intensity (may exceed 1).
    pub sun_color: Vec3,
    /// Ambient light from above (hemisphere lighting).
    pub sky_ambient: Vec3,
    /// Ambient light from below (hemisphere lighting).
    pub ground_ambient: Vec3,
    /// Local lights; each object uses the nearest few that reach it.
    pub point_lights: Vec<PointLight>,
    /// Distance fog for materials with `fog` set.
    pub fog: Option<Fog>,
    /// What fills uncovered pixels.
    pub background: Background,
}

impl Default for Environment {
    fn default() -> Environment {
        Environment {
            sun_direction: Vec3::new(0.4, 0.8, 0.3),
            sun_color: Vec3::splat(0.85),
            sky_ambient: Vec3::splat(0.35),
            ground_ambient: Vec3::splat(0.15),
            point_lights: Vec::new(),
            fog: None,
            background: Background::Solid(0xFF20_2830),
        }
    }
}

impl Environment {
    /// Background colour for a view ray with the given sine of elevation.
    pub fn background_at(&self, sin_elevation: f32) -> u32 {
        match &self.background {
            Background::Solid(c) => *c,
            Background::Sky(stops) => {
                let Some(first) = stops.first() else { return 0xFF00_0000 };
                if sin_elevation <= first.0 {
                    return first.1;
                }
                for w in stops.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    if sin_elevation <= b.0 {
                        let t = ((sin_elevation - a.0) / (b.0 - a.0).max(1e-6)).clamp(0.0, 1.0);
                        return crate::texture::lerp_color(a.1, b.1, (t * 256.0) as u32) | 0xFF00_0000;
                    }
                }
                stops.last().unwrap().1
            }
        }
    }
}
