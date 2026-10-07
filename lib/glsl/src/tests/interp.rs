//! The interpreter, end to end: GLSL compiled, linked, lowered to SSA,
//! compiled to bytecode and run on sixteen lanes whose inputs differ, so
//! that every branch and loop diverges. Expected values come from plain
//! Rust, or from the constant folder (both must agree bit for bit: they
//! share [`crate::ops`]).

use std::format;
use std::vec::Vec;

use super::ir::program;
use crate::interp::{self, Env, Exec, Input, LANES, Lanes, NoTextures, Output};
use crate::ops::Value;

const FS: &str = "#version 300 es\nprecision highp float; out vec4 c; void main() { c = vec4(1.0); }";

/// A vertex shader reading `in float x` (location 0) and writing
/// `out vec4 r`, run on lanes where x = `xs[lane]`; returns r's
/// components per lane, as bits.
fn run_with(body: &str, xs: [f32; LANES], uniforms: &[(&str, &[u32])]) -> [[u32; LANES]; 4] {
    let vs = format!("#version 300 es\nlayout(location = 0) in float x;\nout vec4 r;\n{body}");
    let p = program(&vs, FS);
    let code = interp::compile(&p.vertex);
    let mut storage = std::vec![[0u32; 4]; p.linked.slots as usize];
    for (name, values) in uniforms {
        let u = p.linked.uniforms.iter().find(|u| u.name == *name).unwrap_or_else(|| panic!("no uniform {name}"));
        for (k, v) in values.iter().enumerate() {
            storage[u.slot as usize + k / 4][k % 4] = *v;
        }
    }
    let env = Env { uniforms: &storage, blocks: &[], textures: &NoTextures };
    let mut e = Exec::new(&code);
    e.prologue(&code, &env);
    if let Some(r) = code.input(Input::Value { slot: 0, comp: 0 }) {
        e.regs[r as usize] = Lanes::from_f32(xs);
    }
    e.run(&code, interp::ALL, &env);
    assert!(!e.runaway, "a loop ran away");
    let mut out = [[0u32; LANES]; 4];
    for (c, o) in out.iter_mut().enumerate() {
        if let Some(r) = code.output(Output::Value { slot: 0, comp: c as u8 }) {
            *o = e.regs[r as usize].0;
        }
    }
    out
}

fn lanes() -> [f32; LANES] {
    core::array::from_fn(|i| i as f32)
}

fn run(body: &str) -> [[u32; LANES]; 4] {
    run_with(body, lanes(), &[])
}

fn fx(out: &[[u32; LANES]; 4], c: usize) -> [f32; LANES] {
    out[c].map(f32::from_bits)
}

#[test]
fn arithmetic_on_every_lane() {
    let o = run("void main() { r = vec4(x * 2.0 + 1.0, x * x, -x, 1.0 / (x + 1.0)); }");
    for i in 0..LANES {
        let x = i as f32;
        assert_eq!(fx(&o, 0)[i], x * 2.0 + 1.0);
        assert_eq!(fx(&o, 1)[i], x * x);
        assert_eq!(fx(&o, 2)[i], -x);
        assert_eq!(fx(&o, 3)[i], 1.0 / (x + 1.0));
    }
}

#[test]
fn branches_diverge() {
    let o = run(r#"void main() {
            float a;
            if (x > 7.5) { a = x; } else { a = -x; }
            float b = 0.0;
            if (mod(x, 3.0) == 0.0) b = 1.0; else if (mod(x, 3.0) == 1.0) b = 2.0;
            r = vec4(a, b, 0.0, 0.0);
        }"#);
    for i in 0..LANES {
        let x = i as f32;
        assert_eq!(fx(&o, 0)[i], if x > 7.5 { x } else { -x });
        let m = x % 3.0;
        assert_eq!(
            fx(&o, 1)[i],
            if m == 0.0 {
                1.0
            } else if m == 1.0 {
                2.0
            } else {
                0.0
            }
        );
    }
}

#[test]
fn loops_with_per_lane_trip_counts() {
    let o = run(r#"void main() {
            float sum = 0.0;
            int n = int(x);
            for (int i = 0; i < n; i++) sum += float(i);
            float fact = 1.0;
            int k = n;
            while (k > 1) { fact *= float(k); k--; }
            int steps = 0;
            int v = n + 1;
            do { v = (v % 2 == 0) ? v / 2 : 3 * v + 1; steps++; } while (v != 1 && steps < 100);
            r = vec4(sum, fact, float(steps), 0.0);
        }"#);
    for i in 0..LANES {
        let n = i as i64;
        assert_eq!(fx(&o, 0)[i], (n * (n - 1) / 2).max(0) as f32, "sum, lane {i}");
        let fact: f32 = (2..=n.max(1)).map(|k| k as f32).product();
        assert_eq!(fx(&o, 1)[i], fact, "factorial, lane {i}");
        let (mut v, mut steps) = (n + 1, 0);
        loop {
            v = if v % 2 == 0 { v / 2 } else { 3 * v + 1 };
            steps += 1;
            if v == 1 || steps >= 100 {
                break;
            }
        }
        assert_eq!(fx(&o, 2)[i], steps as f32, "collatz, lane {i}");
    }
}

#[test]
fn break_and_continue_diverge() {
    let o = run(r#"void main() {
            int n = int(x);
            float odd = 0.0;
            float first_big = -1.0;
            for (int i = 0; i < 20; i++) {
                if (i > n) break;
                if (i % 2 == 0) continue;
                odd += float(i);
            }
            for (int i = 0; i < 20; i++) {
                float sq = float(i * i);
                if (sq > x * 3.0) { first_big = sq; break; }
            }
            r = vec4(odd, first_big, 0.0, 0.0);
        }"#);
    for i in 0..LANES {
        let n = i as i32;
        let odd: i32 = (0..20).take_while(|&k| k <= n).filter(|k| k % 2 == 1).sum();
        assert_eq!(fx(&o, 0)[i], odd as f32, "odd sum, lane {i}");
        let big = (0..20).map(|k| (k * k) as f32).find(|&sq| sq > i as f32 * 3.0).unwrap_or(-1.0);
        assert_eq!(fx(&o, 1)[i], big, "first big square, lane {i}");
    }
}

#[test]
fn early_returns_from_loops() {
    let o = run(r#"
        float find(int target) {
            for (int i = 0; i < 6; i++) {
                for (int j = 0; j < 6; j++) {
                    if (i * j == target) return float(i * 10 + j);
                }
            }
            return -1.0;
        }
        float sgn(float v) { if (v > 8.0) return 1.0; if (v < 4.0) return -1.0; return 0.0; }
        void main() { r = vec4(find(int(x)), sgn(x), 0.0, 0.0); }"#);
    for i in 0..LANES {
        let t = i as i32;
        let mut found = -1.0;
        'outer: for a in 0..6 {
            for b in 0..6 {
                if a * b == t {
                    found = (a * 10 + b) as f32;
                    break 'outer;
                }
            }
        }
        assert_eq!(fx(&o, 0)[i], found, "find, lane {i}");
        let x = i as f32;
        assert_eq!(
            fx(&o, 1)[i],
            if x > 8.0 {
                1.0
            } else if x < 4.0 {
                -1.0
            } else {
                0.0
            }
        );
    }
}

#[test]
fn switch_falls_through_per_lane() {
    let o = run(r#"void main() {
            float acc = 0.0;
            switch (int(x) % 6) {
                case 0: acc += 1.0;
                case 1: acc += 2.0; break;
                case 2: acc = 10.0;
                default: acc += 100.0;
                case 4: acc += 1000.0;
            }
            float loops = 0.0;
            for (int i = 0; i < 4; i++) {
                switch ((int(x) + i) % 3) {
                    case 0: continue;
                    case 1: loops += 1.0; break;
                    default: loops += 10.0;
                }
                loops += 0.5;
            }
            r = vec4(acc, loops, 0.0, 0.0);
        }"#);
    for i in 0..LANES {
        let acc = match i % 6 {
            0 => 3.0,
            1 => 2.0,
            2 => 1110.0,
            4 => 1000.0,
            _ => 1100.0,
        };
        assert_eq!(fx(&o, 0)[i], acc, "switch, lane {i}");
        let mut loops = 0.0;
        for k in 0..4 {
            match (i + k) % 3 {
                0 => continue,
                1 => loops += 1.0,
                _ => loops += 10.0,
            }
            loops += 0.5;
        }
        assert_eq!(fx(&o, 1)[i], loops, "switch in loop, lane {i}");
    }
}

#[test]
fn dynamic_indexing() {
    let table: Vec<u32> = (0..8).flat_map(|k| [(k as f32 * 1.5).to_bits(), 0, 0, 0]).collect();
    let o = run_with(
        r#"
        uniform vec4 table[8];
        void main() {
            int i = int(x);
            float local[5];
            for (int k = 0; k < 5; k++) local[k] = float(k * k);
            local[i % 5] += 100.0;
            r = vec4(table[i % 8].x, local[(i + 2) % 5], local[i % 5], table[i].x);
        }"#,
        lanes(),
        &[("table[0]", &table)],
    );
    for i in 0..LANES {
        assert_eq!(fx(&o, 0)[i], (i % 8) as f32 * 1.5, "uniform, lane {i}");
        let mut local: Vec<f32> = (0..5).map(|k| (k * k) as f32).collect();
        local[i % 5] += 100.0;
        assert_eq!(fx(&o, 1)[i], local[(i + 2) % 5], "local, lane {i}");
        assert_eq!(fx(&o, 2)[i], local[i % 5], "written local, lane {i}");
        // Out of range: clamped to the last element, never outside.
        assert_eq!(fx(&o, 3)[i], (i.min(7)) as f32 * 1.5, "clamped, lane {i}");
    }
}

#[test]
fn out_and_inout_parameters() {
    let o = run(r#"
        void split(float v, out float whole, out float part) { whole = floor(v / 2.0); part = v - whole * 2.0; }
        void bump(inout float a, inout int count) { a += 1.0; count++; }
        void main() {
            float w, p;
            split(x, w, p);
            int c = 0;
            float a[3];
            a[0] = 0.0; a[1] = 0.0; a[2] = 0.0;
            int i = 0;
            bump(a[int(x) % 3], c);
            bump(a[int(x) % 3], c);
            r = vec4(w, p, a[int(x) % 3], float(c));
        }"#);
    for i in 0..LANES {
        let x = i as f32;
        assert_eq!(fx(&o, 0)[i], (x / 2.0).floor());
        assert_eq!(fx(&o, 1)[i], x - (x / 2.0).floor() * 2.0);
        assert_eq!(fx(&o, 2)[i], 2.0);
        assert_eq!(fx(&o, 3)[i], 2.0);
    }
}

/// Built-in functions agree with the constant folder: the same expression
/// on a uniform input is computed at run time and folded at compile time.
#[test]
fn run_time_matches_constant_folding() {
    let exprs = [
        "sin(v) + cos(v * 0.5)",
        "pow(abs(v) + 0.5, 1.7)",
        "exp2(v * 0.25) - log2(abs(v) + 1.0)",
        "inversesqrt(abs(v) + 0.25)",
        "atan(v, 2.0) + atan(v) + asin(clamp(v * 0.1, -1.0, 1.0))",
        "smoothstep(0.0, 10.0, v) + step(3.0, v) + mix(1.0, 7.0, fract(v * 0.3))",
        "mod(v, 3.5) + floor(v * 0.7) + ceil(v * 0.3) + round(v * 0.5) + trunc(-v * 0.3)",
        "length(vec3(v, 1.0, 2.0)) + distance(vec2(v), vec2(1.0, -3.0))",
        "dot(normalize(vec3(v, 1.0, -2.0)), vec3(0.2, 0.3, 0.4))",
        "reflect(vec3(v, 1.0, 0.0), normalize(vec3(0.0, 1.0, 1.0))).x",
        "refract(normalize(vec3(v, -1.0, 0.0)), vec3(0.0, 1.0, 0.0), 0.66).y",
        "determinant(mat3(v, 1.0, 2.0, 0.0, v, 1.0, 3.0, 0.0, 1.0))",
        "inverse(mat2(v + 1.0, 2.0, 3.0, 4.0))[1][0]",
        "(mat3(v) * vec3(1.0, 2.0, 3.0)).z + (vec2(v, 1.0) * mat2(1.0, 2.0, 3.0, 4.0)).y",
        "float(int(v * 3.7) / 2 + int(v) % 3 + (int(v) << 2) + (int(v) >> 1))",
        "float(uint(v * 2.0) * 3u / 5u + uint(v) % 7u)",
        "uintBitsToFloat(floatBitsToUint(v) ^ 0x80000000u)",
        "unpackHalf2x16(packHalf2x16(vec2(v * 0.37, -v))).x + unpackHalf2x16(packHalf2x16(vec2(v, 65504.0))).y",
        "unpackSnorm2x16(packSnorm2x16(vec2(v * 0.1 - 0.7, 0.25))).x + unpackUnorm2x16(packUnorm2x16(vec2(v * 0.07, 0.5))).x",
        "sinh(v * 0.1) + cosh(v * 0.1) + tanh(v * 0.3) + asinh(v) + acosh(v + 1.0) + atanh(v * 0.05)",
        "float(isnan(v / 0.0 - v / 0.0)) + float(isinf(1.0 / (v - v)))",
        "max(v, 3.0) - min(v, 2.0) + clamp(v, 1.0, 4.0)",
        "faceforward(vec2(1.0, v), vec2(v, -1.0), vec2(0.5, 0.5)).y",
        "outerProduct(vec2(v, 2.0), vec3(1.0, v, 3.0))[1][1]",
        "radians(v * 10.0) + degrees(v * 0.1)",
        "float(all(greaterThan(vec3(v), vec3(1.0, 2.0, 3.0)))) + float(any(equal(ivec2(int(v)), ivec2(3, 9))))",
    ];
    for e in exprs {
        for v in [0.0f32, 1.0, 2.5, -3.25, 7.75, 100.0] {
            // Folded at compile time...
            let folded = run_with(&format!("void main() {{ const float v = {v:?}; r = vec4({e}); }}"), lanes(), &[]);
            // ...and computed at run time from a uniform...
            let uniform = run_with(
                &format!("uniform float u; void main() {{ float v = u; r = vec4({e}); }}"),
                lanes(),
                &[("u", &[v.to_bits()])],
            );
            // ...and from an input (no prologue).
            let input = run_with(&format!("void main() {{ float v = x; r = vec4({e}); }}"), [v; LANES], &[]);
            let k = folded[0][0];
            for lane in 0..LANES {
                let same = |a: u32, b: u32| a == b || (f32::from_bits(a).is_nan() && f32::from_bits(b).is_nan());
                assert!(
                    same(uniform[0][lane], k),
                    "{e} at {v}: uniform {:?} vs folded {:?}",
                    f32::from_bits(uniform[0][lane]),
                    f32::from_bits(k)
                );
                assert!(
                    same(input[0][lane], k),
                    "{e} at {v}: input {:?} vs folded {:?}",
                    f32::from_bits(input[0][lane]),
                    f32::from_bits(k)
                );
            }
        }
    }
}

#[test]
fn every_operation_matches_its_definition() {
    use crate::ops::{self, Op, Ty};
    // Lanes of interesting values, as bits.
    let specials: [u32; LANES] = [
        0,
        0x8000_0000,
        1.0f32.to_bits(),
        (-1.5f32).to_bits(),
        0x7F80_0000,
        0xFF80_0000,
        0x7FC0_0000,
        1,
        0xFFFF_FFFF,
        0x7FFF_FFFF,
        0x8000_0000,
        3.75f32.to_bits(),
        1e-40f32.to_bits(),
        65504.0f32.to_bits(),
        17,
        0x4000_0000,
    ];
    let mut rot = specials;
    rot.rotate_left(5);
    let ops_list: Vec<(Op, Ty)> = [
        Op::FNeg,
        Op::FAbs,
        Op::FSign,
        Op::FFloor,
        Op::FCeil,
        Op::FTrunc,
        Op::FRoundEven,
        Op::FFract,
        Op::FSqrt,
        Op::FRsq,
        Op::FExp,
        Op::FLog,
        Op::FExp2,
        Op::FLog2,
        Op::FSin,
        Op::FCos,
        Op::FTan,
        Op::FAsin,
        Op::FAcos,
        Op::FAtan,
        Op::FSinh,
        Op::FCosh,
        Op::FTanh,
        Op::FAsinh,
        Op::FAcosh,
        Op::FAtanh,
        Op::FAdd,
        Op::FSub,
        Op::FMul,
        Op::FDiv,
        Op::FMin,
        Op::FMax,
        Op::FPow,
        Op::FAtan2,
        Op::FLt,
        Op::FLe,
        Op::FEq,
        Op::FNe,
        Op::FIsNan,
        Op::FIsInf,
        Op::FToI,
        Op::FToU,
        Op::FToB,
        Op::FToHalf,
    ]
    .into_iter()
    .map(|o| (o, o.signature().ret))
    .chain(
        [
            Op::INeg,
            Op::INot,
            Op::IAbs,
            Op::ISign,
            Op::IAdd,
            Op::ISub,
            Op::IMul,
            Op::IAnd,
            Op::IOr,
            Op::IXor,
            Op::IShl,
            Op::IDiv,
            Op::UDiv,
            Op::IRem,
            Op::URem,
            Op::IShr,
            Op::UShr,
            Op::IMin,
            Op::UMin,
            Op::IMax,
            Op::UMax,
            Op::IEq,
            Op::INe,
            Op::ILt,
            Op::ULt,
            Op::ILe,
            Op::ULe,
            Op::IToF,
            Op::UToF,
            Op::IToB,
            Op::HalfToF,
        ]
        .into_iter()
        .map(|o| (o, if o.signature().ret == Ty::Bool { Ty::Bool } else { Ty::I32 })),
    )
    .chain(
        [Op::BNot, Op::BAnd, Op::BOr, Op::BXor, Op::BEq, Op::BToF, Op::BToI]
            .into_iter()
            .map(|o| (o, o.signature().ret)),
    )
    .collect();
    for (op, ret) in ops_list {
        let sig = op.signature();
        let to = |bits: u32, t: Ty| {
            let t = if t == Ty::I32 && ret == Ty::U32 { Ty::U32 } else { t };
            match t {
                Ty::Bool => Value::B(bits != 0),
                t => Value::from_bits(t, bits),
            }
        };
        let a = if sig.args[0] == Ty::Bool { specials.map(|x| if x & 1 == 1 { !0 } else { 0 }) } else { specials };
        let c = if sig.args.get(1) == Some(&Ty::Bool) { rot.map(|x| if x & 2 == 2 { !0 } else { 0 }) } else { rot };
        let got = crate::interp::exec_apply_for_tests(op, ret, &a, &c);
        for i in 0..LANES {
            let args: Vec<Value> =
                sig.args.iter().enumerate().map(|(k, &t)| to(if k == 0 { a[i] } else { c[i] }, t)).collect();
            let want = ops::eval(op, &args, ret).bits();
            assert_eq!(got[i], want, "{op:?} on {:#x}, {:#x}", a[i], c[i]);
        }
    }
}

#[test]
fn half_floats_round_trip_and_round_to_even() {
    use crate::ops::{f16_to_f32, f32_to_f16};
    for h in 0..=u16::MAX {
        let f = f16_to_f32(h);
        if f.is_nan() {
            assert!(f16_to_f32(f32_to_f16(f)).is_nan());
            continue;
        }
        assert_eq!(f32_to_f16(f), h, "half {h:#06x} -> {f} -> back");
    }
    // Halfway between 1.0 and the next half (1 + 2^-10): ties to even.
    assert_eq!(f32_to_f16(1.0 + 2f32.powi(-11)), 0x3C00);
    assert_eq!(f32_to_f16(1.0 + 3.0 * 2f32.powi(-11)), 0x3C02);
    assert_eq!(f32_to_f16(70000.0), 0x7C00);
    assert_eq!(f32_to_f16(-1e-10), 0x8000);
    assert_eq!(f32_to_f16(2f32.powi(-24)), 0x0001);
}

#[test]
fn loops_cannot_run_away() {
    let vs = "#version 300 es\nlayout(location = 0) in float x;\nout vec4 r;\nvoid main() { float a = 0.0; while (x >= 0.0) { a += 1.0; } r = vec4(a); }";
    let p = program(vs, FS);
    let code = interp::compile(&p.vertex);
    let env = Env { uniforms: &[], blocks: &[], textures: &NoTextures };
    let mut e = Exec::new(&code);
    e.prologue(&code, &env);
    if let Some(r) = code.input(Input::Value { slot: 0, comp: 0 }) {
        e.regs[r as usize] = Lanes::from_f32([1.0; LANES]);
    }
    e.run(&code, interp::ALL, &env);
    assert!(e.runaway);
}
