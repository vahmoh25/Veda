//! 8x8 integer DCTs: the "islow" inverse transform of the Independent JPEG Group's libjpeg
//! (`jidctint.c`) and the matching forward transform (`jfdctint.c`).
//!
//! Both are the Loeffler-Ligtenberg-Moschytz algorithm with 13-bit fixed-point constants and two
//! extra bits of precision between the passes, giving results identical to libjpeg's default
//! decoder. The inverse transform does its arithmetic in 64 bits, so corrupt coefficients can
//! never overflow, and it short-circuits columns and rows whose AC terms are all zero (most of
//! them in typical photos).

const CONST_BITS: u32 = 13;
const PASS1_BITS: u32 = 2;

const FIX_0_298631336: i64 = 2446;
const FIX_0_390180644: i64 = 3196;
const FIX_0_541196100: i64 = 4433;
const FIX_0_765366865: i64 = 6270;
const FIX_0_899976223: i64 = 7373;
const FIX_1_175875602: i64 = 9633;
const FIX_1_501321110: i64 = 12299;
const FIX_1_847759065: i64 = 15137;
const FIX_1_961570560: i64 = 16069;
const FIX_2_053119869: i64 = 16819;
const FIX_2_562915447: i64 = 20995;
const FIX_3_072711026: i64 = 25172;

#[inline(always)]
fn descale(x: i64, n: u32) -> i64 {
    (x + (1 << (n - 1))) >> n
}

#[inline(always)]
fn clamp_sample(x: i64) -> u8 {
    (x + 128).clamp(0, 255) as u8
}

/// Writes the 8x8 block of a coefficient block that only has a DC term.
#[inline]
pub(crate) fn idct_dc(dc: i32, out: &mut [u8], stride: usize) {
    let v = clamp_sample((dc as i64 + 4) >> 3);
    for row in out.chunks_mut(stride).take(8) {
        row[..8].fill(v);
    }
}

/// Inverse DCT of dequantized coefficients (natural order) into an 8x8 block of samples at the
/// start of `out` with row pitch `stride`.
pub(crate) fn idct(coef: &[i32; 64], out: &mut [u8], stride: usize) {
    let mut ws = [0i64; 64];
    // Pass 1: columns, results scaled up by sqrt(8) * 2^PASS1_BITS.
    for col in 0..8 {
        let c = |r: usize| coef[r * 8 + col] as i64;
        if c(1) == 0 && c(2) == 0 && c(3) == 0 && c(4) == 0 && c(5) == 0 && c(6) == 0 && c(7) == 0 {
            let dc = c(0) << PASS1_BITS;
            for r in 0..8 {
                ws[r * 8 + col] = dc;
            }
            continue;
        }
        let (z2, z3) = (c(2), c(6));
        let z1 = (z2 + z3) * FIX_0_541196100;
        let tmp2 = z1 - z3 * FIX_1_847759065;
        let tmp3 = z1 + z2 * FIX_0_765366865;
        let (z2, z3) = (c(0), c(4));
        let tmp0 = (z2 + z3) << CONST_BITS;
        let tmp1 = (z2 - z3) << CONST_BITS;
        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;

        let (mut t0, mut t1, mut t2, mut t3) = (c(7), c(5), c(3), c(1));
        let z1 = t0 + t3;
        let z2 = t1 + t2;
        let z3 = t0 + t2;
        let z4 = t1 + t3;
        let z5 = (z3 + z4) * FIX_1_175875602;
        t0 *= FIX_0_298631336;
        t1 *= FIX_2_053119869;
        t2 *= FIX_3_072711026;
        t3 *= FIX_1_501321110;
        let z1 = -z1 * FIX_0_899976223;
        let z2 = -z2 * FIX_2_562915447;
        let z3 = -z3 * FIX_1_961570560 + z5;
        let z4 = -z4 * FIX_0_390180644 + z5;
        t0 += z1 + z3;
        t1 += z2 + z4;
        t2 += z2 + z3;
        t3 += z1 + z4;

        let n = CONST_BITS - PASS1_BITS;
        ws[col] = descale(tmp10 + t3, n);
        ws[7 * 8 + col] = descale(tmp10 - t3, n);
        ws[8 + col] = descale(tmp11 + t2, n);
        ws[6 * 8 + col] = descale(tmp11 - t2, n);
        ws[2 * 8 + col] = descale(tmp12 + t1, n);
        ws[5 * 8 + col] = descale(tmp12 - t1, n);
        ws[3 * 8 + col] = descale(tmp13 + t0, n);
        ws[4 * 8 + col] = descale(tmp13 - t0, n);
    }

    // Pass 2: rows, removing the PASS1_BITS scaling and the factor 8.
    for (w, o) in ws.as_chunks::<8>().0.iter().zip(out.chunks_mut(stride)) {
        let o = &mut o[..8];
        if w[1] == 0 && w[2] == 0 && w[3] == 0 && w[4] == 0 && w[5] == 0 && w[6] == 0 && w[7] == 0 {
            o.fill(clamp_sample(descale(w[0], PASS1_BITS + 3)));
            continue;
        }
        let (z2, z3) = (w[2], w[6]);
        let z1 = (z2 + z3) * FIX_0_541196100;
        let tmp2 = z1 - z3 * FIX_1_847759065;
        let tmp3 = z1 + z2 * FIX_0_765366865;
        let tmp0 = (w[0] + w[4]) << CONST_BITS;
        let tmp1 = (w[0] - w[4]) << CONST_BITS;
        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;

        let (mut t0, mut t1, mut t2, mut t3) = (w[7], w[5], w[3], w[1]);
        let z1 = t0 + t3;
        let z2 = t1 + t2;
        let z3 = t0 + t2;
        let z4 = t1 + t3;
        let z5 = (z3 + z4) * FIX_1_175875602;
        t0 *= FIX_0_298631336;
        t1 *= FIX_2_053119869;
        t2 *= FIX_3_072711026;
        t3 *= FIX_1_501321110;
        let z1 = -z1 * FIX_0_899976223;
        let z2 = -z2 * FIX_2_562915447;
        let z3 = -z3 * FIX_1_961570560 + z5;
        let z4 = -z4 * FIX_0_390180644 + z5;
        t0 += z1 + z3;
        t1 += z2 + z4;
        t2 += z2 + z3;
        t3 += z1 + z4;

        let n = CONST_BITS + PASS1_BITS + 3;
        o[0] = clamp_sample(descale(tmp10 + t3, n));
        o[7] = clamp_sample(descale(tmp10 - t3, n));
        o[1] = clamp_sample(descale(tmp11 + t2, n));
        o[6] = clamp_sample(descale(tmp11 - t2, n));
        o[2] = clamp_sample(descale(tmp12 + t1, n));
        o[5] = clamp_sample(descale(tmp12 - t1, n));
        o[3] = clamp_sample(descale(tmp13 + t0, n));
        o[4] = clamp_sample(descale(tmp13 - t0, n));
    }
}

/// Forward DCT of 8x8 level-shifted samples (`sample - 128`, natural order) in place. The output
/// is scaled up by 8 (quantize by dividing by `8 * q`).
pub(crate) fn fdct(data: &mut [i32; 64]) {
    // Pass 1: rows, results scaled up by sqrt(8) * 2^PASS1_BITS.
    for row in data.as_chunks_mut::<8>().0 {
        let tmp0 = row[0] + row[7];
        let tmp7 = row[0] - row[7];
        let tmp1 = row[1] + row[6];
        let tmp6 = row[1] - row[6];
        let tmp2 = row[2] + row[5];
        let tmp5 = row[2] - row[5];
        let tmp3 = row[3] + row[4];
        let tmp4 = row[3] - row[4];

        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;
        row[0] = (tmp10 + tmp11) << PASS1_BITS;
        row[4] = (tmp10 - tmp11) << PASS1_BITS;
        let z1 = (tmp12 + tmp13) * FIX_0_541196100 as i32;
        let n = CONST_BITS - PASS1_BITS;
        row[2] = descale32(z1 + tmp13 * FIX_0_765366865 as i32, n);
        row[6] = descale32(z1 - tmp12 * FIX_1_847759065 as i32, n);

        let (r7, r5, r3, r1) = fdct_odd(tmp4, tmp5, tmp6, tmp7);
        row[7] = descale32(r7, n);
        row[5] = descale32(r5, n);
        row[3] = descale32(r3, n);
        row[1] = descale32(r1, n);
    }
    // Pass 2: columns, removing the PASS1_BITS scaling (output stays scaled by 8).
    for col in 0..8 {
        let d = |r: usize| data[r * 8 + col];
        let tmp0 = d(0) + d(7);
        let tmp7 = d(0) - d(7);
        let tmp1 = d(1) + d(6);
        let tmp6 = d(1) - d(6);
        let tmp2 = d(2) + d(5);
        let tmp5 = d(2) - d(5);
        let tmp3 = d(3) + d(4);
        let tmp4 = d(3) - d(4);

        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;
        data[col] = descale32(tmp10 + tmp11, PASS1_BITS);
        data[4 * 8 + col] = descale32(tmp10 - tmp11, PASS1_BITS);
        let z1 = (tmp12 + tmp13) * FIX_0_541196100 as i32;
        let n = CONST_BITS + PASS1_BITS;
        data[2 * 8 + col] = descale32(z1 + tmp13 * FIX_0_765366865 as i32, n);
        data[6 * 8 + col] = descale32(z1 - tmp12 * FIX_1_847759065 as i32, n);

        let (r7, r5, r3, r1) = fdct_odd(tmp4, tmp5, tmp6, tmp7);
        data[7 * 8 + col] = descale32(r7, n);
        data[5 * 8 + col] = descale32(r5, n);
        data[3 * 8 + col] = descale32(r3, n);
        data[8 + col] = descale32(r1, n);
    }
}

#[inline(always)]
fn descale32(x: i32, n: u32) -> i32 {
    (x + (1 << (n - 1))) >> n
}

/// Odd part of the forward DCT; returns the undescaled outputs 7, 5, 3, 1.
#[inline(always)]
fn fdct_odd(tmp4: i32, tmp5: i32, tmp6: i32, tmp7: i32) -> (i32, i32, i32, i32) {
    let z1 = tmp4 + tmp7;
    let z2 = tmp5 + tmp6;
    let z3 = tmp4 + tmp6;
    let z4 = tmp5 + tmp7;
    let z5 = (z3 + z4) * FIX_1_175875602 as i32;
    let tmp4 = tmp4 * FIX_0_298631336 as i32;
    let tmp5 = tmp5 * FIX_2_053119869 as i32;
    let tmp6 = tmp6 * FIX_3_072711026 as i32;
    let tmp7 = tmp7 * FIX_1_501321110 as i32;
    let z1 = -z1 * FIX_0_899976223 as i32;
    let z2 = -z2 * FIX_2_562915447 as i32;
    let z3 = -z3 * FIX_1_961570560 as i32 + z5;
    let z4 = -z4 * FIX_0_390180644 as i32 + z5;
    (tmp4 + z1 + z3, tmp5 + z2 + z4, tmp6 + z2 + z3, tmp7 + z1 + z4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference floating-point DCTs.
    fn cos_table() -> [[f64; 8]; 8] {
        let mut t = [[0.0; 8]; 8];
        for (u, row) in t.iter_mut().enumerate() {
            for (x, v) in row.iter_mut().enumerate() {
                // cos((2x+1) u pi / 16) via the crate's sin helper: cos(a) = sin(a + pi/2).
                *v = crate::mathf::sin_pi(((2 * x + 1) * u) as f64 / 16.0 + 0.5);
            }
        }
        t
    }

    #[test]
    fn matches_float_reference() {
        let cos = cos_table();
        let c = |u: usize| if u == 0 { core::f64::consts::FRAC_1_SQRT_2 } else { 1.0 };
        let mut seed = 12345u32;
        for trial in 0..200 {
            let mut samples = [0i32; 64];
            for (i, s) in samples.iter_mut().enumerate() {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *s = if trial % 4 == 0 { ((i % 8) * 30) as i32 - 128 } else { (seed % 256) as i32 - 128 };
            }
            // Forward: integer FDCT (scaled by 8) against the float DCT.
            let mut f = samples;
            fdct(&mut f);
            for v in 0..8 {
                for u in 0..8 {
                    let mut sum = 0.0;
                    for y in 0..8 {
                        for x in 0..8 {
                            sum += samples[y * 8 + x] as f64 * cos[u][x] * cos[v][y];
                        }
                    }
                    let want = 0.25 * c(u) * c(v) * sum * 8.0;
                    assert!((f[v * 8 + u] as f64 - want).abs() < 8.0, "fdct {u},{v}: {} vs {want}", f[v * 8 + u]);
                }
            }
            // Inverse: IDCT of the rounded (unscaled) coefficients reproduces the samples.
            let coef: [i32; 64] = core::array::from_fn(|i| (f[i] + 4) >> 3);
            let mut out = [0u8; 64];
            idct(&coef, &mut out, 8);
            for i in 0..64 {
                let want = samples[i] + 128;
                assert!((out[i] as i32 - want).abs() <= 2, "idct sample {i}: {} vs {want}", out[i]);
            }
        }
        // DC-only shortcut agrees with the full transform.
        for dc in [-1024, -517, -3, 0, 5, 100, 1016] {
            let mut coef = [0i32; 64];
            coef[0] = dc;
            let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
            idct(&coef, &mut a, 8);
            idct_dc(dc, &mut b, 8);
            assert_eq!(a, b);
        }
        // Extreme coefficients do not overflow.
        let wild = [i32::MAX; 64];
        let mut out = [0u8; 64];
        idct(&wild, &mut out, 8);
        idct(&[i32::MIN + 1; 64], &mut out, 8);
    }
}
