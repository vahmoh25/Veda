//! Accuracy tests for [`crate::f32`] and [`crate::f64`] against the host `std`
//! (on Windows: the MSVC universal CRT; on Linux: glibc).
//!
//! `f64` results are compared with `std` directly. `f32` results are compared
//! with `std`'s *`f64`* function evaluated on the widened argument and rounded
//! to `f32`, which is a (nearly) correctly rounded reference. Errors are
//! measured in ulps of the result type. Run
//! `cargo test -p vmath libm -- --nocapture` to print the per-function tables;
//! `cargo test -p vmath --release exhaustive -- --ignored --nocapture` checks
//! single-argument `f32` functions on all 2^32 inputs.

use crate::Rng;
use std::format;
use std::println;
use std::string::String;
use std::vec::Vec;

/// Whether the host's C runtime, the reference, is glibc, whose `log10`
/// is less accurate than the universal CRT's.
const GLIBC: bool = cfg!(all(target_os = "linux", target_env = "gnu"));

// ---------------------------------------------------------------------------
// ulp distances
// ---------------------------------------------------------------------------

/// Maps floats to integers so that adjacent floats map to adjacent integers.
fn ord64(x: f64) -> i64 {
    let b = x.to_bits() as i64;
    if b < 0 { i64::MIN - b } else { b }
}

fn ord32(x: f32) -> i32 {
    let b = x.to_bits() as i32;
    if b < 0 { i32::MIN - b } else { b }
}

/// Distance in ulps; NaN equals NaN, a NaN mismatch is `u64::MAX`.
pub(crate) fn ulps64(a: f64, b: f64) -> u64 {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => 0,
        (false, false) => ord64(a).abs_diff(ord64(b)),
        _ => u64::MAX,
    }
}

pub(crate) fn ulps32(a: f32, b: f32) -> u64 {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => 0,
        (false, false) => ord32(a).abs_diff(ord32(b)) as u64,
        _ => u64::MAX,
    }
}

/// Exact agreement including the sign of zero (NaN payloads are ignored).
fn same64(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

fn same32(a: f32, b: f32) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

/// Special results must agree exactly: NaN-ness, infinities, and the sign of
/// zero when both results are zero. (Finite results, including a tiny
/// subnormal versus zero, are judged by the ulp bound.)
fn special_mismatch(a: f64, b: f64) -> bool {
    if a.is_nan() || b.is_nan() {
        return a.is_nan() != b.is_nan();
    }
    if a.is_infinite() || b.is_infinite() {
        return a != b;
    }
    a == 0.0 && b == 0.0 && a.to_bits() != b.to_bits()
}

/// Rust does not specify signaling-NaN behaviour, so random inputs use quiet NaNs.
fn quiet64(x: f64) -> f64 {
    if x.is_nan() { f64::NAN } else { x }
}

fn quiet32(x: f32) -> f32 {
    if x.is_nan() { f32::NAN } else { x }
}

/// Reference implementations where `std`'s lose accuracy: functions it
/// writes in Rust with formulas that do (e.g. `atanh` near ±1), and hard
/// cases that a C runtime gets wrong.
mod reference {
    /// The double closest to a multiple of π/2, 6381956970095103·2^797
    /// (Muller, *Elementary Functions*): 4.687e-19 away, so its cosine and
    /// tangent need π to about 1000 bits.
    const HARDEST: f64 = 5.319372648326541e255;

    /// `cos`, correctly rounded at ±[`HARDEST`] (from exact arithmetic),
    /// where glibc's is 8 ulp off.
    pub fn cos(x: f64) -> f64 {
        if x.abs() == HARDEST { -4.687165924254628e-19 } else { x.cos() }
    }

    /// `tan`, correctly rounded at ±[`HARDEST`], where glibc's is 14 ulp off.
    pub fn tan(x: f64) -> f64 {
        if x.abs() == HARDEST { -2.133485385753704e18 * x.signum() } else { x.tan() }
    }

    pub fn atanh(x: f64) -> f64 {
        0.5 * (x.ln_1p() - (-x).ln_1p())
    }

    pub fn asinh(x: f64) -> f64 {
        let a = x.abs();
        let r = if a < 1e-9 {
            a
        } else if a > 1e9 {
            a.ln() + core::f64::consts::LN_2
        } else {
            (a + a * a / (1.0 + 1f64.hypot(a))).ln_1p()
        };
        r.copysign(x)
    }

    pub fn acosh(x: f64) -> f64 {
        if x.is_nan() || x < 1.0 {
            f64::NAN
        } else if x < 2.0 {
            let t = x - 1.0; // exact
            (t + (2.0 * t + t * t).sqrt()).ln_1p()
        } else if x < 1e9 {
            (2.0 * x - 1.0 / (x + (x * x - 1.0).sqrt())).ln()
        } else {
            x.ln() + core::f64::consts::LN_2
        }
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Report {
    title: &'static str,
    rows: Vec<String>,
    failures: Vec<String>,
}

fn fmt_ulps(u: u64) -> String {
    if u == u64::MAX { String::from("NaN mismatch") } else { format!("{u}") }
}

impl Report {
    fn new(title: &'static str) -> Self {
        Report { title, ..Default::default() }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        name: &str,
        bound: u64,
        n: usize,
        max: u64,
        exact: usize,
        worst: String,
        special: Option<String>,
    ) {
        let pct = 100.0 * exact as f64 / n.max(1) as f64;
        self.rows
            .push(format!("{name:<16} max {:>12} ulp  {pct:>7.3}% exact  n={n:<7} worst at {worst}", fmt_ulps(max)));
        if max > bound {
            self.failures.push(format!("{name}: max error {} ulp > bound {bound} at {worst}", fmt_ulps(max)));
        }
        if let Some(s) = special {
            self.failures.push(format!("{name}: special value mismatch: {s}"));
        }
    }

    /// One-argument f64 function. `special` inputs must match `std` exactly
    /// (including the sign of zero) whenever `std` returns 0, ∞ or NaN.
    fn f64_1(&mut self, name: &str, bound: u64, inputs: &[f64], ours: impl Fn(f64) -> f64, std: impl Fn(f64) -> f64) {
        let (mut max, mut worst, mut exact, mut special) = (0u64, 0.0f64, 0usize, None);
        for &x in inputs {
            let (a, b) = (ours(x), std(x));
            let d = ulps64(a, b);
            if d == 0 {
                exact += 1;
            }
            if d > max {
                max = d;
                worst = x;
            }
            if special.is_none() && special_mismatch(a, b) {
                special = Some(format!("x={x:e}: got {a:e}, std {b:e}"));
            }
        }
        let r = ours(worst);
        self.record(
            name,
            bound,
            inputs.len(),
            max,
            exact,
            format!("x={worst:e} (got {r:e}, std {:e})", std(worst)),
            special,
        );
    }

    fn f64_2(
        &mut self,
        name: &str,
        bound: u64,
        inputs: &[(f64, f64)],
        ours: impl Fn(f64, f64) -> f64,
        std: impl Fn(f64, f64) -> f64,
    ) {
        let (mut max, mut worst, mut exact, mut special) = (0u64, (0.0, 0.0), 0usize, None);
        for &(x, y) in inputs {
            let (a, b) = (ours(x, y), std(x, y));
            let d = ulps64(a, b);
            if d == 0 {
                exact += 1;
            }
            if d > max {
                max = d;
                worst = (x, y);
            }
            if special.is_none() && special_mismatch(a, b) {
                special = Some(format!("({x:e}, {y:e}): got {a:e}, std {b:e}"));
            }
        }
        let (r, s) = (ours(worst.0, worst.1), std(worst.0, worst.1));
        self.record(
            name,
            bound,
            inputs.len(),
            max,
            exact,
            format!("({:e}, {:e}) (got {r:e}, std {s:e})", worst.0, worst.1),
            special,
        );
    }

    fn f32_1(
        &mut self,
        name: &str,
        bound: u64,
        inputs: &[f32],
        ours: impl Fn(f32) -> f32,
        reference: impl Fn(f32) -> f32,
    ) {
        let (mut max, mut worst, mut exact, mut special) = (0u64, 0.0f32, 0usize, None);
        for &x in inputs {
            let (a, b) = (ours(x), reference(x));
            let d = ulps32(a, b);
            if d == 0 {
                exact += 1;
            }
            if d > max {
                max = d;
                worst = x;
            }
            if special.is_none() && special_mismatch(a as f64, b as f64) {
                special = Some(format!("x={x:e}: got {a:e}, ref {b:e}"));
            }
        }
        let r = ours(worst);
        self.record(
            name,
            bound,
            inputs.len(),
            max,
            exact,
            format!("x={worst:e} (got {r:e}, ref {:e})", reference(worst)),
            special,
        );
    }

    fn f32_2(
        &mut self,
        name: &str,
        bound: u64,
        inputs: &[(f32, f32)],
        ours: impl Fn(f32, f32) -> f32,
        reference: impl Fn(f32, f32) -> f32,
    ) {
        let (mut max, mut worst, mut exact, mut special) = (0u64, (0.0, 0.0), 0usize, None);
        for &(x, y) in inputs {
            let (a, b) = (ours(x, y), reference(x, y));
            let d = ulps32(a, b);
            if d == 0 {
                exact += 1;
            }
            if d > max {
                max = d;
                worst = (x, y);
            }
            if special.is_none() && special_mismatch(a as f64, b as f64) {
                special = Some(format!("({x:e}, {y:e}): got {a:e}, ref {b:e}"));
            }
        }
        let (r, s) = (ours(worst.0, worst.1), reference(worst.0, worst.1));
        self.record(
            name,
            bound,
            inputs.len(),
            max,
            exact,
            format!("({:e}, {:e}) (got {r:e}, ref {s:e})", worst.0, worst.1),
            special,
        );
    }

    fn finish(self) {
        println!("\n=== {} ===", self.title);
        for row in &self.rows {
            println!("{row}");
        }
        assert!(self.failures.is_empty(), "{}:\n{}", self.title, self.failures.join("\n"));
    }
}

// ---------------------------------------------------------------------------
// Input generators
// ---------------------------------------------------------------------------

const SPECIAL_F64: &[f64] = &[
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    -0.5,
    2.0,
    -2.0,
    3.0,
    10.0,
    100.0,
    1e10,
    1e22,
    1e300,
    -1e300,
    f64::MAX,
    -f64::MAX,
    f64::MIN_POSITIVE,
    -f64::MIN_POSITIVE,
    5e-324,
    -5e-324,
    1e-310,
    -1e-310,
    f64::EPSILON,
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
    core::f64::consts::PI,
    -core::f64::consts::PI,
    core::f64::consts::FRAC_PI_2,
    -core::f64::consts::FRAC_PI_2,
    core::f64::consts::FRAC_PI_4,
    core::f64::consts::E,
    0.1,
    -0.1,
    1e-8,
    709.78,
    -745.1,
    1024.0,
    -1074.0,
    -1075.0,
    0.9999999999999999,
    1.0000000000000002,
];

fn special_f32() -> Vec<f32> {
    let mut v: Vec<f32> = SPECIAL_F64.iter().map(|&x| x as f32).collect();
    v.extend_from_slice(&[
        f32::MAX,
        -f32::MAX,
        f32::MIN_POSITIVE,
        1e-45,
        -1e-45,
        1e-40,
        f32::EPSILON,
        88.72283,
        88.722_84,
        -103.972,
        -104.0,
        128.0,
        -150.0,
        -149.5,
        0.99999994,
        1.0000001,
    ]);
    v
}

fn uniform(rng: &mut Rng, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * rng.next_f64()
}

/// Random sign times 2^U(lo, hi) (log-uniform magnitudes).
fn log_uniform(rng: &mut Rng, lo: f64, hi: f64, signed: bool) -> f64 {
    let v = uniform(rng, lo, hi).exp2();
    if signed && rng.next_bool() { -v } else { v }
}

/// A mix of magnitudes, special values, random bit patterns and subnormals.
fn general_f64(rng: &mut Rng, n: usize) -> Vec<f64> {
    let mut v = Vec::from(SPECIAL_F64);
    for _ in 0..n / 8 {
        v.push(quiet64(f64::from_bits(rng.next_u64())));
        v.push(uniform(rng, -1.0, 1.0));
        v.push(uniform(rng, -10.0, 10.0));
        v.push(uniform(rng, -1000.0, 1000.0));
        v.push(log_uniform(rng, -60.0, 60.0, true));
        v.push(log_uniform(rng, -1074.0, 1024.0, true));
        v.push(f64::from_bits(rng.next_u64() >> 12) * if rng.next_bool() { -1.0 } else { 1.0 }); // subnormal
        v.push((rng.range_i32(-1000, 1000) as f64) * 0.5); // integers and halves
    }
    v
}

fn general_f32(rng: &mut Rng, n: usize) -> Vec<f32> {
    let mut v = special_f32();
    for _ in 0..n / 8 {
        v.push(quiet32(f32::from_bits(rng.next_u32())));
        v.push(uniform(rng, -1.0, 1.0) as f32);
        v.push(uniform(rng, -10.0, 10.0) as f32);
        v.push(uniform(rng, -1000.0, 1000.0) as f32);
        v.push(log_uniform(rng, -30.0, 30.0, true) as f32);
        v.push(log_uniform(rng, -149.0, 128.0, true) as f32);
        v.push(f32::from_bits(rng.next_u32() >> 9) * if rng.next_bool() { -1.0 } else { 1.0 });
        v.push((rng.range_i32(-1000, 1000) as f32) * 0.5);
    }
    v
}

/// Arguments that stress trigonometric argument reduction.
fn trig_f64(rng: &mut Rng, n: usize) -> Vec<f64> {
    use core::f64::consts::FRAC_PI_2;
    let mut v = general_f64(rng, n);
    for k in 1..2000 {
        let x = k as f64 * FRAC_PI_2;
        v.extend_from_slice(&[x, -x, f64::from_bits(x.to_bits() + 1), f64::from_bits(x.to_bits() - 1)]);
    }
    for _ in 0..n / 4 {
        let k = rng.range_i32(-1_000_000, 1_000_000) as f64;
        let x = k * FRAC_PI_2;
        let ulp_off = rng.range_i32(-8, 9) as i64;
        v.push(f64::from_bits((x.to_bits() as i64 + ulp_off) as u64));
        v.push(uniform(rng, -1e5, 1e5));
        v.push(log_uniform(rng, 17.0, 70.0, true)); // around and beyond 2^20·π/2
        v.push(log_uniform(rng, 70.0, 1024.0, true)); // huge
    }
    // Known hard cases for argument reduction.
    v.extend_from_slice(&[1e22, 5.319372648326541e255, 6381956970095103.0 * 2f64.powi(797), 1.0e18, 2f64.powi(1023)]);
    v
}

fn trig_f32(rng: &mut Rng, n: usize) -> Vec<f32> {
    use core::f32::consts::FRAC_PI_2;
    let mut v = general_f32(rng, n);
    for k in 1..20000 {
        let x = k as f32 * FRAC_PI_2;
        v.extend_from_slice(&[x, -x, f32::from_bits(x.to_bits() + 1), f32::from_bits(x.to_bits() - 1)]);
    }
    for _ in 0..n / 4 {
        v.push(uniform(rng, -1e5, 1e5) as f32);
        v.push(uniform(rng, -10.0, 10.0) as f32);
        v.push(log_uniform(rng, 20.0, 128.0, true) as f32);
    }
    // Large arguments (the exhaustive test covers every float as well).
    v.extend_from_slice(&[1.633_124e16, 7.662_035e23, 4.230_996_6e20, 1e10, 1e20, 1e30, f32::MAX]);
    v
}

fn positive_f64(rng: &mut Rng, n: usize) -> Vec<f64> {
    let mut v: Vec<f64> = general_f64(rng, n).into_iter().map(f64::abs).collect();
    for _ in 0..n / 4 {
        v.push(1.0 + uniform(rng, -0.3, 0.42)); // reduction boundaries
        v.push(1.0 + log_uniform(rng, -60.0, -1.0, true)); // near 1
        v.push(log_uniform(rng, -1074.0, 1024.0, false));
        v.push(10f64.powi(rng.range_i32(-300, 300)));
    }
    v
}

fn positive_f32(rng: &mut Rng, n: usize) -> Vec<f32> {
    let mut v: Vec<f32> = general_f32(rng, n).into_iter().map(f32::abs).collect();
    for _ in 0..n / 4 {
        v.push((1.0 + uniform(rng, -0.3, 0.42)) as f32);
        v.push((1.0 + log_uniform(rng, -30.0, -1.0, true)) as f32);
        v.push(log_uniform(rng, -149.0, 128.0, false) as f32);
        v.push(10f32.powi(rng.range_i32(-38, 38)));
    }
    v
}

fn unit_interval_f64(rng: &mut Rng, n: usize) -> Vec<f64> {
    let mut v = general_f64(rng, n / 2);
    for _ in 0..n / 4 {
        v.push(uniform(rng, -1.0, 1.0));
        v.push(1.0 - log_uniform(rng, -53.0, -1.0, false)); // near 1
        v.push(-1.0 + log_uniform(rng, -53.0, -1.0, false)); // near -1
        v.push(log_uniform(rng, -60.0, -1.0, true)); // near 0
    }
    v
}

fn unit_interval_f32(rng: &mut Rng, n: usize) -> Vec<f32> {
    let mut v = general_f32(rng, n / 2);
    for _ in 0..n / 4 {
        v.push(uniform(rng, -1.0, 1.0) as f32);
        v.push((1.0 - log_uniform(rng, -24.0, -1.0, false)) as f32);
        v.push((-1.0 + log_uniform(rng, -24.0, -1.0, false)) as f32);
        v.push(log_uniform(rng, -30.0, -1.0, true) as f32);
    }
    v
}

fn pairs_f64(rng: &mut Rng, xs: &[f64], ys: &[f64], n: usize) -> Vec<(f64, f64)> {
    let mut v = Vec::new();
    for &x in SPECIAL_F64 {
        for &y in SPECIAL_F64 {
            v.push((x, y));
        }
    }
    for _ in 0..n {
        let x = xs[rng.below_usize(xs.len())];
        let y = ys[rng.below_usize(ys.len())];
        v.push((x, y));
    }
    v
}

fn pairs_f32(rng: &mut Rng, xs: &[f32], ys: &[f32], n: usize) -> Vec<(f32, f32)> {
    let sp = special_f32();
    let mut v = Vec::new();
    for &x in &sp {
        for &y in &sp {
            v.push((x, y));
        }
    }
    for _ in 0..n {
        let x = xs[rng.below_usize(xs.len())];
        let y = ys[rng.below_usize(ys.len())];
        v.push((x, y));
    }
    v
}

const N: usize = 200_000;

// ---------------------------------------------------------------------------
// f64
// ---------------------------------------------------------------------------

mod f64_tests {
    use super::*;
    use crate::f64 as m;

    #[test]
    fn libm_f64_exact_functions() {
        let mut rng = Rng::new(1);
        let mut v = general_f64(&mut rng, N);
        for _ in 0..N / 4 {
            // Half-integers and values near rounding boundaries.
            let k = rng.range_i32(-1 << 20, 1 << 20) as f64 + 0.5;
            v.push(k);
            v.push(f64::from_bits(k.to_bits() + 1));
            v.push(f64::from_bits(k.to_bits() - 1));
            v.push(log_uniform(&mut rng, 40.0, 60.0, true)); // around 2^52
        }
        let mut r = Report::new("f64 exact functions (bound 0 ulp)");
        r.f64_1("sqrt", 0, &v, m::sqrt, f64::sqrt);
        r.f64_1("sqrt_soft", 0, &v, m::sqrt_soft, f64::sqrt);
        r.f64_1("floor", 0, &v, m::floor, f64::floor);
        r.f64_1("ceil", 0, &v, m::ceil, f64::ceil);
        r.f64_1("trunc", 0, &v, m::trunc, f64::trunc);
        r.f64_1("round", 0, &v, m::round, f64::round);
        r.f64_1("round_ties_even", 0, &v, m::round_ties_even, f64::round_ties_even);
        r.f64_1("fract", 0, &v, m::fract, f64::fract);
        r.f64_1("abs", 0, &v, m::abs, f64::abs);
        r.f64_1("signum", 0, &v, m::signum, f64::signum);
        r.f64_1("recip", 0, &v, m::recip, f64::recip);
        r.f64_1("to_degrees", 0, &v, m::to_degrees, f64::to_degrees);
        r.f64_1("to_radians", 0, &v, m::to_radians, f64::to_radians);
        // std's powi is pow(x, n) with the MSVC runtime: small exponents use
        // multiplication here (<= 1.5 ulp), large ones powf (< 1 ulp).
        for n in [-1075, -1023, -64, -7, -5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 10, 31, 64, 1000, i32::MAX, i32::MIN] {
            let bound = match n.unsigned_abs() {
                0 => 0,
                1 => 1, // 1/x is correctly rounded, the C runtime's pow(x, -1) is not always
                2..=4 => 3,
                _ => 1,
            };
            r.f64_1(&format!("powi(x,{n})"), bound, &v, |x| m::powi(x, n), |x| x.powf(n as f64));
        }

        let mut pv = pairs_f64(&mut rng, &v, &v, N);
        for _ in 0..N / 2 {
            // Remainders with small and large exponent differences.
            pv.push((log_uniform(&mut rng, -30.0, 30.0, true), log_uniform(&mut rng, -30.0, 30.0, true)));
            pv.push((log_uniform(&mut rng, 900.0, 1023.0, true), log_uniform(&mut rng, -1074.0, -1000.0, true)));
        }
        r.f64_2("fmod", 0, &pv, m::fmod, |x, y| x % y);
        r.f64_2("rem_euclid", 0, &pv, m::rem_euclid, f64::rem_euclid);
        r.f64_2("div_euclid", 0, &pv, m::div_euclid, f64::div_euclid);
        r.f64_2("copysign", 0, &pv, m::copysign, f64::copysign);
        r.f64_2("min", 0, &pv, m::min, f64::min);
        r.f64_2("max", 0, &pv, m::max, f64::max);
        r.f64_2("mul_add(x,y,1)", 0, &pv, |x, y| m::mul_add(x, y, 1.0), |x, y| x * y + 1.0);
        r.finish();
    }

    #[test]
    fn libm_f64_accuracy() {
        let mut rng = Rng::new(2);
        let gv = general_f64(&mut rng, N);
        let trig = trig_f64(&mut rng, N);
        let pos = positive_f64(&mut rng, N);
        let unit = unit_interval_f64(&mut rng, N);
        let mut expv = general_f64(&mut rng, N / 2);
        let mut hyp = general_f64(&mut rng, N / 2);
        let mut log1p = general_f64(&mut rng, N / 2);
        for _ in 0..N / 2 {
            expv.push(uniform(&mut rng, -746.0, 710.0));
            expv.push(uniform(&mut rng, -1076.0, 1025.0));
            expv.push(log_uniform(&mut rng, -60.0, 0.0, true));
            hyp.push(uniform(&mut rng, -720.0, 720.0));
            hyp.push(uniform(&mut rng, -25.0, 25.0));
            hyp.push(log_uniform(&mut rng, -60.0, 3.0, true));
            log1p.push(uniform(&mut rng, -1.0, 1.0));
            log1p.push(log_uniform(&mut rng, -60.0, -1.0, true));
            log1p.push(log_uniform(&mut rng, -1.0, 1000.0, false));
        }

        // Note: the reference (the host C runtime) is itself not always correctly
        // rounded; e.g. its cbrt(-6.007033920768956) is off by 1.39 ulp and its
        // expm1(1.4661957730558935e-15) by 2 ulp (checked with exact arithmetic),
        // where vmath is within 0.61 ulp and correctly rounded respectively.
        let mut r = Report::new("f64 vs std (host C runtime)");
        r.f64_1("cbrt", 2, &gv, m::cbrt, f64::cbrt);
        r.f64_1("sin", 1, &trig, m::sin, f64::sin);
        r.f64_1("cos", 1, &trig, m::cos, reference::cos);
        r.f64_1("tan", 1, &trig, m::tan, reference::tan);
        r.f64_1("asin", 1, &unit, m::asin, f64::asin);
        r.f64_1("acos", 1, &unit, m::acos, f64::acos);
        r.f64_1("atan", 1, &gv, m::atan, f64::atan);
        r.f64_1("exp", 1, &expv, m::exp, f64::exp);
        r.f64_1("exp2", 1, &expv, m::exp2, f64::exp2);
        r.f64_1("exp_m1", 2, &expv, m::exp_m1, f64::exp_m1);
        r.f64_1("ln", 1, &pos, m::ln, f64::ln);
        r.f64_1("log2", 1, &pos, m::log2, f64::log2);
        // glibc's log10 is not correctly rounded: where it and vmath's are 2
        // ulp apart, it is up to 1.6 ulp from the exact result and vmath's
        // within 0.6 (checked with exact arithmetic).
        r.f64_1("log10", if GLIBC { 2 } else { 1 }, &pos, m::log10, f64::log10);
        r.f64_1("ln(general)", 1, &gv, m::ln, f64::ln);
        r.f64_1("ln_1p", 1, &log1p, m::ln_1p, f64::ln_1p);
        r.f64_1("sinh", 2, &hyp, m::sinh, f64::sinh);
        r.f64_1("cosh", 2, &hyp, m::cosh, f64::cosh);
        r.f64_1("tanh", 2, &hyp, m::tanh, f64::tanh);
        r.f64_1("asinh", 2, &gv, m::asinh, reference::asinh);
        r.f64_1("acosh", 3, &pos, m::acosh, reference::acosh);
        r.f64_1("atanh", 2, &unit, m::atanh, reference::atanh);

        let pv = pairs_f64(&mut rng, &gv, &gv, N);
        let mut atan2v = pairs_f64(&mut rng, &unit, &unit, N / 2);
        let mut hypotv = pv.clone();
        for _ in 0..N / 2 {
            atan2v.push((uniform(&mut rng, -10.0, 10.0), uniform(&mut rng, -10.0, 10.0)));
            hypotv.push((log_uniform(&mut rng, -1074.0, 1024.0, true), log_uniform(&mut rng, -1074.0, 1024.0, true)));
            hypotv.push((log_uniform(&mut rng, -10.0, 10.0, true), log_uniform(&mut rng, -10.0, 10.0, true)));
        }
        r.f64_2("atan2", 2, &atan2v, m::atan2, f64::atan2);
        r.f64_2("atan2(general)", 2, &pv, m::atan2, f64::atan2);
        r.f64_2("hypot", 1, &hypotv, m::hypot, f64::hypot);

        let mut powv = pairs_f64(&mut rng, &pos, &gv, N / 2);
        for _ in 0..N / 2 {
            let x = log_uniform(&mut rng, -20.0, 20.0, false);
            powv.push((x, uniform(&mut rng, -50.0, 50.0)));
            powv.push((1.0 + log_uniform(&mut rng, -50.0, -1.0, true), log_uniform(&mut rng, 3.0, 40.0, true)));
            powv.push((-log_uniform(&mut rng, -20.0, 20.0, false), rng.range_i32(-60, 60) as f64));
            powv.push((log_uniform(&mut rng, -1074.0, 1024.0, false), uniform(&mut rng, -2.0, 2.0)));
            powv.push((uniform(&mut rng, 0.0, 2.0), 0.5));
        }
        r.f64_2("powf", 1, &powv, m::powf, f64::powf);
        r.f64_2("powf(general)", 1, &pv, m::powf, f64::powf);
        // ln(x) / ln(base) with two independently rounded logarithms: a few ulp.
        r.f64_2("log(x,b)", 4, &pairs_f64(&mut rng, &pos, &pos, N), m::log, f64::log);
        r.finish();
    }

    #[test]
    fn sin_cos_matches_sin_and_cos() {
        let mut rng = Rng::new(3);
        for x in trig_f64(&mut rng, N / 4) {
            let (s, c) = m::sin_cos(x);
            assert!(same64(s, m::sin(x)) && same64(c, m::cos(x)), "x = {x:e}");
        }
    }

    /// The Payne–Hanek path must agree with the Cody–Waite path where both apply.
    #[test]
    fn large_reduction_matches_medium() {
        let mut rng = Rng::new(4);
        for i in 0..N {
            let x = if i % 2 == 0 {
                uniform(&mut rng, 1.0, 1_647_000.0)
            } else {
                // near multiples of π/2 (cancellation)
                let k = rng.range_i32(1, 1_000_000) as f64 * core::f64::consts::FRAC_PI_2;
                f64::from_bits((k.to_bits() as i64 + rng.range_i32(-4, 5) as i64) as u64)
            };
            let x = if rng.next_bool() { -x } else { x };
            let (n1, a0, a1) = m::rem_pio2(x);
            let (n2, b0, b1) = m::rem_pio2_large(x);
            assert_eq!(n1, n2, "quadrant mismatch for x = {x:e}");
            let (a, b) = (a0 + a1, b0 + b1);
            assert!(ulps64(a, b) <= 1, "x = {x:e}: {a0:e}+{a1:e} vs {b0:e}+{b1:e}");
            // As double-doubles both agree far beyond double precision (the
            // Cody-Waite result itself has an absolute error of about |x|·2^-87).
            let diff = ((a0 - b0) + (a1 - b1)).abs();
            assert!(diff <= x.abs() * 2f64.powi(-80) + a.abs() * 2f64.powi(-70), "x = {x:e}: tails differ by {diff:e}");
        }
    }

    #[test]
    fn special_cases() {
        const INF: f64 = f64::INFINITY;
        const NAN: f64 = f64::NAN;
        // A few cases spelled out (the accuracy tests also check every special value).
        assert!(same64(m::sin(-0.0), -0.0));
        assert!(same64(m::tan(-0.0), -0.0));
        assert!(same64(m::floor(-0.5), -1.0));
        assert!(same64(m::ceil(-0.5), -0.0));
        assert!(same64(m::round(-0.5), -1.0));
        assert!(same64(m::round(2.5), 3.0));
        assert!(same64(m::round_ties_even(2.5), 2.0));
        assert!(same64(m::round_ties_even(-0.5), -0.0));
        assert!(same64(m::trunc(-0.9), -0.0));
        assert!(same64(m::fract(-0.0), 0.0));
        assert!(m::fract(INF).is_nan());
        assert!(same64(m::sqrt(-0.0), -0.0));
        assert!(m::sqrt(-1.0).is_nan());
        assert!(same64(m::cbrt(-27.0), -3.0));
        assert!(same64(m::exp(-INF), 0.0));
        assert!(same64(m::exp(INF), INF));
        assert!(same64(m::exp_m1(-INF), -1.0));
        assert!(same64(m::ln(0.0), -INF));
        assert!(same64(m::ln(-0.0), -INF));
        assert!(m::ln(-1.0).is_nan());
        assert!(same64(m::ln_1p(-1.0), -INF));
        assert!(same64(m::log2(8.0), 3.0));
        assert!(same64(m::log10(1000.0), 3.0));
        assert!(same64(m::powf(NAN, 0.0), 1.0));
        assert!(same64(m::powf(1.0, NAN), 1.0));
        assert!(same64(m::powf(-1.0, INF), 1.0));
        assert!(same64(m::powf(-0.0, -3.0), -INF));
        assert!(same64(m::powf(-0.0, 3.0), -0.0));
        assert!(same64(m::powf(-8.0, 1.0 / 3.0), NAN) || m::powf(-8.0, 1.0 / 3.0).is_nan());
        assert!(same64(m::powf(-2.0, 3.0), -8.0));
        assert!(same64(m::powf(2.0, 0.5), core::f64::consts::SQRT_2));
        assert!(same64(m::hypot(INF, NAN), INF));
        assert!(same64(m::hypot(NAN, -INF), INF));
        assert!(same64(m::atan2(0.0, -0.0), core::f64::consts::PI));
        assert!(same64(m::atan2(-0.0, -0.0), -core::f64::consts::PI));
        assert!(same64(m::atan2(-0.0, 0.0), -0.0));
        assert!(m::fmod(1.0, 0.0).is_nan());
        assert!(same64(m::fmod(-5.0, 3.0), -2.0));
        assert!(same64(m::rem_euclid(-5.0, 3.0), 1.0));
        assert!(same64(m::div_euclid(-5.0, 3.0), -2.0));
        assert!(same64(m::signum(-0.0), -1.0));
        assert!(m::signum(NAN).is_nan());
        assert_eq!(m::min(NAN, 1.0), 1.0);
        assert_eq!(m::max(1.0, NAN), 1.0);
        assert_eq!(m::scalbn(1.0, -1074), 5e-324);
        assert_eq!(m::scalbn(1.5, 1024), INF);
        assert_eq!(m::scalbn(f64::MAX, -2046), f64::MAX * 2f64.powi(-1023) * 2f64.powi(-1023));
        assert_eq!(m::lerp(1.0, 3.0, 0.5), 2.0);
        assert_eq!(m::clamp(5.0, 0.0, 1.0), 1.0);
    }

    #[test]
    #[should_panic]
    fn clamp_panics_like_std() {
        let _ = m::clamp(0.5, 1.0, 0.0);
    }
}

// ---------------------------------------------------------------------------
// f32
// ---------------------------------------------------------------------------

mod f32_tests {
    use super::*;
    use crate::f32 as m;

    /// Reference: the `std` f64 function on the widened argument, rounded to f32.
    fn r1(f: fn(f64) -> f64) -> impl Fn(f32) -> f32 {
        move |x| f(x as f64) as f32
    }

    fn r2(f: fn(f64, f64) -> f64) -> impl Fn(f32, f32) -> f32 {
        move |x, y| f(x as f64, y as f64) as f32
    }

    #[test]
    fn libm_f32_exact_functions() {
        let mut rng = Rng::new(11);
        let mut v = general_f32(&mut rng, N);
        for _ in 0..N / 4 {
            let k = rng.range_i32(-1 << 20, 1 << 20) as f32 + 0.5;
            v.push(k);
            v.push(f32::from_bits(k.to_bits() + 1));
            v.push(f32::from_bits(k.to_bits() - 1));
            v.push(log_uniform(&mut rng, 18.0, 26.0, true) as f32);
        }
        let mut r = Report::new("f32 exact functions (bound 0 ulp)");
        r.f32_1("sqrt", 0, &v, m::sqrt, f32::sqrt);
        r.f32_1("sqrt_soft", 0, &v, m::sqrt_soft, f32::sqrt);
        r.f32_1("floor", 0, &v, m::floor, f32::floor);
        r.f32_1("ceil", 0, &v, m::ceil, f32::ceil);
        r.f32_1("trunc", 0, &v, m::trunc, f32::trunc);
        r.f32_1("round", 0, &v, m::round, f32::round);
        r.f32_1("round_ties_even", 0, &v, m::round_ties_even, f32::round_ties_even);
        r.f32_1("fract", 0, &v, m::fract, f32::fract);
        r.f32_1("abs", 0, &v, m::abs, f32::abs);
        r.f32_1("signum", 0, &v, m::signum, f32::signum);
        r.f32_1("recip", 0, &v, m::recip, f32::recip);
        r.f32_1("to_degrees", 0, &v, m::to_degrees, f32::to_degrees);
        r.f32_1("to_radians", 0, &v, m::to_radians, f32::to_radians);
        for n in
            [-150, -127, -65, -64, -9, -3, -2, -1, 0, 1, 2, 3, 5, 7, 24, 64, 65, 128, 16_777_217, i32::MAX, i32::MIN]
        {
            let reference = move |x: f32| (x as f64).powf(n as f64) as f32;
            r.f32_1(&format!("powi(x,{n})"), 1, &v, |x| m::powi(x, n), reference);
        }
        let mut pv = pairs_f32(&mut rng, &v, &v, N);
        for _ in 0..N / 2 {
            pv.push((log_uniform(&mut rng, -20.0, 20.0, true) as f32, log_uniform(&mut rng, -20.0, 20.0, true) as f32));
            pv.push((
                log_uniform(&mut rng, 100.0, 127.0, true) as f32,
                log_uniform(&mut rng, -149.0, -120.0, true) as f32,
            ));
        }
        r.f32_2("fmod", 0, &pv, m::fmod, |x, y| x % y);
        r.f32_2("rem_euclid", 0, &pv, m::rem_euclid, f32::rem_euclid);
        r.f32_2("div_euclid", 0, &pv, m::div_euclid, f32::div_euclid);
        r.f32_2("copysign", 0, &pv, m::copysign, f32::copysign);
        r.f32_2("min", 0, &pv, m::min, f32::min);
        r.f32_2("max", 0, &pv, m::max, f32::max);
        r.f32_2("mul_add(x,y,1)", 1, &pv, |x, y| m::mul_add(x, y, 1.0), |x, y| x.mul_add(y, 1.0));
        r.f32_2("mul_add(x,y,-x)", 1, &pv, |x, y| m::mul_add(x, y, -x), |x, y| x.mul_add(y, -x));
        r.finish();
    }

    #[test]
    fn libm_f32_accuracy() {
        let mut rng = Rng::new(12);
        let gv = general_f32(&mut rng, N);
        let trig = trig_f32(&mut rng, N);
        let pos = positive_f32(&mut rng, N);
        let unit = unit_interval_f32(&mut rng, N);
        let mut expv = general_f32(&mut rng, N / 2);
        let mut log1p = general_f32(&mut rng, N / 2);
        for _ in 0..N / 2 {
            expv.push(uniform(&mut rng, -105.0, 89.0) as f32);
            expv.push(uniform(&mut rng, -151.0, 129.0) as f32);
            expv.push(log_uniform(&mut rng, -30.0, 0.0, true) as f32);
            log1p.push(uniform(&mut rng, -1.0, 1.0) as f32);
            log1p.push(log_uniform(&mut rng, -30.0, -1.0, true) as f32);
            log1p.push(log_uniform(&mut rng, -1.0, 100.0, false) as f32);
        }

        let mut r = Report::new("f32 vs correctly rounded reference");
        r.f32_1("cbrt", 1, &gv, m::cbrt, r1(f64::cbrt));
        r.f32_1("sin", 1, &trig, m::sin, r1(f64::sin));
        r.f32_1("cos", 1, &trig, m::cos, r1(f64::cos));
        r.f32_1("tan", 1, &trig, m::tan, r1(f64::tan));
        r.f32_1("asin", 1, &unit, m::asin, r1(f64::asin));
        r.f32_1("acos", 1, &unit, m::acos, r1(f64::acos));
        r.f32_1("atan", 1, &gv, m::atan, r1(f64::atan));
        r.f32_1("exp", 1, &expv, m::exp, r1(f64::exp));
        r.f32_1("exp2", 1, &expv, m::exp2, r1(f64::exp2));
        r.f32_1("exp_m1", 1, &expv, m::exp_m1, r1(f64::exp_m1));
        r.f32_1("ln", 1, &pos, m::ln, r1(f64::ln));
        r.f32_1("log2", 1, &pos, m::log2, r1(f64::log2));
        r.f32_1("log10", 1, &pos, m::log10, r1(f64::log10));
        r.f32_1("ln(general)", 1, &gv, m::ln, r1(f64::ln));
        r.f32_1("ln_1p", 1, &log1p, m::ln_1p, r1(f64::ln_1p));
        r.f32_1("sinh", 1, &expv, m::sinh, r1(f64::sinh));
        r.f32_1("cosh", 1, &expv, m::cosh, r1(f64::cosh));
        r.f32_1("tanh", 1, &gv, m::tanh, r1(f64::tanh));
        r.f32_1("asinh", 1, &gv, m::asinh, r1(f64::asinh));
        r.f32_1("acosh", 1, &pos, m::acosh, r1(f64::acosh));
        r.f32_1("atanh", 1, &unit, m::atanh, r1(f64::atanh));

        let pv = pairs_f32(&mut rng, &gv, &gv, N);
        let mut atan2v = pairs_f32(&mut rng, &unit, &unit, N / 2);
        let mut hypotv = pv.clone();
        for _ in 0..N / 2 {
            atan2v.push((uniform(&mut rng, -10.0, 10.0) as f32, uniform(&mut rng, -10.0, 10.0) as f32));
            hypotv.push((
                log_uniform(&mut rng, -149.0, 128.0, true) as f32,
                log_uniform(&mut rng, -149.0, 128.0, true) as f32,
            ));
        }
        r.f32_2("atan2", 1, &atan2v, m::atan2, r2(f64::atan2));
        r.f32_2("atan2(general)", 1, &pv, m::atan2, r2(f64::atan2));
        r.f32_2("hypot", 1, &hypotv, m::hypot, r2(f64::hypot));

        let mut powv = pairs_f32(&mut rng, &pos, &gv, N / 2);
        for _ in 0..N / 2 {
            powv.push((log_uniform(&mut rng, -10.0, 10.0, false) as f32, uniform(&mut rng, -30.0, 30.0) as f32));
            powv.push((
                (1.0 + log_uniform(&mut rng, -23.0, -1.0, true)) as f32,
                log_uniform(&mut rng, 3.0, 30.0, true) as f32,
            ));
            powv.push((-log_uniform(&mut rng, -10.0, 10.0, false) as f32, rng.range_i32(-40, 40) as f32));
            powv.push((uniform(&mut rng, 0.0, 1.0) as f32, 2.4));
            powv.push((uniform(&mut rng, 0.0, 1.0) as f32, 1.0 / 2.4));
        }
        r.f32_2("powf", 1, &powv, m::powf, r2(f64::powf));
        r.f32_2("powf(general)", 1, &pv, m::powf, r2(f64::powf));
        // ln(x) / ln(base), here and in std, from two independently rounded
        // logarithms each: where the two were 3 ulp apart, both were within
        // 1.7 ulp of the exact result, on either side of it (checked with
        // exact arithmetic).
        r.f32_2("log(x,b)", 3, &pairs_f32(&mut rng, &pos, &pos, N), m::log, f32::log);
        r.finish();
    }

    #[test]
    fn sin_cos_matches_sin_and_cos() {
        let mut rng = Rng::new(13);
        for x in trig_f32(&mut rng, N / 4) {
            let (s, c) = m::sin_cos(x);
            assert!(same32(s, m::sin(x)) && same32(c, m::cos(x)), "x = {x:e}");
        }
    }

    /// Checks single-argument functions on every `f32` (slow: run in release mode).
    #[test]
    #[ignore = "exhaustive: run with --release -- --ignored"]
    fn exhaustive_f32() {
        type F = (&'static str, fn(f32) -> f32, fn(f64) -> f64);
        let funcs: &[F] = &[
            ("sin", m::sin, f64::sin),
            ("cos", m::cos, f64::cos),
            ("tan", m::tan, f64::tan),
            ("asin", m::asin, f64::asin),
            ("acos", m::acos, f64::acos),
            ("atan", m::atan, f64::atan),
            ("exp", m::exp, f64::exp),
            ("exp2", m::exp2, f64::exp2),
            ("exp_m1", m::exp_m1, f64::exp_m1),
            ("ln", m::ln, f64::ln),
            ("log2", m::log2, f64::log2),
            ("log10", m::log10, f64::log10),
            ("ln_1p", m::ln_1p, f64::ln_1p),
            ("cbrt", m::cbrt, f64::cbrt),
            ("sinh", m::sinh, f64::sinh),
            ("cosh", m::cosh, f64::cosh),
            ("tanh", m::tanh, f64::tanh),
            ("sqrt", m::sqrt, f64::sqrt),
        ];
        let threads = std::thread::available_parallelism().map_or(8, |n| n.get());
        println!("\n=== f32 exhaustive (all 2^32 inputs, {threads} threads) ===");
        for &(name, ours, reference) in funcs {
            let chunk = (1u64 << 32) / threads as u64;
            let results: Vec<(u64, u32, u64)> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..threads as u64)
                    .map(|t| {
                        s.spawn(move || {
                            let (mut max, mut worst, mut wrong) = (0u64, 0u32, 0u64);
                            let start = t * chunk;
                            let end = if t + 1 == threads as u64 { 1u64 << 32 } else { start + chunk };
                            for bits in start..end {
                                let x = f32::from_bits(bits as u32);
                                let d = ulps32(ours(x), reference(x as f64) as f32);
                                if d != 0 {
                                    wrong += 1;
                                }
                                if d > max {
                                    max = d;
                                    worst = bits as u32;
                                }
                            }
                            (max, worst, wrong)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let max = results.iter().map(|r| r.0).max().unwrap();
            let worst = results.iter().max_by_key(|r| r.0).unwrap().1;
            let wrong: u64 = results.iter().map(|r| r.2).sum();
            println!(
                "{name:<8} max {:>3} ulp, {wrong:>10} of 2^32 not correctly rounded, worst x = {:e} ({worst:#010x})",
                fmt_ulps(max),
                f32::from_bits(worst)
            );
            assert!(max <= 1, "{name}: {max} ulp");
        }
    }

    #[test]
    fn special_cases() {
        const INF: f32 = f32::INFINITY;
        const NAN: f32 = f32::NAN;
        assert!(same32(m::sin(-0.0), -0.0));
        assert!(same32(m::floor(-0.5), -1.0));
        assert!(same32(m::ceil(-0.5), -0.0));
        assert!(same32(m::round(-2.5), -3.0));
        assert!(same32(m::round_ties_even(-2.5), -2.0));
        assert!(same32(m::exp(-INF), 0.0));
        assert!(same32(m::exp2(-150.0), 0.0));
        assert!(same32(m::exp2(-149.0), 1e-45));
        assert!(same32(m::exp2(127.0), 2f32.powi(127)));
        assert!(same32(m::exp2(128.0), INF));
        assert!(same32(m::ln(0.0), -INF));
        assert!(m::ln(-1.0).is_nan());
        assert!(same32(m::log2(1024.0), 10.0));
        assert!(same32(m::log2(1e-45), -149.0));
        assert!(same32(m::log10(1e-10), -10.0));
        assert!(same32(m::powf(NAN, 0.0), 1.0));
        assert!(same32(m::powf(1.0, NAN), 1.0));
        assert!(same32(m::powf(-0.0, -1.0), -INF));
        assert!(same32(m::powf(-INF, 3.0), -INF));
        assert!(same32(m::powf(-INF, -3.0), -0.0));
        assert!(same32(m::powf(-2.0, 3.0), -8.0));
        assert!(same32(m::powf(2.0, 10.0), 1024.0));
        assert!(same32(m::powf(10.0, -2.0), 0.01));
        assert!(m::powf(-2.0, 0.5).is_nan());
        assert!(same32(m::powf(0.5, INF), 0.0));
        assert!(same32(m::powf(0.5, -INF), INF));
        assert!(same32(m::hypot(3.0, 4.0), 5.0));
        assert!(same32(m::hypot(NAN, INF), INF));
        assert!(same32(m::cbrt(-8.0), -2.0));
        assert!(same32(m::atan2(1.0, -INF), core::f32::consts::PI));
        assert!(same32(m::fmod(-7.5, 2.0), -1.5));
        assert!(same32(m::rem_euclid(-7.5, 2.0), 0.5));
        assert!(same32(m::div_euclid(-7.5, 2.0), -4.0));
    }
}

// ---------------------------------------------------------------------------
// Compile-time evaluation and trait dispatch
// ---------------------------------------------------------------------------

#[test]
fn const_evaluation_matches_runtime() {
    const S: f32 = crate::f32::sin(1.0);
    const E: f64 = crate::f64::exp(1.0);
    const P: f32 = crate::f32::powf(2.0, 0.5);
    const L: f64 = crate::f64::ln(10.0);
    const T: f64 = crate::f64::tan(1e22);
    const F: f32 = crate::f32::floor(-1.5);
    let one = std::hint::black_box(1.0f32);
    assert_eq!(S, crate::f32::sin(one));
    assert_eq!(E, crate::f64::exp(one as f64));
    assert_eq!(P, crate::f32::powf(2.0 * one, 0.5));
    assert_eq!(L, crate::f64::ln(10.0 * one as f64));
    assert_eq!(T, crate::f64::tan(1e22 * one as f64));
    assert_eq!(F, -2.0);
}

#[test]
fn float_ext_dispatches_to_vmath() {
    use crate::FloatExt;
    let x = std::hint::black_box(0.7f32);
    let y = std::hint::black_box(0.7f64);
    assert_eq!(FloatExt::sin(x), crate::f32::sin(x));
    assert_eq!(FloatExt::powf(x, 3.3), crate::f32::powf(x, 3.3));
    assert_eq!(FloatExt::atan2(x, 2.0), crate::f32::atan2(x, 2.0));
    assert_eq!(FloatExt::sin_cos(y), crate::f64::sin_cos(y));
    assert_eq!(FloatExt::hypot(y, 2.0), crate::f64::hypot(y, 2.0));
    assert_eq!(FloatExt::mul_add(y, 2.0, 1.0), crate::f64::mul_add(y, 2.0, 1.0));
    assert_eq!(FloatExt::rem_euclid(-y, 0.25), crate::f64::rem_euclid(-y, 0.25));
    assert_eq!(FloatExt::powi(y, -3), crate::f64::powi(y, -3));
    assert_eq!(FloatExt::lerp(1.0f32, 3.0, 0.5), 2.0);
    assert_eq!(FloatExt::log(8.0f64, 2.0), 3.0);
}

/// `use vmath::f64;` must not break the primitive type or its associated items.
#[test]
fn module_named_like_primitive() {
    use crate::f64;
    let x: f64 = 2.0;
    assert_eq!(f64::sqrt(x * x), 2.0);
    assert_eq!(f64::MAX, 1.797_693_134_862_315_7e308);
    assert_eq!(f64::from_bits(x.to_bits()), 2.0);
    assert_eq!(f64::consts::PI, core::f64::consts::PI);
}
