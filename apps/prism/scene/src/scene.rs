//! The scene: a reflective torus knot on a pedestal of light, crystals
//! orbiting it, a fountain of sparks, all under a sky.
//!
//! It shows off OpenGL ES 3.0: the sky is rendered into a cube map at
//! start-up (render to texture), which the background and every reflection
//! sample; the sun casts shadows through a depth texture compared in the
//! shader; the crystals are one instanced draw; the sparks are simulated by
//! a vertex shader whose results transform feedback writes back to a
//! buffer, and drawn as point sprites with additive blending.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vgl::{Context, gl};
use vmath::f32 as m;
use vmath::{Mat4, Vec3};

use crate::mesh::{self, Mesh};
use crate::shaders::*;

/// Shadow map size.
const SHADOW_SIZE: i32 = 1024;
/// Sky cube map face size.
const SKY_SIZE: i32 = 128;
/// Sparks.
const PARTICLES: i32 = 1200;
/// Crystals.
const CRYSTALS: i32 = 18;

/// Attribute locations shared by the programs.
const POS: u32 = 0;
const NORMAL: u32 = 1;
const INST: u32 = 2;
const INST_COLOR: u32 = 3;
const VEL: u32 = 1;
const LIFE: u32 = 2;

/// What the user can switch.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub shadows: bool,
    pub particles: bool,
    pub paused: bool,
}

/// Camera controls for a frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Controls {
    /// Turn left/right and up/down (radians per second, scaled).
    pub yaw: f32,
    pub pitch: f32,
    /// Move closer (negative) or away.
    pub zoom: f32,
}

/// A linked program and the uniform locations it needs.
struct Program {
    id: u32,
    loc: Vec<(&'static str, i32)>,
}

impl Program {
    fn new(
        gl: &mut Context,
        vs: &str,
        fs: &str,
        attribs: &[(u32, &str)],
        feedback: &[&str],
        uniforms: &[&'static str],
    ) -> Result<Program, String> {
        let shader = |gl: &mut Context, kind: u32, src: &str| -> Result<u32, String> {
            let s = gl.create_shader(kind);
            gl.shader_source(s, &[src]);
            gl.compile_shader(s);
            if gl.get_shaderiv(s, gl::COMPILE_STATUS) == 0 {
                let what = if kind == gl::VERTEX_SHADER { "vertex" } else { "fragment" };
                return Err(format!("{what} shader: {}", gl.get_shader_info_log(s)));
            }
            Ok(s)
        };
        let v = shader(gl, gl::VERTEX_SHADER, vs)?;
        let f = shader(gl, gl::FRAGMENT_SHADER, fs)?;
        let id = gl.create_program();
        gl.attach_shader(id, v);
        gl.attach_shader(id, f);
        for &(index, name) in attribs {
            gl.bind_attrib_location(id, index, name);
        }
        if !feedback.is_empty() {
            gl.transform_feedback_varyings(id, feedback, gl::INTERLEAVED_ATTRIBS);
        }
        gl.link_program(id);
        gl.delete_shader(v);
        gl.delete_shader(f);
        if gl.get_programiv(id, gl::LINK_STATUS) == 0 {
            return Err(format!("link: {}", gl.get_program_info_log(id)));
        }
        let loc = uniforms.iter().map(|&u| (u, gl.get_uniform_location(id, u))).collect();
        Ok(Program { id, loc })
    }

    fn at(&self, name: &str) -> i32 {
        self.loc.iter().find(|(n, _)| *n == name).map_or(-1, |&(_, l)| l)
    }
}

/// A mesh in buffers, with its vertex array.
struct Geometry {
    vao: u32,
    count: i32,
}

/// The scene and its GL objects.
pub struct Scene {
    pub options: Options,
    time: f32,
    yaw: f32,
    pitch: f32,
    distance: f32,
    sun: Vec3,
    object: Program,
    depth: Program,
    background: Program,
    update: Program,
    sparks: Program,
    knot: Geometry,
    crystal: Geometry,
    floor: Geometry,
    fullscreen: u32,
    sky: u32,
    shadow_map: u32,
    shadow_fbo: u32,
    /// Particle state, twice: read one, write the other.
    particle_buffers: [u32; 2],
    update_vaos: [u32; 2],
    draw_vaos: [u32; 2],
    current: usize,
}

const OBJECT_UNIFORMS: &[&str] = &[
    "model",
    "viewProj",
    "lightViewProj",
    "instanced",
    "time",
    "baseColor",
    "eye",
    "sunDir",
    "sunColor",
    "reflectivity",
    "floorPattern",
    "shadows",
    "env",
    "shadowMap",
];

fn floats_as_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn mat(m: &Mat4) -> [f32; 16] {
    let c = [m.x_axis, m.y_axis, m.z_axis, m.w_axis];
    let mut out = [0.0; 16];
    for (i, v) in c.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&[v.x, v.y, v.z, v.w]);
    }
    out
}

/// Uploads a mesh: interleaved positions and normals, and indices.
fn upload(gl: &mut Context, m: &Mesh) -> Geometry {
    let vao = gl.gen_vertex_array();
    gl.bind_vertex_array(vao);
    let vbo = gl.gen_buffer();
    gl.bind_buffer(gl::ARRAY_BUFFER, vbo);
    gl.buffer_data(gl::ARRAY_BUFFER, &floats_as_bytes(&m.vertices), gl::STATIC_DRAW);
    gl.vertex_attrib_pointer(POS, 3, gl::FLOAT, false, 24, 0);
    gl.vertex_attrib_pointer(NORMAL, 3, gl::FLOAT, false, 24, 12);
    gl.enable_vertex_attrib_array(POS);
    gl.enable_vertex_attrib_array(NORMAL);
    let ibo = gl.gen_buffer();
    gl.bind_buffer(gl::ELEMENT_ARRAY_BUFFER, ibo);
    let idx: Vec<u8> = m.indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    gl.buffer_data(gl::ELEMENT_ARRAY_BUFFER, &idx, gl::STATIC_DRAW);
    gl.bind_vertex_array(0);
    Geometry { vao, count: m.indices.len() as i32 }
}

impl Scene {
    /// Builds the scene: compiles the shaders, uploads the meshes, renders
    /// the sky into its cube map.
    pub fn new(gl: &mut Context) -> Result<Scene, String> {
        let object_attribs = [(POS, "pos"), (NORMAL, "normal"), (INST, "inst"), (INST_COLOR, "instColor")];
        let object = Program::new(gl, OBJECT_VS, OBJECT_FS, &object_attribs, &[], OBJECT_UNIFORMS)?;
        let depth = Program::new(gl, OBJECT_VS, DEPTH_FS, &object_attribs, &[], OBJECT_UNIFORMS)?;
        let background = Program::new(gl, BACKGROUND_VS, BACKGROUND_FS, &[(POS, "pos")], &[], &["invViewProj", "sky"])?;
        let update = Program::new(
            gl,
            PARTICLE_UPDATE_VS,
            DEPTH_FS,
            &[(POS, "pos"), (VEL, "vel"), (LIFE, "life")],
            &["outPos", "outVel", "outLife"],
            &["time", "dt"],
        )?;
        let sparks = Program::new(
            gl,
            PARTICLE_DRAW_VS,
            PARTICLE_DRAW_FS,
            &[(POS, "pos"), (LIFE, "life")],
            &[],
            &["viewProj", "pointScale"],
        )?;
        let sky_program =
            Program::new(gl, FULLSCREEN_VS, SKY_FS, &[(POS, "pos")], &[], &["forward", "right", "up", "sunDir"])?;

        let knot = upload(gl, &mesh::torus_knot(2.0, 3.0, 2.6, 0.32, 192, 20));
        let floor = upload(gl, &mesh::disc(40.0, 96));
        let crystal = upload(gl, &mesh::crystal());
        // Per-crystal orbit angle, radius, height, scale and colour.
        gl.bind_vertex_array(crystal.vao);
        let mut inst = Vec::new();
        for i in 0..CRYSTALS {
            let t = i as f32 / CRYSTALS as f32;
            let angle = t * core::f32::consts::TAU;
            let radius = 4.6 + 0.9 * m::sin(t * 37.0);
            let height = 1.2 + 1.6 * (0.5 + 0.5 * m::sin(t * 23.0));
            let hue = t * 6.0;
            let color = [0.5 + 0.5 * m::cos(hue), 0.5 + 0.5 * m::cos(hue - 2.1), 0.5 + 0.5 * m::cos(hue + 2.1)];
            inst.extend_from_slice(&[angle, radius, height, 0.38 + 0.12 * m::cos(t * 11.0)]);
            inst.extend_from_slice(&color);
        }
        let ib = gl.gen_buffer();
        gl.bind_buffer(gl::ARRAY_BUFFER, ib);
        gl.buffer_data(gl::ARRAY_BUFFER, &floats_as_bytes(&inst), gl::STATIC_DRAW);
        gl.vertex_attrib_pointer(INST, 4, gl::FLOAT, false, 28, 0);
        gl.vertex_attrib_pointer(INST_COLOR, 3, gl::FLOAT, false, 28, 16);
        gl.enable_vertex_attrib_array(INST);
        gl.enable_vertex_attrib_array(INST_COLOR);
        gl.vertex_attrib_divisor(INST, 1);
        gl.vertex_attrib_divisor(INST_COLOR, 1);
        gl.bind_vertex_array(0);

        // A triangle covering the viewport.
        let fullscreen = gl.gen_vertex_array();
        gl.bind_vertex_array(fullscreen);
        let fb = gl.gen_buffer();
        gl.bind_buffer(gl::ARRAY_BUFFER, fb);
        gl.buffer_data(gl::ARRAY_BUFFER, &floats_as_bytes(&[-1.0, -1.0, 3.0, -1.0, -1.0, 3.0]), gl::STATIC_DRAW);
        gl.vertex_attrib_pointer(POS, 2, gl::FLOAT, false, 0, 0);
        gl.enable_vertex_attrib_array(POS);
        gl.bind_vertex_array(0);

        let sun = Vec3::new(0.48, 0.62, -0.62).normalize();

        // The sky, rendered into each face of a cube map.
        let sky = gl.gen_texture();
        gl.bind_texture(gl::TEXTURE_CUBE_MAP, sky);
        gl.tex_storage_2d(gl::TEXTURE_CUBE_MAP, 1, gl::RGBA8, SKY_SIZE, SKY_SIZE);
        gl.tex_parameteri(gl::TEXTURE_CUBE_MAP, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl.tex_parameteri(gl::TEXTURE_CUBE_MAP, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, -1.0, 0.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, -1.0, 0.0]),
            ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, -1.0, 0.0]),
            ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, -1.0, 0.0]),
        ];
        let fbo = gl.gen_framebuffer();
        gl.bind_framebuffer(gl::FRAMEBUFFER, fbo);
        gl.use_program(sky_program.id);
        gl.uniform3f(sky_program.at("sunDir"), sun.x, sun.y, sun.z);
        gl.bind_vertex_array(fullscreen);
        gl.viewport(0, 0, SKY_SIZE, SKY_SIZE);
        for (i, (f, r, u)) in faces.iter().enumerate() {
            gl.framebuffer_texture_2d(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_CUBE_MAP_POSITIVE_X + i as u32,
                sky,
                0,
            );
            gl.uniform3f(sky_program.at("forward"), f[0], f[1], f[2]);
            gl.uniform3f(sky_program.at("right"), r[0], r[1], r[2]);
            gl.uniform3f(sky_program.at("up"), u[0], u[1], u[2]);
            gl.draw_arrays(gl::TRIANGLES, 0, 3);
        }
        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        gl.delete_framebuffers(&[fbo]);
        gl.delete_program(sky_program.id);

        // The shadow map: a depth texture compared when sampled.
        let shadow_map = gl.gen_texture();
        gl.bind_texture(gl::TEXTURE_2D, shadow_map);
        gl.tex_storage_2d(gl::TEXTURE_2D, 1, gl::DEPTH_COMPONENT24, SHADOW_SIZE, SHADOW_SIZE);
        gl.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        gl.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
        gl.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
        gl.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_COMPARE_MODE, gl::COMPARE_REF_TO_TEXTURE as i32);
        gl.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_COMPARE_FUNC, gl::LEQUAL as i32);
        let shadow_fbo = gl.gen_framebuffer();
        gl.bind_framebuffer(gl::FRAMEBUFFER, shadow_fbo);
        gl.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::DEPTH_ATTACHMENT, gl::TEXTURE_2D, shadow_map, 0);
        gl.draw_buffers(&[gl::NONE]);
        gl.read_buffer(gl::NONE);
        if gl.check_framebuffer_status(gl::FRAMEBUFFER) != gl::FRAMEBUFFER_COMPLETE {
            return Err("the shadow framebuffer is incomplete".into());
        }
        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);

        // Particles: position, velocity, remaining life (all expired, so
        // they start in the fountain, staggered).
        let mut state = Vec::new();
        for i in 0..PARTICLES {
            state.extend_from_slice(&[0.0, -10.0, 0.0, 0.0, 0.0, 0.0, -(i as f32) / PARTICLES as f32 * 3.0]);
        }
        let mut particle_buffers = [0; 2];
        let mut update_vaos = [0; 2];
        let mut draw_vaos = [0; 2];
        for k in 0..2 {
            let b = gl.gen_buffer();
            gl.bind_buffer(gl::ARRAY_BUFFER, b);
            gl.buffer_data(gl::ARRAY_BUFFER, &floats_as_bytes(&state), gl::DYNAMIC_COPY);
            particle_buffers[k] = b;
            let u = gl.gen_vertex_array();
            gl.bind_vertex_array(u);
            gl.vertex_attrib_pointer(POS, 3, gl::FLOAT, false, 28, 0);
            gl.vertex_attrib_pointer(VEL, 3, gl::FLOAT, false, 28, 12);
            gl.vertex_attrib_pointer(LIFE, 1, gl::FLOAT, false, 28, 24);
            for a in [POS, VEL, LIFE] {
                gl.enable_vertex_attrib_array(a);
            }
            update_vaos[k] = u;
            let d = gl.gen_vertex_array();
            gl.bind_vertex_array(d);
            gl.vertex_attrib_pointer(POS, 3, gl::FLOAT, false, 28, 0);
            gl.vertex_attrib_pointer(LIFE, 1, gl::FLOAT, false, 28, 24);
            gl.enable_vertex_attrib_array(POS);
            gl.enable_vertex_attrib_array(LIFE);
            draw_vaos[k] = d;
        }
        gl.bind_vertex_array(0);

        // Samplers: the sky on unit 0, the shadow map on unit 1.
        for p in [&object, &depth] {
            gl.use_program(p.id);
            gl.uniform1i(p.at("env"), 0);
            gl.uniform1i(p.at("shadowMap"), 1);
        }
        gl.use_program(background.id);
        gl.uniform1i(background.at("sky"), 0);
        gl.use_program(0);
        let e = gl.get_error();
        if e != gl::NO_ERROR {
            return Err(format!("GL error {e:#x} while building the scene"));
        }
        Ok(Scene {
            options: Options { shadows: true, particles: true, paused: false },
            time: 0.0,
            yaw: 0.6,
            pitch: 0.32,
            distance: 13.5,
            sun,
            object,
            depth,
            background,
            update,
            sparks,
            knot,
            crystal,
            floor,
            fullscreen,
            sky,
            shadow_map,
            shadow_fbo,
            particle_buffers,
            update_vaos,
            draw_vaos,
            current: 0,
        })
    }

    /// Advances the animation by `dt` seconds.
    pub fn update(&mut self, dt: f32, c: Controls) {
        if !self.options.paused {
            self.time += dt;
            self.yaw += dt * 0.12;
        }
        self.yaw += c.yaw * dt;
        self.pitch = (self.pitch + c.pitch * dt).clamp(0.05, 1.3);
        self.distance = (self.distance * (1.0 + c.zoom * dt)).clamp(6.0, 30.0);
    }

    /// Simulates the sparks for `dt` seconds (transform feedback).
    fn simulate(&mut self, gl: &mut Context, dt: f32) {
        let (src, dst) = (self.current, 1 - self.current);
        gl.use_program(self.update.id);
        gl.uniform1f(self.update.at("time"), self.time);
        gl.uniform1f(self.update.at("dt"), dt);
        gl.bind_vertex_array(self.update_vaos[src]);
        gl.bind_buffer_base(gl::TRANSFORM_FEEDBACK_BUFFER, 0, self.particle_buffers[dst]);
        gl.enable(gl::RASTERIZER_DISCARD);
        gl.begin_transform_feedback(gl::POINTS);
        gl.draw_arrays(gl::POINTS, 0, PARTICLES);
        gl.end_transform_feedback();
        gl.disable(gl::RASTERIZER_DISCARD);
        gl.bind_buffer_base(gl::TRANSFORM_FEEDBACK_BUFFER, 0, 0);
        self.current = dst;
    }

    /// The light's view and projection (an orthographic box around the
    /// scene, seen from the sun).
    fn light_view_proj(&self) -> Mat4 {
        let view = Mat4::look_at_rh(self.sun * 20.0, Vec3::ZERO, Vec3::new(0.0, 1.0, 0.0));
        Mat4::orthographic_rh(-9.0, 9.0, -9.0, 9.0, 5.0, 35.0) * view
    }

    fn knot_model(&self) -> Mat4 {
        Mat4::from_translation(Vec3::new(0.0, 2.6, 0.0))
            * Mat4::from_rotation_y(self.time * 0.5)
            * Mat4::from_rotation_x(0.35 + 0.15 * m::sin(self.time * 0.7))
    }

    /// Draws the knot, the crystals and the floor with `p`.
    fn draw_objects(&self, gl: &mut Context, p: &Program, floor: bool) {
        let identity = mat(&Mat4::IDENTITY);
        gl.uniform1i(p.at("instanced"), 0);
        gl.uniform_matrix4fv(p.at("model"), false, &mat(&self.knot_model()));
        gl.uniform3f(p.at("baseColor"), 0.55, 0.62, 0.78);
        gl.uniform1f(p.at("reflectivity"), 0.85);
        gl.uniform1i(p.at("floorPattern"), 0);
        gl.bind_vertex_array(self.knot.vao);
        gl.draw_elements(gl::TRIANGLES, self.knot.count, gl::UNSIGNED_SHORT, 0);
        gl.uniform1i(p.at("instanced"), 1);
        gl.uniform1f(p.at("reflectivity"), 0.35);
        gl.bind_vertex_array(self.crystal.vao);
        gl.draw_elements_instanced(gl::TRIANGLES, self.crystal.count, gl::UNSIGNED_SHORT, 0, CRYSTALS);
        if floor {
            gl.uniform1i(p.at("instanced"), 0);
            gl.uniform_matrix4fv(p.at("model"), false, &identity);
            gl.uniform1f(p.at("reflectivity"), 0.08);
            gl.uniform1i(p.at("floorPattern"), 1);
            gl.bind_vertex_array(self.floor.vao);
            gl.draw_elements(gl::TRIANGLES, self.floor.count, gl::UNSIGNED_SHORT, 0);
        }
    }

    /// Renders a frame into the bound framebuffer (`width` x `height`).
    pub fn render(&mut self, gl: &mut Context, width: i32, height: i32, dt: f32) {
        if self.options.particles && !self.options.paused {
            // Real time, in steps short enough for the bounces.
            let steps = m::ceil(dt * 30.0).clamp(1.0, 4.0);
            for _ in 0..steps as u32 {
                self.simulate(gl, (dt / steps).min(1.0 / 30.0));
            }
        }
        let light = self.light_view_proj();
        // The shadow map.
        if self.options.shadows {
            gl.bind_framebuffer(gl::FRAMEBUFFER, self.shadow_fbo);
            gl.viewport(0, 0, SHADOW_SIZE, SHADOW_SIZE);
            gl.enable(gl::DEPTH_TEST);
            gl.depth_func(gl::LESS);
            gl.depth_mask(true);
            gl.clear(gl::DEPTH_BUFFER_BIT);
            gl.enable(gl::POLYGON_OFFSET_FILL);
            gl.polygon_offset(1.5, 3.0);
            gl.use_program(self.depth.id);
            gl.uniform_matrix4fv(self.depth.at("viewProj"), false, &mat(&light));
            gl.uniform1f(self.depth.at("time"), self.time);
            self.draw_objects(gl, &self.depth, false);
            gl.disable(gl::POLYGON_OFFSET_FILL);
            gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        }
        // The view.
        let eye = Vec3::new(
            self.distance * m::cos(self.pitch) * m::sin(self.yaw),
            1.2 + self.distance * m::sin(self.pitch),
            self.distance * m::cos(self.pitch) * m::cos(self.yaw),
        );
        let view = Mat4::look_at_rh(eye, Vec3::new(0.0, 1.8, 0.0), Vec3::new(0.0, 1.0, 0.0));
        let aspect = width as f32 / height.max(1) as f32;
        let fov = 0.9f32;
        let proj = Mat4::perspective_rh(fov, aspect, 0.3, 80.0);
        let view_proj = proj * view;
        gl.viewport(0, 0, width, height);
        gl.enable(gl::DEPTH_TEST);
        gl.depth_func(gl::LESS);
        gl.depth_mask(true);
        gl.clear(gl::DEPTH_BUFFER_BIT);
        gl.active_texture(gl::TEXTURE0);
        gl.bind_texture(gl::TEXTURE_CUBE_MAP, self.sky);
        gl.active_texture(gl::TEXTURE1);
        gl.bind_texture(gl::TEXTURE_2D, self.shadow_map);
        gl.active_texture(gl::TEXTURE0);
        let p = &self.object;
        gl.use_program(p.id);
        gl.uniform_matrix4fv(p.at("viewProj"), false, &mat(&view_proj));
        gl.uniform_matrix4fv(p.at("lightViewProj"), false, &mat(&light));
        gl.uniform1f(p.at("time"), self.time);
        gl.uniform3f(p.at("eye"), eye.x, eye.y, eye.z);
        gl.uniform3f(p.at("sunDir"), self.sun.x, self.sun.y, self.sun.z);
        gl.uniform3f(p.at("sunColor"), 1.35, 1.12, 0.92);
        gl.uniform1i(p.at("shadows"), i32::from(self.options.shadows));
        self.draw_objects(gl, &self.object, true);
        // Sparks: additive, tested against the depth but not written.
        if self.options.particles {
            gl.use_program(self.sparks.id);
            gl.uniform_matrix4fv(self.sparks.at("viewProj"), false, &mat(&view_proj));
            gl.uniform1f(self.sparks.at("pointScale"), height as f32 / (2.0 * m::tan(fov * 0.5)));
            gl.bind_vertex_array(self.draw_vaos[self.current]);
            gl.depth_mask(false);
            gl.enable(gl::BLEND);
            gl.blend_func(gl::ONE, gl::ONE);
            gl.draw_arrays(gl::POINTS, 0, PARTICLES);
            gl.disable(gl::BLEND);
            gl.depth_mask(true);
        }
        // The sky, where nothing else is.
        gl.depth_func(gl::LEQUAL);
        gl.depth_mask(false);
        gl.use_program(self.background.id);
        gl.uniform_matrix4fv(self.background.at("invViewProj"), false, &mat(&view_proj.inverse()));
        gl.bind_vertex_array(self.fullscreen);
        gl.draw_arrays(gl::TRIANGLES, 0, 3);
        gl.depth_mask(true);
        gl.depth_func(gl::LESS);
        gl.bind_vertex_array(0);
    }
}
