//! The world around the circuit: terrain (tiles with two levels of detail),
//! trees and rocks, distant mountains, the start gantry, a grandstand,
//! advertising boards, clouds and the sky.

use alloc::vec;
use alloc::vec::Vec;

use v3d::{Background, Billboard, Environment, Fog, Mesh, MeshBuilder, Texture, Vertex, shapes};
use vmath::{FloatExt, Mat4, Noise, Quat, Rng, Vec2, Vec3};

use crate::track::{CURB, HALF_WIDTH, Track, VERGE};

/// Terrain cell size and extent.
const CELL: f32 = 20.0;
const HALF_EXTENT: f32 = 1100.0;
const CELLS: usize = (2.0 * HALF_EXTENT / CELL) as usize;
/// Cells per tile side.
const TILE_CELLS: usize = 10;
/// Width of the flattened corridor around the road.
const CORRIDOR: f32 = HALF_WIDTH + CURB + VERGE;

/// Quick nearest-track-point queries (bucketed samples).
struct TrackIndex {
    cell: f32,
    dim: usize,
    origin: f32,
    buckets: Vec<Vec<u32>>,
}

impl TrackIndex {
    fn new(track: &Track) -> TrackIndex {
        let cell = 64.0;
        let origin = -HALF_EXTENT - 200.0;
        let dim = ((2.0 * -origin) / cell) as usize + 1;
        let mut buckets = vec![Vec::new(); dim * dim];
        for (i, s) in track.samples.iter().enumerate() {
            let cx = (((s.pos.x - origin) / cell) as usize).min(dim - 1);
            let cz = (((s.pos.z - origin) / cell) as usize).min(dim - 1);
            buckets[cz * dim + cx].push(i as u32);
        }
        TrackIndex { cell, dim, origin, buckets }
    }

    /// Horizontal distance to the centre line (capped at ~100 m) and the
    /// road height there.
    fn nearest(&self, track: &Track, x: f32, z: f32) -> (f32, f32) {
        let cx = ((x - self.origin) / self.cell) as isize;
        let cz = ((z - self.origin) / self.cell) as isize;
        let mut best = (f32::MAX, 0.0f32);
        for dz in -2..=2isize {
            for dx in -2..=2isize {
                let (bx, bz) = (cx + dx, cz + dz);
                if bx < 0 || bz < 0 || bx >= self.dim as isize || bz >= self.dim as isize {
                    continue;
                }
                for &i in &self.buckets[bz as usize * self.dim + bx as usize] {
                    let a = &track.samples[i as usize];
                    let b = track.at(i as isize + 1);
                    let (a2, b2) = (Vec2::new(a.pos.x, a.pos.z), Vec2::new(b.pos.x, b.pos.z));
                    let p = Vec2::new(x, z);
                    let ab = b2 - a2;
                    let t = ((p - a2).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
                    let d = (a2 + ab * t).distance(p);
                    if d < best.0 {
                        best = (d, a.pos.y + (b.pos.y - a.pos.y) * t);
                    }
                }
            }
        }
        best
    }
}

/// Terrain height without the track's influence: rolling hills rising
/// into a ring of mountains.
fn natural_height(noise: &Noise, x: f32, z: f32) -> f32 {
    let hills =
        noise.fbm2(x / 380.0, z / 380.0, 4, 2.0, 0.5) * 34.0 + noise.fbm2(x / 90.0 + 7.0, z / 90.0, 2, 2.0, 0.5) * 4.0;
    let r = (x * x + z * z).sqrt();
    let ring = vmath::smoothstep(620.0, 1050.0, r);
    let peaks = (noise.ridged2(x / 260.0 + 3.0, z / 260.0, 4, 2.0, 0.5) * 0.7 + 0.3) * 260.0;
    hills + ring * (60.0 + peaks)
}

/// A terrain tile with two levels of detail and its scenery.
pub struct Tile {
    pub center: Vec3,
    pub near: Mesh,
    pub far: Mesh,
    /// Trees and rocks, detailed and simplified.
    pub scenery: Option<Mesh>,
    pub scenery_far: Option<Mesh>,
}

/// Everything static around the track.
pub struct World {
    pub tiles: Vec<Tile>,
    pub mountains: Mesh,
    pub gantry: Mesh,
    pub banner: Mesh,
    pub stand: Mesh,
    pub crowd: Mesh,
    pub boards: Vec<Mesh>,
    pub clouds: Vec<Billboard>,
    pub env: Environment,
    heights: Vec<f32>,
    /// Grid vertices under the road and its verges (cells entirely made of
    /// them are left out of the terrain mesh).
    covered: Vec<bool>,
}

impl World {
    pub fn new(track: &Track, seed: u64) -> World {
        let noise = Noise::new(seed);
        let index = TrackIndex::new(track);
        // Height grid.
        let n = CELLS + 1;
        let mut heights = vec![0.0f32; n * n];
        let mut covered = vec![false; n * n];
        for j in 0..n {
            for i in 0..n {
                let (x, z) = (-HALF_EXTENT + i as f32 * CELL, -HALF_EXTENT + j as f32 * CELL);
                let (h, d) = ground_at(&noise, &index, track, x, z);
                heights[j * n + i] = h;
                covered[j * n + i] = d < CORRIDOR - 3.0;
            }
        }
        let mut world = World {
            tiles: Vec::new(),
            mountains: mountains(seed),
            gantry: Mesh::new(Vec::new(), Vec::new()),
            banner: Mesh::new(Vec::new(), Vec::new()),
            stand: Mesh::new(Vec::new(), Vec::new()),
            crowd: Mesh::new(Vec::new(), Vec::new()),
            boards: Vec::new(),
            clouds: Vec::new(),
            env: environment(),
            heights,
            covered,
        };
        world.build_tiles(&noise, &index, track, seed);
        world.build_props(track);
        world.build_clouds(seed);
        world
    }

    /// Terrain height (bilinear on the grid; matches the mesh closely).
    pub fn height(&self, x: f32, z: f32) -> f32 {
        let n = CELLS + 1;
        let fx = ((x + HALF_EXTENT) / CELL).clamp(0.0, CELLS as f32 - 0.001);
        let fz = ((z + HALF_EXTENT) / CELL).clamp(0.0, CELLS as f32 - 0.001);
        let (i, j) = (fx as usize, fz as usize);
        let (tx, tz) = (fx - i as f32, fz - j as f32);
        let h = |a: usize, b: usize| self.heights[b * n + a];
        let top = h(i, j) + (h(i + 1, j) - h(i, j)) * tx;
        let bottom = h(i, j + 1) + (h(i + 1, j + 1) - h(i, j + 1)) * tx;
        top + (bottom - top) * tz
    }

    fn grid(&self, i: usize, j: usize) -> f32 {
        let n = CELLS + 1;
        self.heights[j.min(CELLS) * n + i.min(CELLS)]
    }

    fn build_tiles(&mut self, noise: &Noise, index: &TrackIndex, track: &Track, seed: u64) {
        let tiles = CELLS / TILE_CELLS;
        let tree_models = tree_models();
        let rocks: Vec<MeshBuilder> = (0..3).map(|k| rock_model(seed + k)).collect();
        let mut rng = Rng::new(seed ^ 0x5EED);
        for tj in 0..tiles {
            for ti in 0..tiles {
                let (i0, j0) = (ti * TILE_CELLS, tj * TILE_CELLS);
                let near = self.tile_mesh(noise, i0, j0, 1);
                let far = self.tile_mesh(noise, i0, j0, 2);
                let cx = -HALF_EXTENT + (i0 as f32 + TILE_CELLS as f32 / 2.0) * CELL;
                let cz = -HALF_EXTENT + (j0 as f32 + TILE_CELLS as f32 / 2.0) * CELL;
                let center = Vec3::new(cx, self.height(cx, cz), cz);
                // Scenery, with a simplified copy for distant tiles.
                let mut sc = MeshBuilder::new();
                let mut sc_far = MeshBuilder::new();
                let size = TILE_CELLS as f32 * CELL;
                let forest = (noise.fbm2(cx / 500.0 + 11.0, cz / 500.0, 3, 2.0, 0.5) + 0.3).clamp(0.0, 1.0);
                let r = (cx * cx + cz * cz).sqrt();
                let count = if r > 1000.0 { 0 } else { (forest * 34.0) as usize + 3 };
                for _ in 0..count {
                    let x = cx + rng.range_f32(-0.5, 0.5) * size;
                    let z = cz + rng.range_f32(-0.5, 0.5) * size;
                    let (d, _) = index.nearest(track, x, z);
                    if d < CORRIDOR + 3.0 {
                        continue;
                    }
                    let y = self.height(x, z);
                    let slope = (self.height(x + 4.0, z) - self.height(x - 4.0, z)).abs()
                        + (self.height(x, z + 4.0) - self.height(x, z - 4.0)).abs();
                    if slope > 6.0 || y > 150.0 {
                        continue;
                    }
                    let (m, m_far) = &tree_models[rng.below(tree_models.len() as u32) as usize];
                    let s = rng.range_f32(0.75, 1.35);
                    let t = Mat4::from_scale_rotation_translation(
                        Vec3::splat(s),
                        Quat::from_rotation_y(rng.range_f32(0.0, vmath::TAU)),
                        Vec3::new(x, y - 0.3, z),
                    );
                    sc.append_transformed(m, &t);
                    sc_far.append_transformed(m_far, &t);
                }
                for _ in 0..rng.below(3) {
                    let x = cx + rng.range_f32(-0.5, 0.5) * size;
                    let z = cz + rng.range_f32(-0.5, 0.5) * size;
                    let (d, _) = index.nearest(track, x, z);
                    if d < CORRIDOR + 3.0 || r > 1000.0 {
                        continue;
                    }
                    let y = self.height(x, z);
                    let s = rng.range_f32(1.2, 3.5);
                    let m = &rocks[rng.below(3) as usize];
                    let t = Mat4::from_scale_rotation_translation(
                        Vec3::new(s, s * 0.7, s),
                        Quat::from_rotation_y(rng.range_f32(0.0, vmath::TAU)),
                        Vec3::new(x, y - 0.4, z),
                    );
                    sc.append_transformed(m, &t);
                }
                let scenery = if sc.indices.is_empty() { None } else { Some(sc.build()) };
                let scenery_far = if sc_far.indices.is_empty() { None } else { Some(sc_far.build()) };
                self.tiles.push(Tile { center, near, far, scenery, scenery_far });
            }
        }
    }

    /// Terrain mesh for the tile at grid (i0, j0), every `step` cells.
    /// Coarse tiles get skirts that hide cracks next to finer neighbours;
    /// cells under the road corridor are left out (the road and its verges
    /// cover them).
    fn tile_mesh(&self, noise: &Noise, i0: usize, j0: usize, step: usize) -> Mesh {
        let mut b = MeshBuilder::new();
        let m = TILE_CELLS / step;
        let row = m as u32 + 1;
        let n = CELLS + 1;
        let covered = |gi: usize, gj: usize| self.covered[gj.min(CELLS) * n + gi.min(CELLS)];
        for j in 0..=m {
            for i in 0..=m {
                let (gi, gj) = (i0 + i * step, j0 + j * step);
                let x = -HALF_EXTENT + gi as f32 * CELL;
                let z = -HALF_EXTENT + gj as f32 * CELL;
                let y = self.grid(gi, gj);
                let e = step;
                let dx = self.grid(gi + e, gj) - self.grid(gi.saturating_sub(e), gj);
                let dz = self.grid(gi, gj + e) - self.grid(gi, gj.saturating_sub(e));
                let n = Vec3::new(-dx, 2.0 * CELL * step as f32, -dz).normalize();
                let color = terrain_color(noise, x, y, z, n);
                b.vertex(Vertex::new(Vec3::new(x, y, z), n, Vec2::new(x / 10.0, z / 10.0), color));
            }
        }
        for j in 0..m as u32 {
            for i in 0..m as u32 {
                let (gi, gj) = (i0 + i as usize * step, j0 + j as usize * step);
                let hidden = covered(gi, gj)
                    && covered(gi + step, gj)
                    && covered(gi, gj + step)
                    && covered(gi + step, gj + step);
                if hidden {
                    continue;
                }
                let a = j * row + i;
                b.quad(a, a + row, a + row + 1, a + 1);
            }
        }
        // Skirts along the four edges of coarse tiles.
        let edges: [(u32, u32, i32); 4] = [(0, 1, 0), (m as u32 * row, 1, 1), (0, row, 2), (m as u32, row, 3)];
        for (start, stride, side) in edges {
            if step == 1 {
                break;
            }
            let base = b.vertices.len() as u32;
            for k in 0..=m as u32 {
                let mut v = b.vertices[(start + k * stride) as usize];
                v.position.y -= 12.0;
                b.vertex(v);
            }
            for k in 0..m as u32 {
                let (top0, top1) = (start + k * stride, start + (k + 1) * stride);
                let (bot0, bot1) = (base + k, base + k + 1);
                // Face outwards from the tile.
                if side == 0 || side == 3 {
                    b.quad(top0, top1, bot1, bot0);
                } else {
                    b.quad(top0, bot0, bot1, top1);
                }
            }
        }
        let _ = noise;
        b.build()
    }

    fn build_props(&mut self, track: &Track) {
        let (p, dir, left, up) = track.frame(6.0);
        let flat_left = Vec3::new(left.x, 0.0, left.z).normalize_or(Vec3::X);
        let yaw = dir.x.atan2(dir.z);
        let basis = |offset: Vec3| -> Mat4 {
            Mat4::from_rotation_translation(
                Quat::from_rotation_y(yaw),
                p + flat_left * offset.x + Vec3::Y * offset.y + dir * offset.z,
            )
        };
        // Start gantry: two pillars and a beam; the banner is textured.
        let mut g = MeshBuilder::new();
        let span = HALF_WIDTH + 1.6;
        for side in [-1.0f32, 1.0] {
            let pillar = shapes::cuboid(Vec3::new(0.9, 8.0, 0.9), 0xFF3A3E46);
            g.append_transformed(&pillar, &basis(Vec3::new(side * span, 4.0 - 0.5, 0.0)));
            let foot = shapes::cuboid(Vec3::new(1.4, 0.6, 1.4), 0xFF2A2C30);
            g.append_transformed(&foot, &basis(Vec3::new(side * span, 0.0, 0.0)));
        }
        let beam = shapes::cuboid(Vec3::new(2.0 * span + 0.9, 0.35, 0.9), 0xFF3A3E46);
        g.append_transformed(&beam, &basis(Vec3::new(0.0, 8.7, 0.0)));
        self.gantry = g.flat_shaded().build();
        let mut banner = shapes::cuboid(Vec3::new(2.0 * span - 0.9, 1.6, 0.5), 0xFFFFFFFF);
        banner.transform(&basis(Vec3::new(0.0, 7.4, 0.0)));
        self.banner = banner.build();
        let _ = up;

        // Grandstand on the outside (right) of the start straight.
        let (sp, sdir, sleft, _) = track.frame(track.length - 40.0);
        let syaw = sdir.x.atan2(sdir.z);
        let sflat = Vec3::new(sleft.x, 0.0, sleft.z).normalize_or(Vec3::X);
        let stand_at = |offset: Vec3| -> Mat4 {
            Mat4::from_rotation_translation(
                Quat::from_rotation_y(syaw),
                sp + sflat * offset.x + Vec3::Y * offset.y + sdir * offset.z,
            )
        };
        let mut st = MeshBuilder::new();
        let mut crowd = MeshBuilder::new();
        let base_x = -(HALF_WIDTH + CURB + 9.0);
        for k in 0..6 {
            let x = base_x - k as f32 * 1.6;
            let y = 0.6 + k as f32 * 0.9;
            let step = shapes::cuboid(Vec3::new(1.6, 0.25, 44.0), 0xFF8A8E96);
            st.append_transformed(&step, &stand_at(Vec3::new(x, y - 0.4, 0.0)));
            // Spectators: a strip facing the track.
            let mut c = MeshBuilder::new();
            let (h0, h1, z0, z1) = (y - 0.25, y + 0.55, -21.5, 21.5);
            let xf = x + 0.5;
            c.vertex(Vertex::new(Vec3::new(xf, h0, z1), Vec3::X, Vec2::new(0.0, 1.0), 0xFFFFFFFF));
            c.vertex(Vertex::new(Vec3::new(xf, h0, z0), Vec3::X, Vec2::new(8.0, 1.0), 0xFFFFFFFF));
            c.vertex(Vertex::new(Vec3::new(xf, h1, z0), Vec3::X, Vec2::new(8.0, 0.0), 0xFFFFFFFF));
            c.vertex(Vertex::new(Vec3::new(xf, h1, z1), Vec3::X, Vec2::new(0.0, 0.0), 0xFFFFFFFF));
            c.quad(0, 1, 2, 3);
            crowd.append_transformed(&c, &stand_at(Vec3::ZERO));
        }
        let back = shapes::cuboid(Vec3::new(0.5, 7.5, 44.0), 0xFF5A5E66);
        st.append_transformed(&back, &stand_at(Vec3::new(base_x - 9.8, 3.6, 0.0)));
        let roof = shapes::cuboid(Vec3::new(12.0, 0.4, 46.0), 0xFFE0E4EA);
        st.append_transformed(&roof, &(stand_at(Vec3::new(base_x - 4.0, 8.6, 0.0)) * Mat4::from_rotation_z(-0.12)));
        for z in [-20.0f32, 0.0, 20.0] {
            let post = shapes::cuboid(Vec3::new(0.3, 8.0, 0.3), 0xFF4A4E56);
            st.append_transformed(&post, &stand_at(Vec3::new(base_x + 1.2, 4.0, z)));
        }
        self.stand = st.flat_shaded().build();
        self.crowd = crowd.build();

        // Advertising boards along the main straight (textured later).
        for k in 0..4 {
            let s = track.length - 90.0 + k as f32 * 30.0;
            let (bp, bdir, bleft, _) = track.frame(s);
            let side = if k % 2 == 0 { 1.0 } else { -1.0 };
            let fl = Vec3::new(bleft.x, 0.0, bleft.z).normalize_or(Vec3::X);
            let yaw = bdir.x.atan2(bdir.z);
            let pos = bp + fl * (side * (HALF_WIDTH + CURB + 3.0)) + Vec3::Y * 1.1;
            let mut board = shapes::cuboid(Vec3::new(0.2, 1.6, 9.0), 0xFFFFFFFF);
            board.transform(&Mat4::from_rotation_translation(Quat::from_rotation_y(yaw), pos));
            self.boards.push(board.build());
        }
    }

    fn build_clouds(&mut self, seed: u64) {
        let mut rng = Rng::new(seed ^ 0xC10D);
        for _ in 0..16 {
            let a = rng.range_f32(0.0, vmath::TAU);
            let r = rng.range_f32(300.0, 2200.0);
            let w = rng.range_f32(260.0, 520.0);
            self.clouds.push(Billboard {
                position: Vec3::new(a.cos() * r, rng.range_f32(320.0, 520.0), a.sin() * r),
                size: Vec2::new(w, w * rng.range_f32(0.3, 0.45)),
                rotation: 0.0,
                color: 0xD8FFFFFF,
                uv: [0.0, 0.0, 1.0, 1.0],
            });
        }
    }
}

/// Terrain height and the distance to the track's centre line.
fn ground_at(noise: &Noise, index: &TrackIndex, track: &Track, x: f32, z: f32) -> (f32, f32) {
    let natural = natural_height(noise, x, z);
    let (d, h) = index.nearest(track, x, z);
    let y = if d < CORRIDOR {
        h - 1.4
    } else {
        let t = vmath::smoothstep(CORRIDOR, CORRIDOR + 70.0, d);
        let near = h - 0.35;
        near + (natural.max(near - 6.0) - near) * t
    };
    (y, d)
}

fn terrain_color(noise: &Noise, x: f32, y: f32, z: f32, n: Vec3) -> u32 {
    let k = (noise.fbm2(x / 60.0, z / 60.0, 3, 2.0, 0.5) * 0.5 + 0.5).clamp(0.0, 1.0);
    let grass = v3d::lerp_color(0xFF5E8E3A, 0xFF8EB052, (k * 256.0) as u32);
    let dry = v3d::lerp_color(grass, 0xFFA89A62, (vmath::smoothstep(0.62, 0.85, k) * 200.0) as u32);
    let steep = vmath::smoothstep(0.82, 0.62, n.y);
    let rock = v3d::lerp_color(dry, 0xFF8C8680, (steep * 256.0) as u32);
    let snow = vmath::smoothstep(150.0, 210.0, y + noise.perlin2(x / 40.0, z / 40.0) * 20.0);
    v3d::lerp_color(rock, 0xFFF2F5F8, (snow * 256.0) as u32)
}

/// Low-poly trees: conifers and broadleaf trees in a few shades.
/// Low-poly trees, conifers and broadleaf trees in a few shades, each as a
/// (near, far) pair: the far version is a single cheap shape.
fn tree_models() -> Vec<(MeshBuilder, MeshBuilder)> {
    let mut out = Vec::new();
    let greens = [0xFF2E5A2A, 0xFF3A6A30, 0xFF4A7A34];
    for &g in &greens {
        let mut t = shapes::cylinder(0.35, 0.25, 2.2, 5, false, 0xFF5A4030);
        for (k, (r, h, y)) in [(3.0f32, 4.5f32, 1.6f32), (2.4, 4.0, 3.8), (1.7, 3.4, 5.8)].iter().enumerate() {
            let shade = v3d::lerp_color(g, 0xFF1E3A1A, k as u32 * 30);
            let mut c = shapes::cylinder(*r, 0.0, *h, 7, false, shade);
            c.transform(&Mat4::from_translation(Vec3::new(0.0, *y, 0.0)));
            t.append(&c);
        }
        let mut far = shapes::cylinder(2.8, 0.0, 9.0, 5, false, v3d::lerp_color(g, 0xFF1E3A1A, 30));
        far.transform(&Mat4::from_translation(Vec3::new(0.0, 0.6, 0.0)));
        out.push((t.flat_shaded(), far.flat_shaded()));
    }
    for &g in &[0xFF4E8A36u32, 0xFF6A9A3A] {
        let mut t = shapes::cylinder(0.3, 0.22, 3.0, 5, false, 0xFF6A4A34);
        let mut crown = shapes::icosphere(2.6, 0, g);
        let mut rng = Rng::new(g as u64);
        for v in &mut crown.vertices {
            v.position *= rng.range_f32(0.85, 1.15);
        }
        crown.transform(&Mat4::from_scale_rotation_translation(
            Vec3::new(1.0, 0.85, 1.0),
            Quat::IDENTITY,
            Vec3::new(0.0, 4.4, 0.0),
        ));
        t.append(&crown);
        let mut far = shapes::cylinder(2.6, 0.0, 2.6, 4, false, g);
        let mut bottom = shapes::cylinder(2.6, 0.0, 2.6, 4, false, v3d::lerp_color(g, 0xFF1E3A1A, 60));
        bottom.transform(&Mat4::from_rotation_x(vmath::PI));
        far.append(&bottom);
        far.transform(&Mat4::from_translation(Vec3::new(0.0, 4.4, 0.0)));
        out.push((t.flat_shaded(), far.flat_shaded()));
    }
    out
}

fn rock_model(seed: u64) -> MeshBuilder {
    let mut b = shapes::icosphere(1.0, 1, 0xFF8A847A);
    let n = Noise::new(seed);
    for v in &mut b.vertices {
        let k = n.fbm3(v.position.x * 1.7, v.position.y * 1.7, v.position.z * 1.7, 3, 2.0, 0.5);
        v.position *= 1.0 + 0.3 * k;
    }
    b.flat_shaded()
}

/// A ring of far mountain silhouettes, pre-tinted with the haze.
fn mountains(seed: u64) -> Mesh {
    let mut rng = Rng::new(seed ^ 0x3333);
    let mut b = MeshBuilder::new();
    let segs = 72;
    let haze = 0xFFA8B6C8;
    let mut heights: Vec<f32> = (0..segs).map(|_| rng.range_f32(140.0, 420.0)).collect();
    heights = crate::track::smooth(&heights, 1);
    for (i, &height) in heights.iter().enumerate() {
        let a0 = i as f32 / segs as f32 * vmath::TAU;
        let a1 = (i + 1) as f32 / segs as f32 * vmath::TAU;
        let am = (a0 + a1) * 0.5 + rng.range_f32(-0.02, 0.02);
        let r = 2600.0;
        let p0 = Vec3::new(a0.cos() * r, -40.0, a0.sin() * r);
        let p1 = Vec3::new(a1.cos() * r, -40.0, a1.sin() * r);
        let peak = Vec3::new(am.cos() * (r + 120.0), height, am.sin() * (r + 120.0));
        let side = v3d::lerp_color(0xFF7A8AA0, haze, rng.below(120) + 60);
        let base = b.vertices.len() as u32;
        let n = (peak - (p0 + p1) * 0.5).cross(p1 - p0).normalize_or(Vec3::Y);
        let n = if n.dot(-p0) < 0.0 { -n } else { n };
        let snow = if height > 320.0 { 0xFFE6ECF4 } else { side };
        b.vertex(Vertex::new(p0, n, Vec2::ZERO, haze));
        b.vertex(Vertex::new(p1, n, Vec2::ZERO, haze));
        b.vertex(Vertex::new(peak, n, Vec2::ZERO, snow));
        // Seen from the inside of the ring.
        b.triangle(base, base + 1, base + 2);
    }
    b.build()
}

/// Late-afternoon light, haze and sky.
fn environment() -> Environment {
    Environment {
        sun_direction: Vec3::new(-0.45, 0.45, -0.62).normalize(),
        sun_color: Vec3::new(1.05, 0.94, 0.8),
        sky_ambient: Vec3::new(0.42, 0.5, 0.64),
        ground_ambient: Vec3::new(0.25, 0.23, 0.18),
        point_lights: Vec::new(),
        fog: Some(Fog { color: Vec3::new(0.74, 0.79, 0.86), start: 140.0, end: 1500.0, max: 0.88 }),
        background: Background::Sky(vec![
            (-1.0, 0xFFB4BECA),
            (0.0, 0xFFC6CED8),
            (0.05, 0xFFAEC4DE),
            (0.22, 0xFF7AA2D2),
            (0.6, 0xFF3E6CB0),
            (1.0, 0xFF2A5296),
        ]),
    }
}

/// Textures made with the HUD's font renderer (banner, boards, crowd).
pub fn text_texture(text: &mut vgfx::Text, font: usize, label: &str, bg: u32, fg: u32, accent: u32) -> Texture {
    let (w, h) = (256i32, 32i32);
    let mut bmp = vgfx::Bitmap::new(w, h);
    {
        let mut c = vgfx::Canvas::for_bitmap(&mut bmp);
        c.clear(vgfx::Color(bg));
        // Checkered ends.
        for y in 0..4 {
            for x in 0..5 {
                let col = if (x + y) % 2 == 0 { 0xFFFFFFFF } else { 0xFF101010 };
                c.fill_rect(vgfx::Rect::new(x * 8, y * 8, 8, 8), vgfx::Color(col));
                c.fill_rect(vgfx::Rect::new(w - 40 + x * 8, y * 8, 8, 8), vgfx::Color(col));
            }
        }
        c.fill_rect(vgfx::Rect::new(40, h - 4, w - 80, 4), vgfx::Color(accent));
        let size = 22.0;
        let tw = text.measure(font, size, label);
        text.draw(&mut c, font, size, (w as f32 - tw) / 2.0, 23.0, label, vgfx::Color(fg));
    }
    Texture::new(w as u32, h as u32, &bmp.pixels)
}

/// A crowd texture: rows of colourful spectators.
pub fn crowd_texture(seed: u64) -> Texture {
    let mut rng = Rng::new(seed);
    let shirts = [0xFFD83A3A, 0xFF3A6AD8, 0xFFF2F2F2, 0xFFF2C23A, 0xFF3AA858, 0xFF2A2A2A, 0xFFE87A2A];
    let mut cols = vec![0u32; 32];
    for c in &mut cols {
        *c = shirts[rng.below(shirts.len() as u32) as usize];
    }
    Texture::from_fn(64, 16, |x, y| {
        let person = (x / 2) as usize % 32;
        if y < 5 {
            0xFF2A2A2E
        } else if y < 8 {
            if x % 2 == 0 { 0xFFE0B090 } else { 0xFF3A3A3E }
        } else {
            cols[(person + (y as usize / 8) * 7) % 32]
        }
    })
}
