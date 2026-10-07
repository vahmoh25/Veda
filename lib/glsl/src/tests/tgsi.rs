//! The TGSI back end: every program translates into well-formed text
//! (balanced control flow, every register declared, nothing virglrenderer
//! cannot read). That the host runs it correctly is tested in `vgl`,
//! against virglrenderer itself.

use std::format;
use std::string::String;
use std::vec::Vec;

use super::ir::program;
use crate::tgsi::{Options, Shader, translate};

/// Translates both stages of a program, checking each.
fn both(vs: &str, fs: &str) -> (Shader, Shader) {
    let p = program(vs, fs);
    let v = translate(&p.vertex, &p.linked, &Options::default());
    let f = translate(&p.fragment, &p.linked, &Options::default());
    check(&v.text);
    check(&f.text);
    (v, f)
}

/// The number in `FILE[n]` at the start of `s`.
fn index_after(s: &str, file: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let pat = format!("{file}[");
    let mut rest = s;
    while let Some(p) = rest.find(&pat) {
        // Not part of a longer name ("SVIEW[" contains no "IN[", but
        // "ADDR[0].x+" follows "CONST[").
        let before = rest[..p].chars().last();
        rest = &rest[p + pat.len()..];
        if before.is_some_and(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse() {
            out.push(n);
        }
    }
    out
}

/// The `n` of a declaration `DCL FILE[0..n]`.
fn declared_range(text: &str, file: &str) -> Option<u32> {
    let pat = format!("DCL {file}[0..");
    let p = text.find(&pat)?;
    let digits: String = text[p + pat.len()..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn check(text: &str) {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert!(lines[0] == "VERT" || lines[0] == "FRAG", "{text}");
    assert_eq!(lines.last(), Some(&"END"), "{text}");
    let mut ifs = 0i32;
    let mut loops = 0i32;
    let mut seen_code = false;
    for l in &lines[1..] {
        let word = l.split([' ', '[']).next().unwrap_or("");
        match word {
            "DCL" | "IMM" | "PROPERTY" => assert!(!seen_code, "declaration after code:\n{text}"),
            _ => seen_code = true,
        }
        match word {
            "UIF" | "IF" => ifs += 1,
            "ENDIF" => ifs -= 1,
            "BGNLOOP" => loops += 1,
            "ENDLOOP" => loops -= 1,
            "CONT" => panic!("virglrenderer has no CONT:\n{text}"),
            _ => {}
        }
        assert!(ifs >= 0 && loops >= 0, "unbalanced control flow:\n{text}");
        // Sources always have four-component swizzles.
        if !matches!(word, "DCL" | "IMM" | "PROPERTY") {
            for operand in l.split(", ").skip(1) {
                let operand = operand.trim_start_matches(['-', '|']).trim_end_matches('|');
                if let Some(dot) = operand.rfind("].") {
                    let swz = &operand[dot + 2..];
                    let target_like = swz.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                    assert!(
                        swz.len() == 4 || target_like || operand.starts_with("IMM") && swz == "xyz",
                        "short swizzle in `{l}`:\n{text}"
                    );
                }
            }
        }
    }
    assert_eq!((ifs, loops), (0, 0), "unbalanced control flow:\n{text}");
    let temps = declared_range(text, "TEMP").map_or(0, |n| n + 1);
    for t in index_after(text, "TEMP") {
        assert!(t < temps, "TEMP[{t}] not declared (have {temps}):\n{text}");
    }
    let imms = lines.iter().filter(|l| l.starts_with("IMM[")).count() as u32;
    for i in index_after(text, "IMM") {
        assert!(i < imms, "IMM[{i}] not declared:\n{text}");
    }
}

#[test]
fn a_transform_and_a_color() {
    let (v, f) = both(
        "attribute vec4 p; attribute vec2 uv; uniform mat4 mvp; varying vec2 t;\n\
         void main() { t = uv; gl_Position = mvp * p; }",
        "precision mediump float; varying vec2 t; uniform vec4 tint;\n\
         void main() { gl_FragColor = vec4(t, 0.0, 1.0) * tint; }",
    );
    assert!(v.text.contains("DCL OUT[0], POSITION"), "{}", v.text);
    assert!(v.text.contains("GENERIC[0]"), "{}", v.text);
    assert!(f.text.contains("PROPERTY FS_COLOR0_WRITES_ALL_CBUFS 1"), "{}", f.text);
    assert!(f.text.contains("DCL IN[0], GENERIC[0], PERSPECTIVE"), "{}", f.text);
    assert!(f.text.contains("DCL OUT[0], COLOR[0]"), "{}", f.text);
    assert_eq!(v.position, Some(0));
    assert_eq!(v.varyings, std::vec![1]);
}

#[test]
fn loops_continue_through_a_flag() {
    let (v, _) = both(
        "#version 300 es\nuniform int n; out float r;\n\
         void main() {\n\
           float acc = 0.0;\n\
           for (int i = 0; i < 10; i++) {\n\
             if (i == n) continue;\n\
             if (i > 7) { if (acc > 3.0) continue; acc += 0.5; }\n\
             for (int j = 0; j < i; ++j) { if (j > 3) break; acc += float(j); }\n\
             acc *= 1.5;\n\
           }\n\
           r = acc; gl_Position = vec4(acc);\n\
         }",
        "#version 300 es\nprecision mediump float; in float r; out vec4 c; void main() { c = vec4(r); }",
    );
    assert!(v.text.contains("BGNLOOP"), "{}", v.text);
    assert!(v.text.contains("BRK"), "{}", v.text);
}

#[test]
fn textures_of_every_kind() {
    let (_, f) = both(
        "#version 300 es\nin vec4 p; out vec4 q; void main() { q = p; gl_Position = p; }",
        "#version 300 es\nprecision highp float;\n\
         uniform sampler2D a; uniform highp sampler3D b; uniform samplerCube c; uniform highp sampler2DArray d;\n\
         uniform highp sampler2DShadow e; uniform highp samplerCubeShadow g; uniform highp sampler2DArrayShadow h;\n\
         uniform highp isampler2D i; uniform highp usampler2D u;\n\
         in vec4 q; out vec4 o;\n\
         void main() {\n\
           vec4 s = texture(a, q.xy) + texture(a, q.xy, 1.0) + textureLod(a, q.xy, 2.0)\n\
             + textureOffset(a, q.xy, ivec2(1, -2)) + textureGrad(a, q.xy, q.zw, q.wz)\n\
             + texelFetch(a, ivec2(q.xy), 0) + textureProj(a, q.xyz)\n\
             + texture(b, q.xyz) + texture(c, q.xyz) + textureLod(c, q.xyz, 1.0) + texture(d, q.xyz);\n\
           float z = texture(e, q.xyz) + texture(g, q) + texture(h, q) + textureLod(e, q.xyz, 0.0);\n\
           ivec2 sz = textureSize(a, 0) + textureSize(c, 1);\n\
           ivec3 sz3 = textureSize(b, 0) + textureSize(d, 0);\n\
           o = s + vec4(z) + vec4(sz.xyxy) + vec4(sz3, 0.0)\n\
             + vec4(texture(i, q.xy)) + vec4(texelFetch(u, ivec2(q.xy), 0));\n\
         }",
    );
    for needle in [
        "TEX ",
        "TXB ",
        "TXL ",
        "TXD ",
        "TXF ",
        "TXQ ",
        "SHADOW2D",
        "SHADOWCUBE",
        "SHADOW2D_ARRAY",
        "2D_ARRAY",
        "CUBE",
        "3D",
        "SINT",
        "UINT",
    ] {
        assert!(f.text.contains(needle), "no {needle}:\n{}", f.text);
    }
    assert_eq!(f.samplers.len(), 9);
}

#[test]
fn indexing_uniforms_blocks_and_samplers() {
    let (v, f) = both(
        "#version 300 es\nuniform vec4 table[8]; uniform int k; in vec4 p;\n\
         layout(std140) uniform Lights { vec4 color[4]; mat3 m; float scale[3]; };\n\
         out vec4 q;\n\
         void main() { q = table[k] + color[k & 3] + vec4(m[k % 3], scale[k % 3]); gl_Position = p; }",
        "#version 300 es\nprecision mediump float; in vec4 q; out vec4 c; void main() { c = q; }",
    );
    assert!(v.text.contains("UARL ADDR[0].x"), "{}", v.text);
    assert!(v.text.contains("CONST[1][ADDR[0].x+"), "{}", v.text);
    assert_eq!(v.blocks, std::vec![0]);
    assert!(f.text.contains("DCL OUT[0], COLOR[0]"), "{}", f.text);
    // GLSL ES 1.00 indexes sampler arrays with loop indices.
    let (_, f) = both(
        "attribute vec4 p; void main() { gl_Position = p; }",
        "precision mediump float; uniform sampler2D s[3];\n\
         void main() { gl_FragColor = vec4(0.0);\n\
           for (int i = 0; i < 3; i++) gl_FragColor += texture2D(s[i], vec2(0.5)); }",
    );
    assert!(f.text.contains("SAMP[2]"), "{}", f.text);
}

#[test]
fn functions_the_host_lacks() {
    let (_, f) = both(
        "#version 300 es\nin vec4 p; out vec4 q; void main() { q = p; gl_Position = p; }",
        "#version 300 es\nprecision highp float; in vec4 q; out vec4 o;\n\
         void main() {\n\
           vec4 a = asin(q) + acos(q) + atan(q) + atan(q, q.yzwx) + tan(q);\n\
           vec4 b = sinh(q) + cosh(q) + tanh(q) + asinh(q) + acosh(q + 2.0) + atanh(q * 0.5);\n\
           bvec4 n = isnan(q); bvec4 i = isinf(q);\n\
           uint h = packHalf2x16(q.xy); vec2 back = unpackHalf2x16(h);\n\
           o = a + b + vec4(n) + vec4(i) + vec4(back, exp(q.x), log(q.y));\n\
         }",
    );
    assert!(f.text.contains("EX2"), "{}", f.text);
    assert!(f.text.contains("LG2"), "{}", f.text);
}

#[test]
fn integers_and_flat_varyings() {
    let (v, f) = both(
        "#version 300 es\nin ivec4 p; flat out ivec2 m; flat out uint w;\n\
         void main() { m = p.xy / (p.zw | 1) + (p.xy % 7) - (p.xy << 2) + (p.xy >> 1);\n\
           w = uint(p.x) * 3u + (uint(p.y) >> 3u); gl_Position = vec4(p); }",
        "#version 300 es\nprecision highp float; flat in ivec2 m; flat in uint w; layout(location = 1) out ivec4 c;\n\
         void main() { c = ivec4(m, int(w), abs(m.x) * sign(m.y)); }",
    );
    assert!(f.text.contains("CONSTANT"), "{}", f.text);
    assert!(f.text.contains("COLOR[1]"), "{}", f.text);
    assert!(v.text.contains("IDIV"), "{}", v.text);
}

#[test]
fn fragment_built_ins() {
    let (v, f) = both(
        "#version 300 es\nin vec4 p; void main() { gl_PointSize = 4.0; gl_Position = p * float(gl_VertexID + gl_InstanceID); }",
        "#version 300 es\nprecision highp float; out vec4 c;\n\
         void main() { c = vec4(gl_FragCoord.xy, gl_PointCoord) * (gl_FrontFacing ? 1.0 : 0.5);\n\
           if (c.x > 100.0) discard; gl_FragDepth = 0.25; }",
    );
    assert!(v.text.contains("VERTEXID"), "{}", v.text);
    assert!(v.text.contains("INSTANCEID"), "{}", v.text);
    assert!(v.text.contains("PSIZE"), "{}", v.text);
    assert!(v.point_size.is_some());
    for needle in ["POSITION, LINEAR", "FACE", "PCOORD", "KILL", "OUT[1].z"] {
        assert!(f.text.contains(needle), "no {needle}:\n{}", f.text);
    }
}

#[test]
fn derivatives_and_selects() {
    let (_, f) = both(
        "#version 300 es\nin vec4 p; out vec2 t; void main() { t = p.xy; gl_Position = p; }",
        "#version 300 es\nprecision highp float; in vec2 t; out vec4 c;\n\
         void main() { vec2 d = dFdx(t) + dFdy(t) + fwidth(t); c = vec4(d, t.x > 0.5 ? t.y : -t.y, 1.0); }",
    );
    assert!(f.text.contains("DDX") && f.text.contains("DDY"), "{}", f.text);
    assert!(f.text.contains("UCMP"), "{}", f.text);
}
