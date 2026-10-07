//! Linking and the SSA form: every program links, and its IR satisfies the
//! verifier's invariants.

use std::format;
use std::string::String;

use crate::ir::print::print;
use crate::link::Bindings;
use crate::program::{Program, link};
use crate::{Options, Stage, compile};

pub(crate) fn program(vs: &str, fs: &str) -> Program {
    let o = Options::default();
    let v = compile(Stage::Vertex, &[vs], &o);
    assert!(v.log.is_empty(), "vertex shader:\n{vs}\n{}", v.log);
    let f = compile(Stage::Fragment, &[fs], &o);
    assert!(f.log.is_empty(), "fragment shader:\n{fs}\n{}", f.log);
    let r = link(&v.shader.unwrap(), &f.shader.unwrap(), &Bindings::default(), &o.limits);
    assert!(r.log.is_empty(), "link:\n{}", r.log);
    r.program.expect("linked")
}

pub(crate) fn link_log(vs: &str, fs: &str) -> String {
    let o = Options::default();
    let v = compile(Stage::Vertex, &[vs], &o).shader.expect("vs");
    let f = compile(Stage::Fragment, &[fs], &o).shader.expect("fs");
    link(&v, &f, &Bindings::default(), &o.limits).log
}

const FS100: &str = "precision mediump float; void main() { gl_FragColor = vec4(1.0); }";
const FS300: &str = "#version 300 es\nprecision mediump float; out vec4 c; void main() { c = vec4(1.0); }";

fn vs_only(body: &str) -> Program {
    program(&format!("#version 300 es\n{body}"), FS300)
}

#[test]
fn a_simple_program_prints() {
    let p = program("attribute vec4 p; uniform float k; void main() { gl_Position = p * k; }", FS100);
    let text = print(&p.vertex);
    assert!(text.contains("input 0.x"), "{text}");
    assert!(text.contains("uniform 0.x"), "{text}");
    assert!(text.contains("gl_Position.w ="), "{text}");
    assert!(!text.contains("phi"), "no φ expected:\n{text}");
}

#[test]
fn loops_with_breaks_and_continues() {
    vs_only(
        r#"
        uniform int n;
        out float r;
        void main() {
            float acc = 0.0;
            for (int i = 0; i < 10; i++) {
                if (i == n) continue;
                for (int j = 0; j < i; ++j) {
                    if (j > 3) break;
                    acc += float(j);
                    if (acc > 100.0) { acc = -1.0; break; }
                }
                if (acc < 0.0) break;
            }
            int k = 0;
            do { k += 2; if (k == 6) continue; acc += 1.0; } while (k < n);
            while (bool b = k > 0) { k--; }
            r = acc;
            gl_Position = vec4(acc);
        }
    "#,
    );
}

#[test]
fn returns_inside_loops_and_branches() {
    let p = vs_only(
        r#"
        uniform int n;
        float f(int x) {
            for (int i = 0; i < 8; i++) {
                for (int j = 0; j < 8; j++) {
                    if (i * j == x) return float(i + j);
                }
                if (i == 7) return -1.0;
            }
            return 0.0;
        }
        float g(float y) { if (y > 0.0) return y; else return -y; }
        void main() { gl_Position = vec4(f(n), g(float(n)), 0.0, 1.0); }
    "#,
    );
    let text = print(&p.vertex);
    assert!(text.contains("loop"), "{text}");
}

#[test]
fn switch_with_fallthrough_and_continue() {
    vs_only(
        r#"
        uniform int n;
        void main() {
            float acc = 0.0;
            for (int i = 0; i < 4; i++) {
                switch (n + i) {
                    case 0: acc += 1.0;
                    case 1: acc += 2.0; break;
                    case 2: continue;
                    default: acc -= 1.0;
                    case 5: acc *= 2.0;
                }
                acc += 0.5;
            }
            gl_Position = vec4(acc);
        }
    "#,
    );
}

#[test]
fn out_parameters_and_side_effects() {
    vs_only(
        r#"
        uniform int n;
        void twice(inout float x, out float y) { y = x; x *= 2.0; }
        void main() {
            float a[4];
            int i = 0;
            a[0] = 1.0; a[1] = 2.0; a[2] = 3.0; a[3] = 4.0;
            twice(a[i++], a[i++]);
            float t;
            float m = modf(a[n & 3], t);
            bool b = (n > 2) && (++i > 3);
            gl_Position = vec4(a[0], a[1], m + t, b ? 1.0 : 0.0);
        }
    "#,
    );
}

#[test]
fn dynamic_indexing_everywhere() {
    let p = program(
        r#"#version 300 es
        uniform int n;
        uniform vec4 colors[8];
        uniform Block { vec4 v[4]; mat3 m[2]; } blk;
        struct S { float f[3]; vec2 g; };
        uniform S s[2];
        out vec4 o[2];
        void main() {
            vec4 local[3] = vec4[](vec4(1.0), vec4(2.0), vec4(3.0));
            local[n % 3].y = 5.0;
            o[n & 1] = colors[n] + blk.v[n] + vec4(blk.m[n & 1][n % 3], s[n & 1].f[n % 3]) + local[n % 3];
            o[1 - (n & 1)] = vec4(s[n & 1].g, 0.0, 0.0);
            gl_Position = vec4(1.0);
        }
    "#,
        r#"#version 300 es
        precision mediump float;
        in vec4 o[2];
        uniform int k;
        out vec4 c;
        void main() { c = o[k & 1]; }
    "#,
    );
    let text = print(&p.vertex);
    assert!(text.contains("uniform") && text.contains('['), "an indexed uniform load:\n{text}");
    assert!(text.contains("block"), "{text}");
    assert!(text.contains("select"), "{text}");
}

#[test]
fn textures_and_derivatives() {
    program(
        r#"#version 300 es
        in vec4 p; out vec2 uv;
        uniform sampler2D vt;
        void main() { uv = p.xy; gl_Position = p + textureLod(vt, p.xy, 0.0) + texture(vt, p.zw); }
    "#,
        r#"#version 300 es
        precision mediump float;
        precision highp sampler2DShadow;
        in vec2 uv;
        uniform sampler2D t[2];
        uniform sampler2DShadow sh;
        uniform samplerCube cube;
        out vec4 c;
        void main() {
            vec4 a = texture(t[1], uv) + textureProj(t[0], vec3(uv, 2.0)) + textureGrad(t[0], uv, dFdx(uv), dFdy(uv));
            float d = texture(sh, vec3(uv, 0.5)) + textureProjOffset(sh, vec4(uv, 0.5, 1.0), ivec2(1, 2));
            c = a * d + texture(cube, vec3(uv, 1.0), 1.0) + vec4(fwidth(uv), vec2(textureSize(t[0], 0)));
        }
    "#,
    );
}

#[test]
fn es2_programs_link() {
    program(
        r#"
        attribute vec3 pos;
        attribute mat2 rot;
        uniform mat4 mvp;
        varying vec2 v;
        varying float w[2];
        void main() { v = rot * pos.xy; w[0] = pos.z; w[1] = 1.0; gl_Position = mvp * vec4(pos, 1.0); gl_PointSize = 4.0; }
    "#,
        r#"
        #extension GL_OES_standard_derivatives : enable
        precision mediump float;
        varying vec2 v;
        varying float w[2];
        uniform sampler2D tex;
        void main() {
            vec4 c = texture2D(tex, v) * w[1] + texture2DProj(tex, vec4(v, 0.0, 2.0));
            gl_FragColor = c + vec4(dFdx(v.x), dFdy(v.y), gl_FragCoord.xy * 0.001);
            if (gl_FragColor.a < 0.0) discard;
        }
    "#,
    );
}

#[test]
fn link_errors() {
    let vs = "attribute vec4 p; void main() { gl_Position = p; }";
    let log =
        link_log(vs, "precision mediump float; varying vec2 v; void main() { gl_FragColor = vec4(v, 0.0, 1.0); }");
    assert!(log.contains("'v' is not declared by the vertex shader"), "{log}");
    let log = link_log(
        "varying vec3 v; void main() { v = vec3(1.0); gl_Position = vec4(0.0); }",
        "precision mediump float; varying vec2 v; void main() { gl_FragColor = vec4(v, 0.0, 1.0); }",
    );
    assert!(log.contains("is a 'vec3' in the vertex shader and a 'vec2'"), "{log}");
    let log = link_log(
        "uniform float u; void main() { gl_Position = vec4(u); }",
        "precision mediump float; uniform int u; void main() { gl_FragColor = vec4(float(u)); }",
    );
    assert!(log.contains("uniform 'u' has different types"), "{log}");
    let log = link_log(
        "#version 300 es\nflat out int v; void main() { v = 1; gl_Position = vec4(0.0); }",
        "#version 300 es\nprecision mediump float; in float v; out vec4 c; void main() { c = vec4(v); }",
    );
    assert!(log.contains("'v' is a 'int'"), "{log}");
    let log = link_log(
        "#version 300 es\nout float v; void main() { v = 1.0; gl_Position = vec4(0.0); }",
        "#version 300 es\nprecision mediump float; flat in float v; out vec4 c; void main() { c = vec4(v); }",
    );
    assert!(log.contains("different interpolation qualifiers"), "{log}");
    let log = link_log("#version 300 es\nvoid main() { gl_Position = vec4(0.0); }", FS100);
    assert!(log.contains("different GLSL ES versions"), "{log}");
}

#[test]
fn reflection_of_uniforms_and_attributes() {
    let p = program(
        r#"#version 300 es
        layout(location = 3) in vec4 a;
        in mat2 b;
        in float c;
        struct L { vec3 dir; float k[2]; };
        uniform L lights[2];
        uniform mat3 m;
        uniform float scalars[4];
        layout(std140) uniform Blk { vec3 x; float y; mat2 z; float arr[2]; } blk;
        void main() { gl_Position = a + vec4(b[0], c, lights[1].k[1]) + vec4(m[2], scalars[3]) + vec4(blk.x, blk.y); }
    "#,
        FS300,
    );
    let l = &p.linked;
    let names: std::vec::Vec<(&str, u32)> = l.attributes.iter().map(|a| (a.name.as_str(), a.location)).collect();
    assert_eq!(names, [("b", 0), ("c", 2), ("a", 3)]);
    let unames: std::vec::Vec<&str> = l.uniforms.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        unames,
        [
            "lights[0].dir",
            "lights[0].k[0]",
            "lights[1].dir",
            "lights[1].k[0]",
            "m",
            "scalars[0]",
            "Blk.x",
            "Blk.y",
            "Blk.z",
            "Blk.arr[0]"
        ]
    );
    // The API names block members by the block's name, not the
    // instance's (GLSL ES 3.00 section 4.3.7).
    // std140: vec3 at 0, float packs at 12, mat2 at 16 (two 16-byte
    // columns), float[2] at 48 with a 16-byte stride; block size 80.
    let off: std::vec::Vec<u32> = l.uniforms[6..].iter().map(|u| u.block.unwrap().offset).collect();
    assert_eq!(off, [0, 12, 16, 48]);
    assert_eq!(l.uniforms[9].block.unwrap().array_stride, 16);
    assert_eq!(l.blocks[0].size, 80);
}
