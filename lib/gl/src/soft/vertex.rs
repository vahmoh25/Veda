//! The vertex stage: attribute fetch (OpenGL ES 3.0 section 2.9) and the
//! vertex shader, sixteen vertices at a time.
//!
//! Attribute reads are bounds checked: an element outside its buffer
//! reads as zero, as robust buffer access would have it.

use vglsl::interp::{Env, Exec, LANES, Lanes};
use vglsl::ops::f16_to_f32;

use super::program::SoftProgram;
use crate::backend::AttribType;

/// One attribute array as the vertex stage reads it.
#[derive(Clone, Copy)]
pub struct Fetch<'a> {
    /// The buffer's bytes (`None`: the array is disabled and the current
    /// value is used).
    pub data: Option<&'a [u8]>,
    pub offset: usize,
    pub stride: usize,
    pub size: u8,
    pub ty: AttribType,
    pub normalized: bool,
    pub integer: bool,
    pub divisor: u32,
    pub current: [u32; 4],
}

impl Fetch<'_> {
    /// Element `element`'s four components (as the shader's 32 bits).
    #[inline]
    pub fn element(&self, element: u32) -> [u32; 4] {
        let Some(data) = self.data else { return self.current };
        let bytes = if self.ty.packed() { 4 } else { self.ty.bytes() as usize * self.size as usize };
        let at = (element as usize).checked_mul(self.stride).and_then(|o| o.checked_add(self.offset));
        let Some(b) = at.and_then(|o| data.get(o..o.checked_add(bytes)?)) else {
            // Outside the buffer: zero.
            return [0; 4];
        };
        decode(b, self.ty, self.size, self.normalized, self.integer)
    }
}

/// Converts one attribute element (section 2.9.1): to floats (normalized
/// or not) for `VertexAttribPointer` arrays, to integers for
/// `VertexAttribIPointer` ones. Missing components are 0, 0, 1.
pub fn decode(b: &[u8], ty: AttribType, size: u8, normalized: bool, integer: bool) -> [u32; 4] {
    let n = size as usize;
    if integer {
        let mut v = [0u32, 0, 0, 1];
        for (i, c) in v.iter_mut().enumerate().take(n) {
            *c = match ty {
                AttribType::Byte => i32::from(b[i] as i8) as u32,
                AttribType::UnsignedByte => u32::from(b[i]),
                AttribType::Short => i32::from(i16::from_le_bytes([b[2 * i], b[2 * i + 1]])) as u32,
                AttribType::UnsignedShort => u32::from(u16::from_le_bytes([b[2 * i], b[2 * i + 1]])),
                _ => u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]),
            };
        }
        return v;
    }
    let mut v = [0.0f32, 0.0, 0.0, 1.0];
    let word = |i: usize| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
    match ty {
        AttribType::Int2101010Rev | AttribType::UnsignedInt2101010Rev => {
            let w = word(0);
            let fields = [w & 0x3FF, (w >> 10) & 0x3FF, (w >> 20) & 0x3FF, w >> 30];
            for (i, c) in v.iter_mut().enumerate() {
                let bits = if i == 3 { 2 } else { 10 };
                *c = if ty == AttribType::Int2101010Rev {
                    let s = ((fields[i] << (32 - bits)) as i32) >> (32 - bits);
                    if normalized { (s as f32 / ((1 << (bits - 1)) - 1) as f32).max(-1.0) } else { s as f32 }
                } else if normalized {
                    fields[i] as f32 / ((1u32 << bits) - 1) as f32
                } else {
                    fields[i] as f32
                };
            }
        }
        _ => {
            for (i, c) in v.iter_mut().enumerate().take(n) {
                *c = match ty {
                    AttribType::Byte => {
                        let x = f32::from(b[i] as i8);
                        if normalized { (x / 127.0).max(-1.0) } else { x }
                    }
                    AttribType::UnsignedByte => {
                        let x = f32::from(b[i]);
                        if normalized { x / 255.0 } else { x }
                    }
                    AttribType::Short => {
                        let x = f32::from(i16::from_le_bytes([b[2 * i], b[2 * i + 1]]));
                        if normalized { (x / 32767.0).max(-1.0) } else { x }
                    }
                    AttribType::UnsignedShort => {
                        let x = f32::from(u16::from_le_bytes([b[2 * i], b[2 * i + 1]]));
                        if normalized { x / 65535.0 } else { x }
                    }
                    AttribType::Int => {
                        let x = word(i) as i32;
                        if normalized { (f64::from(x) / 2_147_483_647.0).max(-1.0) as f32 } else { x as f32 }
                    }
                    AttribType::UnsignedInt => {
                        let x = word(i);
                        if normalized { (f64::from(x) / 4_294_967_295.0) as f32 } else { x as f32 }
                    }
                    AttribType::Fixed => word(i) as i32 as f32 / 65536.0,
                    AttribType::HalfFloat => f16_to_f32(u16::from_le_bytes([b[2 * i], b[2 * i + 1]])),
                    AttribType::Float => f32::from_bits(word(i)),
                    _ => 0.0,
                };
            }
        }
    }
    v.map(f32::to_bits)
}

/// Words per post-transform vertex: position, point size, varyings.
pub fn vertex_words(p: &SoftProgram) -> usize {
    5 + p.varyings.len()
}

/// Runs the vertex shader on `ids` (vertex indices, `gl_VertexID`) of
/// instance `instance`: writes each vertex's position, point size and
/// varyings to `out` (`vertex_words` words each), and its captured
/// transform feedback components to `captured` if given.
#[allow(clippy::too_many_arguments)]
pub fn shade(
    p: &SoftProgram,
    fetch: &[Fetch<'_>],
    env: &Env<'_>,
    exec: &mut Exec,
    ids: &[u32],
    instance: u32,
    out: &mut [u32],
    mut captured: Option<&mut [u32]>,
) {
    let words = vertex_words(p);
    let ncap = p.feedback.len();
    let vs = &p.vs;
    for (batch, chunk) in ids.chunks(LANES).enumerate() {
        let n = chunk.len();
        let mask = if n == LANES { vglsl::interp::ALL } else { (1u32 << n) - 1 };
        // Inputs.
        for &(location, regs) in &p.attribs {
            let f = fetch.get(location as usize).copied();
            let mut values = [[0u32; 4]; LANES];
            if let Some(f) = f {
                for (lane, &id) in chunk.iter().enumerate() {
                    // Per vertex, or per `divisor` instances.
                    let element = instance.checked_div(f.divisor).unwrap_or(id);
                    values[lane] = f.element(element);
                }
            }
            for (c, r) in regs.iter().enumerate() {
                if let Some(r) = r {
                    let reg = &mut exec.regs[*r as usize].0;
                    for lane in 0..n {
                        reg[lane] = values[lane][c];
                    }
                }
            }
        }
        if let Some(r) = p.vertex_id {
            let reg = &mut exec.regs[r as usize].0;
            for (lane, &id) in chunk.iter().enumerate() {
                reg[lane] = id;
            }
        }
        if let Some(r) = p.instance_id {
            exec.regs[r as usize] = Lanes::splat(instance);
        }
        exec.run(vs, mask, env);
        // Outputs.
        let base = batch * LANES;
        let reg = |r: Option<u16>| r.map(|r| exec.regs[r as usize].0);
        let pos = p.position.map(reg);
        let psize = reg(p.point_size);
        for lane in 0..n {
            let o = (base + lane) * words;
            let v = &mut out[o..o + words];
            for c in 0..4 {
                v[c] = pos[c].map_or(if c == 3 { 1.0f32.to_bits() } else { 0 }, |r| r[lane]);
            }
            v[4] = psize.map_or(1.0f32.to_bits(), |r| r[lane]);
        }
        for (k, var) in p.varyings.iter().enumerate() {
            match var.vs {
                Some(r) => {
                    let regv = exec.regs[r as usize].0;
                    for lane in 0..n {
                        out[(base + lane) * words + 5 + k] = regv[lane];
                    }
                }
                None => {
                    for lane in 0..n {
                        out[(base + lane) * words + 5 + k] = 0;
                    }
                }
            }
        }
        if let Some(cap) = captured.as_deref_mut() {
            for (k, c) in p.feedback.iter().enumerate() {
                let values = c.reg.map(|r| exec.regs[r as usize].0);
                for lane in 0..n {
                    cap[(base + lane) * ncap + k] = values.map_or(0, |v| v[lane]);
                }
            }
        }
    }
}
