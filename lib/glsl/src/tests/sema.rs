//! The semantic checks and constant folding.

use std::format;
use std::string::String;
use std::vec::Vec;

use crate::hir::{Shader, VarKind};
use crate::ops::Value;
use crate::{Options, Stage, compile};

fn vs(src: &str) -> Shader {
    let c = compile(Stage::Vertex, &[src], &Options::default());
    assert!(c.log.is_empty(), "unexpected messages for vertex shader:\n{src}\n{}", c.log);
    c.shader.expect("compiled")
}

fn fs(src: &str) -> Shader {
    let c = compile(Stage::Fragment, &[src], &Options::default());
    assert!(c.log.is_empty(), "unexpected messages for fragment shader:\n{src}\n{}", c.log);
    c.shader.expect("compiled")
}

fn fails(stage: Stage, src: &str, needle: &str) {
    let c = compile(stage, &[src], &Options::default());
    assert!(c.shader.is_none(), "expected an error for:\n{src}");
    assert!(c.log.contains(needle), "log lacks {needle:?} for:\n{src}\n{}", c.log);
}

fn vs_fails(src: &str, needle: &str) {
    fails(Stage::Vertex, src, needle);
}

fn fs_fails(src: &str, needle: &str) {
    fails(Stage::Fragment, src, needle);
}

/// A vertex shader body wrapped in `void main()`.
fn body(stmts: &str) -> String {
    format!("void main() {{ {stmts} }}")
}

fn body3(stmts: &str) -> String {
    format!("#version 300 es\nvoid main() {{ {stmts} }}")
}

/// The folded value of the constant `name`.
fn constant(shader: &Shader, name: &str) -> Vec<Value> {
    let v = shader
        .vars
        .iter()
        .find(|v| shader.name(v.name) == name && v.kind == VarKind::Const)
        .unwrap_or_else(|| panic!("no constant {name}"));
    v.value.clone().expect("a value")
}

fn floats(v: &[Value]) -> Vec<f32> {
    v.iter()
        .map(|x| match x {
            Value::F(f) => *f,
            other => panic!("not a float: {other:?}"),
        })
        .collect()
}

fn close(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-5 * (1.0 + y.abs()))
}

#[test]
fn es2_shaders_compile() {
    vs(r#"
        attribute vec4 position;
        attribute vec3 normal;
        uniform mat4 mvp;
        uniform mat3 normal_matrix;
        varying vec3 v_normal;
        varying vec2 v_uv[2];
        struct Light { vec3 dir; float power; };
        uniform Light light;
        float lambert(vec3 n, Light l) { return max(dot(n, l.dir), 0.0) * l.power; }
        void main() {
            v_normal = normal_matrix * normal;
            v_uv[0] = position.xy;
            v_uv[1] = vec2(lambert(normalize(v_normal), light));
            gl_Position = mvp * position;
            gl_PointSize = 1.0;
        }
    "#);
    fs(r#"
        precision mediump float;
        uniform sampler2D tex;
        uniform samplerCube env;
        varying vec3 v_normal;
        varying vec2 v_uv[2];
        void main() {
            vec4 c = texture2D(tex, v_uv[0]) + textureCube(env, v_normal, 0.5);
            if (c.a < 0.1) discard;
            gl_FragColor = vec4(c.rgb * 0.5, 1.0);
        }
    "#);
}

#[test]
fn es3_shaders_compile() {
    vs(r#"#version 300 es
        layout(location = 0) in vec3 position;
        layout(location = 1) in uvec2 id;
        layout(std140) uniform Camera { mat4 view; mat4 proj; } cam;
        uniform Model { mat4 model; };
        flat out uint v_id;
        out vec3 v_pos;
        void main() {
            v_id = id.x ^ id.y;
            v_pos = (model * vec4(position, 1.0)).xyz;
            gl_Position = cam.proj * cam.view * vec4(v_pos, 1.0);
        }
    "#);
    fs(r#"#version 300 es
        precision highp float;
        precision highp sampler2DArray;
        uniform sampler2DArray layers;
        uniform highp usampler2D ids;
        flat in uint v_id;
        in vec3 v_pos;
        layout(location = 0) out vec4 color;
        layout(location = 1) out uvec4 tag;
        void main() {
            vec4 c = texture(layers, vec3(v_pos.xy, 2.0));
            c += textureLod(layers, v_pos, 1.0);
            c += textureOffset(layers, v_pos, ivec2(1, -1));
            tag = texelFetch(ids, ivec2(gl_FragCoord.xy), 0) + uvec4(v_id);
            color = c * vec4(textureSize(layers, 0), 1.0);
            switch (int(v_id) & 3) { case 0: case 1: color.r = 0.0; break; default: color.g = 1.0; }
        }
    "#);
}

#[test]
fn fragment_shaders_need_a_float_precision() {
    fs_fails("void main() { float x = 1.0; }", "no precision specified for 'float'");
    fs_fails("varying vec2 uv; void main() {}", "no precision specified for 'float'");
    fs("precision lowp float; void main() { float x = 1.0; }");
    fs("void main() { highp float x = 1.0; }");
    // Precision statements are scoped.
    fs_fails("void main() { { precision mediump float; } float x = 1.0; }", "no precision specified");
    // Samplers other than 2D and cube have no default in GLSL ES 3.00.
    fs_fails("#version 300 es\nprecision mediump float;\nuniform sampler3D s; void main() {}", "'sampler3D'");
    vs("#version 300 es\nuniform highp sampler3D s; void main() {}");
}

#[test]
fn there_are_no_implicit_conversions() {
    vs_fails(&body("float x = 1;"), "cannot initialise a 'float' with a 'int'");
    vs_fails(&body("vec2 v = vec2(1.0) + 1;"), "there are no implicit conversions");
    vs_fails(&body3("uint u = 1;"), "cannot initialise a 'uint' with a 'int'");
    vs_fails(&body3("int i = 1u;"), "cannot initialise a 'int' with a 'uint'");
    vs(&body3("uint u = uint(1); int i = int(2u); float f = float(i) + float(u);"));
}

#[test]
fn constant_expressions_fold() {
    let s = vs(r#"
        const float a = 2.0 * 3.0 + 1.0;
        const int n = -7 / 2;
        const vec3 v = normalize(vec3(3.0, 0.0, 4.0));
        const float s = sqrt(16.0) + pow(2.0, 10.0);
        const vec4 w = vec4(vec2(1.0, 2.0), 3.0, 4.0).wzyx;
        const mat2 m = mat2(1.0, 2.0, 3.0, 4.0) * mat2(2.0);
        const vec2 mv = mat2(1.0, 2.0, 3.0, 4.0) * vec2(1.0, 1.0);
        const bool b = all(lessThan(vec2(1.0), vec2(2.0))) && !false;
        const float c = clamp(5.0, 0.0, 1.0) + mix(0.0, 10.0, 0.25) + step(0.5, 0.2);
        const float sm = smoothstep(0.0, 1.0, 0.5);
        void main() {}
    "#);
    assert_eq!(floats(&constant(&s, "a")), [7.0]);
    assert_eq!(constant(&s, "n"), [Value::I(-3)]);
    assert!(close(&floats(&constant(&s, "v")), &[0.6, 0.0, 0.8]));
    assert_eq!(floats(&constant(&s, "s")), [1028.0]);
    assert_eq!(floats(&constant(&s, "w")), [4.0, 3.0, 2.0, 1.0]);
    assert_eq!(floats(&constant(&s, "m")), [2.0, 4.0, 6.0, 8.0]);
    // Columns (1, 2) and (3, 4): m * (1, 1) = (4, 6).
    assert_eq!(floats(&constant(&s, "mv")), [4.0, 6.0]);
    assert_eq!(constant(&s, "b"), [Value::B(true)]);
    assert_eq!(floats(&constant(&s, "c")), [3.5]);
    assert_eq!(floats(&constant(&s, "sm")), [0.5]);
}

#[test]
fn es3_constant_expressions_fold() {
    let s = vs(r#"#version 300 es
        const mat3 m = mat3(2.0, 0.0, 0.0, 0.0, 4.0, 0.0, 1.0, 0.0, 1.0);
        const float d = determinant(m);
        const mat3 i = inverse(m);
        const mat2x3 t = transpose(mat3x2(1.0, 2.0, 3.0, 4.0, 5.0, 6.0));
        const uint bits = floatBitsToUint(1.0);
        const uint packed = packHalf2x16(vec2(1.0, -2.0));
        const vec2 unpacked = unpackHalf2x16(packed);
        const int shifted = (1 << 4) | (0xF0 >> 4) ^ ~0;
        const uint m3 = 17u % 5u;
        const float arr[3] = float[3](1.0, 2.0, 3.0);
        const int len = arr.length();
        const float second = arr[1];
        void main() {}
    "#);
    assert_eq!(floats(&constant(&s, "d")), [8.0]);
    let inv = floats(&constant(&s, "i"));
    assert!(close(&inv, &[0.5, 0.0, 0.0, 0.0, 0.25, 0.0, -0.5, 0.0, 1.0]), "{inv:?}");
    assert_eq!(floats(&constant(&s, "t")), [1.0, 3.0, 5.0, 2.0, 4.0, 6.0]);
    assert_eq!(constant(&s, "bits"), [Value::U(0x3F80_0000)]);
    assert_eq!(constant(&s, "packed"), [Value::U(0xC000_3C00)]);
    assert_eq!(floats(&constant(&s, "unpacked")), [1.0, -2.0]);
    assert_eq!(constant(&s, "shifted"), [Value::I(16 | (0xF ^ -1))]);
    assert_eq!(constant(&s, "m3"), [Value::U(2)]);
    assert_eq!(constant(&s, "len"), [Value::I(3)]);
    assert_eq!(floats(&constant(&s, "second")), [2.0]);
}

#[test]
fn const_needs_a_constant_initialiser() {
    vs_fails("uniform float u; const float x = u; void main() {}", "must be a constant expression");
    vs_fails("float g = sin(1.0); uniform float u; float h = u; void main() {}", "must be a constant expression");
    vs_fails(&body("const float x;"), "needs an initialiser");
}

#[test]
fn operators_type_check() {
    vs(&body("mat3 m; vec3 v = m * vec3(1.0); vec3 w = vec3(1.0) * m; mat4 p = mat4(1.0) * mat4(2.0);"));
    vs(&body("mat2x2 a; float k = 2.0; mat2 b = k * a + a / k;").replace("mat2x2", "mat2"));
    vs_fails(&body("vec3 v = mat4(1.0) * vec3(1.0);"), "cannot apply");
    vs_fails(&body("bool b = vec2(1.0) < vec2(2.0);"), "compare scalars");
    vs_fails(&body("int x = 7 % 2;"), "need GLSL ES 3.00");
    vs_fails(&body("int x = 1 << 2;"), "shifts need GLSL ES 3.00");
    vs(&body3("ivec2 v = ivec2(1) << 2; uint u = 1u << 3; ivec3 w = ivec3(8) >> ivec3(1, 2, 3);"));
    vs_fails(&body3("int x = 1 << ivec2(1);"), "cannot apply");
    vs_fails(&body3("int x = 1 & 1u;"), "same signedness");
    vs_fails(&body("bool b = 1.0 && true;"), "needs bool operands");
    vs_fails(&body("float f = -true;"), "unary '-' needs a number");
    vs_fails(&body("bool b = !vec2(1.0);"), "'!' needs a bool");
}

#[test]
fn constructors_check_their_arguments() {
    vs(&body(
        "vec4 a = vec4(1.0); vec4 b = vec4(vec2(1.0), 2.0, 3.0); vec3 c = vec3(vec4(1.0)); mat2 m = mat2(vec4(1.0));",
    ));
    vs(&body(
        "mat3 m = mat3(mat4(1.0)); mat4 n = mat4(mat2(2.0)); bvec2 b = bvec2(1.0, 0); ivec3 i = ivec3(vec3(1.5));",
    ));
    vs_fails(&body("vec2 v = vec2(1.0, 2.0, 3.0);"), "too many arguments");
    vs_fails(&body("vec3 v = vec3(1.0, 2.0);"), "not enough values");
    vs_fails(&body("mat2 m = mat2(mat2(1.0), 1.0);"), "must be the only argument");
    vs_fails(&body("vec4 v = vec4();"), "needs arguments");
    vs(&body("struct S { float a; vec2 b; }; S s = S(1.0, vec2(2.0));"));
    vs_fails(&body("struct S { float a; vec2 b; }; S s = S(1.0);"), "has 2 members, 1 values given");
    vs_fails(&body("struct S { float a; }; S s = S(1);"), "cannot take a 'int'");
}

#[test]
fn arrays() {
    vs(&body("float a[4]; a[3] = 1.0; const int n = 2; float b[n + 1];"));
    vs_fails(&body("float a[4]; a[4] = 1.0;"), "index 4 is out of range");
    vs_fails(&body("float a[0];"), "greater than zero");
    vs_fails("uniform int n; void main() { float a[n]; }", "constant expression is required");
    vs_fails(&body("float a[2]; float b[2]; a = b;"), "arrays cannot be assigned in GLSL ES 1.00");
    vs(&body3("float a[2]; float b[2] = float[](1.0, 2.0); a = b; int n = a.length(); float c[] = a;"));
    vs_fails(&body3("float a[2] = float[3](1.0, 2.0, 3.0);"), "cannot initialise");
    vs_fails(&body3("float a[] ;"), "needs an initialiser");
}

#[test]
fn swizzles() {
    vs(&body("vec4 v; v.xy = vec2(1.0); v.zw = v.xy; float f = v.w; vec3 c = v.rgb; vec2 t = v.st; v.yx = v.xx;"));
    vs_fails(&body("vec4 v; v.xx = vec2(1.0);"), "repeated components");
    vs_fails(&body("vec2 v; float f = v.z;"), "invalid swizzle");
    vs_fails(&body("vec4 v; vec2 m = v.xg;"), "invalid swizzle");
    vs_fails(&body("float f; float g = f.x;"), "has no member 'x'");
}

#[test]
fn l_values() {
    vs_fails("uniform float u; void main() { u = 1.0; }", "'u' is a uniform");
    vs_fails("attribute vec4 a; void main() { a = vec4(1.0); }", "'a' is a shader input");
    vs_fails(&body("const float c = 1.0; c = 2.0;"), "a constant cannot be assigned");
    fs_fails("precision mediump float; void main() { gl_FragCoord = vec4(1.0); }", "is a shader input");
    vs_fails(&body("float f; (f + 1.0) = 2.0;"), "cannot be assigned");
    vs_fails("void f(const in float x) { x = 1.0; } void main() { f(1.0); }", "'x' is read-only");
    vs_fails("void f(out float x) { x = 1.0; } void main() { f(1.0); }", "a constant cannot be assigned");
    vs("void f(out float x, inout vec2 y) { x = y.x; y = y.yx; } void main() { float a; vec2 b; f(a, b); }");
}

#[test]
fn functions() {
    vs("float f(float x); float f(float x) { return x; } int f(int x) { return x; } void main() { f(1.0); f(1); }");
    vs_fails("void main() {} void main() {}", "already defined");
    vs_fails("int main() { return 0; }", "must be declared 'void main()'");
    vs_fails("void f() {}", "'main' is not defined");
    vs_fails("void f(); void main() { f(); }", "called but never defined");
    vs_fails("float f(float x) { return f(x); } void main() {}", "recursion is not allowed");
    vs_fails("void a(); void b() { a(); } void a() { b(); } void main() { a(); }", "recursion is not allowed");
    vs_fails("float f() { return 1; } void main() {}", "returning a 'int'");
    vs_fails("void f() { return 1.0; } void main() {}", "cannot return a value");
    vs_fails(&body("float x = g(1.0);"), "no function named 'g'");
    vs_fails("float f(float x) { return x; } void main() { f(1); }", "no matching overload for 'f(int)'");
    vs_fails("float f(float x); int f(float x) { return 1; } void main() {}", "different return type");
    vs_fails(&body("void g();"), "only be declared at global scope");
}

#[test]
fn built_ins_can_be_overloaded_in_es2_only() {
    vs("int sin(int x) { return x; } void main() { float a = sin(1.0); int b = sin(1); }");
    vs_fails("float sin(float x) { return x; } void main() {}", "cannot be redeclared or overloaded");
    vs_fails("#version 300 es\nint sin(int x) { return x; } void main() {}", "cannot be redeclared or overloaded");
}

#[test]
fn statements() {
    vs_fails(&body("break;"), "'break' outside a loop");
    vs_fails(&body("continue;"), "'continue' outside a loop");
    vs_fails(&body("discard;"), "only allowed in fragment shaders");
    vs_fails(&body("if (1.0) {}"), "a condition must be a bool");
    vs(&body(
        "for (int i = 0; i < 4; i++) { if (i == 2) break; else continue; } while (false) {} do {} while (false);",
    ));
    // A for loop's header and body share a scope.
    vs_fails(&body("for (int i = 0; i < 2; i++) { int i = 0; }"), "'i' redeclared");
    vs(&body("for (int i = 0; i < 2; i++) { { int i = 0; } }"));
    vs(&body("int i = 0; while (bool b = i < 3) { i++; }"));
}

#[test]
fn switch_statements() {
    vs(&body3("int x = 1; switch (x) { case 0: x = 1; case 1: case 2: { x = 2; break; } default: x = 3; }"));
    vs(&body3("uint x = 1u; switch (x) { case 0u: break; }"));
    vs_fails(&body3("int x; switch (x) { case 0: break; case 0: break; }"), "duplicate case label");
    vs_fails(&body3("int x; switch (x) { default: break; default: break; }"), "more than one default");
    vs_fails(&body3("int x; switch (x) { x = 1; case 0: break; }"), "must follow a case or default");
    vs_fails(&body3("int x; switch (x) { case 0: break; case 1: }"), "followed by a statement");
    vs_fails(&body3("int x; switch (x) { case 0u: break; }"), "the type of the switch");
    vs_fails(&body3("int x; int y; switch (x) { case y: break; }"), "must be a constant expression");
    vs_fails(&body3("float x; switch (x) { case 0: break; }"), "must be an integer scalar");
    vs_fails(&body3("int x; switch (x) { case 0: { case 1: break; } }"), "only appear directly in a switch");
    vs_fails(&body("int x; switch (x) { case 0: break; }"), "reserved word");
}

#[test]
fn qualifiers() {
    fs_fails("attribute vec4 a; void main() {}", "'attribute' is only allowed in vertex shaders");
    vs_fails("struct S { float x; }; varying S v; void main() {}", "cannot be structures");
    vs_fails("varying bool b; void main() {}", "varyings can only be float");
    vs_fails("attribute vec4 a[2]; void main() {}", "attributes can only be float");
    vs_fails("#version 300 es\nout int i; void main() {}", "must be qualified 'flat'");
    vs_fails("#version 300 es\nin bool b; void main() {}", "cannot be bool");
    fs_fails(
        "#version 300 es\nprecision mediump float;\nlayout(location = 0) in vec4 v; void main() {}",
        "'location' only applies",
    );
    fs_fails(
        "#version 300 es\nprecision mediump float;\nout mat2 m; void main() {}",
        "fragment shader outputs can only be",
    );
    vs_fails("#version 300 es\nin flat vec4 v; void main() {}", "wrong order");
    vs_fails("#version 300 es\nlayout(foo) in vec4 v; void main() {}", "unknown layout qualifier 'foo'");
    vs_fails("uniform float u = 1.0; void main() {}", "uniforms cannot have initialisers");
    vs_fails(&body("uniform float u;"), "local variables can only be qualified 'const'");
    vs("invariant varying vec4 v; varying vec4 w; invariant w; invariant gl_Position; void main() {}");
    vs_fails("uniform vec4 u; invariant u; void main() {}", "cannot be made invariant");
    fs_fails(
        "#version 300 es\nprecision mediump float;\nout vec4 a; out vec4 b; void main() {}",
        "needs a layout location",
    );
}

#[test]
fn texture_functions_follow_version_and_stage() {
    fs_fails(
        "precision mediump float; uniform sampler2D s; void main() { gl_FragColor = texture2DLod(s, vec2(0.0), 0.0); }",
        "only available in vertex shaders",
    );
    vs("uniform sampler2D s; void main() { gl_Position = texture2DLod(s, vec2(0.0), 0.0); }");
    vs_fails(
        "uniform sampler2D s; void main() { gl_Position = texture2D(s, vec2(0.0), 1.0); }",
        "bias is only available in fragment shaders",
    );
    vs_fails(
        "uniform sampler2D s; void main() { gl_Position = texture(s, vec2(0.0)); }",
        "no function named 'texture'",
    );
    vs_fails(
        "#version 300 es\nuniform sampler2D s; void main() { gl_Position = texture2D(s, vec2(0.0)); }",
        "no function named 'texture2D'",
    );
    fs(
        "#extension GL_EXT_shader_texture_lod : enable\nprecision mediump float; uniform sampler2D s; void main() { gl_FragColor = texture2DLodEXT(s, vec2(0.0), 0.0); }",
    );
    vs_fails(
        "#version 300 es\nuniform sampler2D s; void main() { gl_Position = textureOffset(s, vec2(0.0), ivec2(9, 0)); }",
        "between -8 and 7",
    );
    vs_fails(
        "#version 300 es\nuniform sampler2D s; uniform ivec2 o; void main() { gl_Position = textureOffset(s, vec2(0.0), o); }",
        "must be a constant expression",
    );
    vs_fails(
        "#version 300 es\nuniform samplerCube s; void main() { gl_Position = texelFetch(s, ivec2(0), 0); }",
        "no matching overload",
    );
}

#[test]
fn derivatives_need_the_extension_in_es2() {
    let src = "precision mediump float; varying vec2 v; void main() { gl_FragColor = vec4(dFdx(v), fwidth(v)); }";
    fs_fails(src, "needs the GL_OES_standard_derivatives extension");
    fs(&format!("#extension GL_OES_standard_derivatives : enable\n{src}"));
    vs_fails(
        "#version 300 es\nin vec2 v; void main() { gl_Position = vec4(dFdx(v), 0.0, 0.0); }",
        "only available in fragment shaders",
    );
}

#[test]
fn es2_fragment_outputs_are_exclusive() {
    fs_fails(
        "precision mediump float; void main() { gl_FragColor = vec4(1.0); gl_FragData[0] = vec4(1.0); }",
        "both gl_FragColor and gl_FragData",
    );
}

#[test]
fn uniform_blocks() {
    vs(
        "#version 300 es\nuniform B { vec4 a; float b[2]; } blk; uniform C { mat4 m; }; void main() { gl_Position = blk.a + m[0] + vec4(blk.b[1]); }",
    );
    vs("#version 300 es\nuniform L { vec4 color; } lights[3]; void main() { gl_Position = lights[2].color; }");
    vs_fails("#version 300 es\nuniform B { vec4 a; } x; uniform B { vec4 c; } y; void main() {}", "declared twice");
    vs_fails("#version 300 es\nuniform B { sampler2D s; }; void main() {}", "cannot be void or samplers");
    vs_fails("#version 300 es\nuniform B { vec4 a; } blk; void main() { blk.a = vec4(1.0); }", "is a uniform");
    vs_fails("uniform B { vec4 a; }; void main() {}", "need GLSL ES 3.00");
}

#[test]
fn structures() {
    vs("struct S { float x; vec2 v[2]; }; struct T { S s; mat2 m; }; void main() { T t; t.s.v[1].y = t.m[1][0]; }");
    vs_fails("struct S { struct T { float y; } t; }; void main() {}", "cannot be nested");
    vs_fails("struct S { float x; float x; }; void main() {}", "duplicate structure member");
    vs_fails("struct S { float x; }; struct S { float y; }; void main() {}", "'S' redeclared");
    vs_fails(&body("struct S { float x; }; S s; s.y = 1.0;"), "has no member 'y'");
    vs_fails("struct S { sampler2D t; }; S s; void main() {}", "samplers can only be uniforms");
    vs(
        "struct S { sampler2D t; float k; }; uniform S s; void main() { gl_Position = texture2DLod(s.t, vec2(s.k), 0.0); }",
    );
}

#[test]
fn names_and_scopes() {
    vs_fails(&body("float x; float x;"), "'x' redeclared");
    vs(&body("float x; { float x; }"));
    vs_fails("float gl_Thing; void main() {}", "reserved");
    vs_fails(&body("y = 1.0;"), "'y' is not declared");
    vs_fails("void main() { float f = 1.0; f(); }", "'f' is not a function");
    vs(&body("float sin = 1.0;"));
    vs_fails("float sin; void main() {}", "name of a built-in function");
}

#[test]
fn built_in_constants() {
    let s = vs("const int n = gl_MaxVertexAttribs; void main() {}");
    assert_eq!(constant(&s, "n"), [Value::I(16)]);
    let s = vs("#version 300 es\nconst int n = gl_MaxProgramTexelOffset; void main() {}");
    assert_eq!(constant(&s, "n"), [Value::I(7)]);
}

#[test]
fn malformed_shaders_never_panic() {
    let pieces = [
        "void main() {",
        "}",
        "float",
        "x",
        "=",
        ";",
        "(",
        ")",
        "vec4(",
        "1.0",
        ",",
        "[",
        "]",
        ".",
        "xyz",
        "struct",
        "S",
        "{",
        "uniform",
        "if",
        "else",
        "for",
        "while",
        "return",
        "discard",
        "int",
        "+",
        "*",
        "?",
        ":",
        "texture2D",
        "#version 300 es\n",
        "out",
        "in",
        "layout(location=0)",
        "switch",
        "case",
        "default",
        "mat4",
        "sampler2D",
    ];
    // A deterministic walk through many combinations.
    let mut seed = 0x1234_5678u32;
    for _ in 0..3000 {
        let mut src = String::new();
        for _ in 0..(seed % 24) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            src.push_str(pieces[(seed as usize) % pieces.len()]);
            src.push(' ');
        }
        let _ = compile(Stage::Vertex, &[&src], &Options::default());
        let _ = compile(Stage::Fragment, &[&src], &Options::default());
    }
}
