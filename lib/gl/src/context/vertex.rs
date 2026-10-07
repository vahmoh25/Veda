//! Vertex arrays and generic vertex attributes (OpenGL ES 3.0 sections 2.8
//! and 2.11).

use super::{AttribArray, Context, Current, VERTEX_ATTRIBS, VertexArray};
use crate::backend::AttribType;
use crate::gl;

/// The type of `VertexAttribPointer` data.
pub(crate) fn attrib_type(ty: u32) -> Option<AttribType> {
    Some(match ty {
        gl::BYTE => AttribType::Byte,
        gl::UNSIGNED_BYTE => AttribType::UnsignedByte,
        gl::SHORT => AttribType::Short,
        gl::UNSIGNED_SHORT => AttribType::UnsignedShort,
        gl::INT => AttribType::Int,
        gl::UNSIGNED_INT => AttribType::UnsignedInt,
        gl::HALF_FLOAT => AttribType::HalfFloat,
        gl::FLOAT => AttribType::Float,
        gl::FIXED => AttribType::Fixed,
        gl::INT_2_10_10_10_REV => AttribType::Int2101010Rev,
        gl::UNSIGNED_INT_2_10_10_10_REV => AttribType::UnsignedInt2101010Rev,
        _ => return None,
    })
}

impl AttribArray {
    /// Bytes from one element to the next.
    pub fn effective_stride(&self) -> u32 {
        if self.stride != 0 {
            return self.stride;
        }
        let t = attrib_type(self.ty).unwrap_or(AttribType::Float);
        if t.packed() { 4 } else { t.bytes() * u32::from(self.size) }
    }
}

impl Context {
    /// `glGenVertexArrays`.
    pub fn gen_vertex_arrays(&mut self, names: &mut [u32]) {
        for n in names {
            *n = self.vertex_arrays.generate();
        }
    }

    /// One vertex array name.
    pub fn gen_vertex_array(&mut self) -> u32 {
        self.vertex_arrays.generate()
    }

    /// `glIsVertexArray`.
    pub fn is_vertex_array(&self, name: u32) -> bool {
        self.vertex_arrays.key(name).is_some()
    }

    /// `glBindVertexArray`.
    pub fn bind_vertex_array(&mut self, name: u32) {
        if name == 0 {
            self.vertex_array = None;
            return;
        }
        if !self.vertex_arrays.is_reserved(name) {
            return self.err(gl::INVALID_OPERATION);
        }
        self.vertex_array = Some(self.vertex_arrays.get_or_create(name, VertexArray::default));
    }

    /// `glDeleteVertexArrays`.
    pub fn delete_vertex_arrays(&mut self, names: &[u32]) {
        for &name in names {
            if name == 0 {
                continue;
            }
            let key = self.vertex_arrays.key(name);
            if key.is_some() && self.vertex_array == key {
                self.vertex_array = None;
            }
            if let Some((_, Some(vao))) = self.vertex_arrays.delete(name) {
                self.release_vao_buffers(&vao);
            }
        }
    }

    pub(crate) fn release_vao_buffers(&mut self, vao: &VertexArray) {
        self.release_buffer(vao.element_buffer);
        for a in &vao.attribs {
            self.release_buffer(a.buffer);
        }
    }

    /// `glVertexAttribPointer` (`offset` into the bound `ARRAY_BUFFER`).
    pub fn vertex_attrib_pointer(
        &mut self,
        index: u32,
        size: i32,
        ty: u32,
        normalized: bool,
        stride: i32,
        offset: usize,
    ) {
        self.attrib_pointer(index, size, ty, normalized, false, stride, offset);
    }

    /// `glVertexAttribIPointer`.
    pub fn vertex_attrib_i_pointer(&mut self, index: u32, size: i32, ty: u32, stride: i32, offset: usize) {
        self.attrib_pointer(index, size, ty, false, true, stride, offset);
    }

    fn attrib_pointer(
        &mut self,
        index: u32,
        size: i32,
        ty: u32,
        normalized: bool,
        integer: bool,
        stride: i32,
        offset: usize,
    ) {
        if index as usize >= VERTEX_ATTRIBS || !(1..=4).contains(&size) || stride < 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let Some(t) = attrib_type(ty) else { return self.err(gl::INVALID_ENUM) };
        if integer
            && !matches!(ty, gl::BYTE | gl::UNSIGNED_BYTE | gl::SHORT | gl::UNSIGNED_SHORT | gl::INT | gl::UNSIGNED_INT)
        {
            return self.err(gl::INVALID_ENUM);
        }
        if t.packed() && size != 4 {
            return self.err(gl::INVALID_OPERATION);
        }
        // There are no client-side arrays: without a buffer, the offset can
        // only be 0 (and the array cannot be drawn from while enabled).
        let buffer = self.bound.array;
        if buffer.is_none() && offset != 0 {
            return self.err(gl::INVALID_OPERATION);
        }
        if let Some(k) = buffer {
            self.buffers.retain(k);
        }
        let a = &mut self.vao_mut().attribs[index as usize];
        let old = a.buffer;
        *a = AttribArray {
            enabled: a.enabled,
            size: size as u8,
            ty,
            normalized: normalized && !integer,
            integer,
            stride: stride as u32,
            offset,
            divisor: a.divisor,
            buffer,
        };
        self.release_buffer(old);
    }

    /// `glEnableVertexAttribArray`.
    pub fn enable_vertex_attrib_array(&mut self, index: u32) {
        self.set_attrib_enabled(index, true);
    }

    /// `glDisableVertexAttribArray`.
    pub fn disable_vertex_attrib_array(&mut self, index: u32) {
        self.set_attrib_enabled(index, false);
    }

    fn set_attrib_enabled(&mut self, index: u32, on: bool) {
        if index as usize >= VERTEX_ATTRIBS {
            return self.err(gl::INVALID_VALUE);
        }
        self.vao_mut().attribs[index as usize].enabled = on;
    }

    /// `glVertexAttribDivisor`.
    pub fn vertex_attrib_divisor(&mut self, index: u32, divisor: u32) {
        if index as usize >= VERTEX_ATTRIBS {
            return self.err(gl::INVALID_VALUE);
        }
        self.vao_mut().attribs[index as usize].divisor = divisor;
    }

    fn set_current(&mut self, index: u32, v: Current) {
        if index as usize >= VERTEX_ATTRIBS {
            return self.err(gl::INVALID_VALUE);
        }
        self.current_attribs[index as usize] = v;
    }

    /// `glVertexAttrib1f`.
    pub fn vertex_attrib1f(&mut self, index: u32, x: f32) {
        self.set_current(index, Current::Float([x, 0.0, 0.0, 1.0]));
    }

    /// `glVertexAttrib2f`.
    pub fn vertex_attrib2f(&mut self, index: u32, x: f32, y: f32) {
        self.set_current(index, Current::Float([x, y, 0.0, 1.0]));
    }

    /// `glVertexAttrib3f`.
    pub fn vertex_attrib3f(&mut self, index: u32, x: f32, y: f32, z: f32) {
        self.set_current(index, Current::Float([x, y, z, 1.0]));
    }

    /// `glVertexAttrib4f`.
    pub fn vertex_attrib4f(&mut self, index: u32, x: f32, y: f32, z: f32, w: f32) {
        self.set_current(index, Current::Float([x, y, z, w]));
    }

    /// `glVertexAttrib{1,2,3,4}fv`: as many components as `v` has (1 to 4).
    pub fn vertex_attribfv(&mut self, index: u32, v: &[f32]) {
        if v.is_empty() || v.len() > 4 {
            return self.err(gl::INVALID_VALUE);
        }
        let mut c = [0.0, 0.0, 0.0, 1.0];
        c[..v.len()].copy_from_slice(v);
        self.set_current(index, Current::Float(c));
    }

    /// `glVertexAttribI4i`.
    pub fn vertex_attrib_i4i(&mut self, index: u32, x: i32, y: i32, z: i32, w: i32) {
        self.set_current(index, Current::Int([x, y, z, w]));
    }

    /// `glVertexAttribI4ui`.
    pub fn vertex_attrib_i4ui(&mut self, index: u32, x: u32, y: u32, z: u32, w: u32) {
        self.set_current(index, Current::Uint([x, y, z, w]));
    }

    /// `glVertexAttribI4iv`.
    pub fn vertex_attrib_i4iv(&mut self, index: u32, v: &[i32; 4]) {
        self.set_current(index, Current::Int(*v));
    }

    /// `glVertexAttribI4uiv`.
    pub fn vertex_attrib_i4uiv(&mut self, index: u32, v: &[u32; 4]) {
        self.set_current(index, Current::Uint(*v));
    }

    /// The state `GetVertexAttrib*` reports, as integers (`None` after an
    /// error).
    fn vertex_attrib_state(&mut self, index: u32, pname: u32) -> Option<i64> {
        if index as usize >= VERTEX_ATTRIBS {
            self.err(gl::INVALID_VALUE);
            return None;
        }
        let a = self.vao().attribs[index as usize];
        Some(match pname {
            gl::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING => i64::from(a.buffer.map_or(0, |k| self.buffers.name(k))),
            gl::VERTEX_ATTRIB_ARRAY_ENABLED => i64::from(a.enabled),
            gl::VERTEX_ATTRIB_ARRAY_SIZE => i64::from(a.size),
            gl::VERTEX_ATTRIB_ARRAY_STRIDE => i64::from(a.stride),
            gl::VERTEX_ATTRIB_ARRAY_TYPE => i64::from(a.ty),
            gl::VERTEX_ATTRIB_ARRAY_NORMALIZED => i64::from(a.normalized),
            gl::VERTEX_ATTRIB_ARRAY_INTEGER => i64::from(a.integer),
            gl::VERTEX_ATTRIB_ARRAY_DIVISOR => i64::from(a.divisor),
            _ => {
                self.err(gl::INVALID_ENUM);
                return None;
            }
        })
    }

    /// `glGetVertexAttribiv`: `CURRENT_VERTEX_ATTRIB` fills four values,
    /// the rest one.
    pub fn get_vertex_attribiv(&mut self, index: u32, pname: u32, out: &mut [i32]) {
        if pname == gl::CURRENT_VERTEX_ATTRIB {
            if let Some(c) = self.current(index) {
                let v: [i32; 4] = match c {
                    Current::Float(f) => f.map(|x| vmath::f32::round(x) as i32),
                    Current::Int(i) => i,
                    Current::Uint(u) => u.map(|x| x as i32),
                };
                fill(out, &v);
            }
            return;
        }
        if let Some(v) = self.vertex_attrib_state(index, pname) {
            fill(out, &[v as i32]);
        }
    }

    /// `glGetVertexAttribfv`.
    pub fn get_vertex_attribfv(&mut self, index: u32, pname: u32, out: &mut [f32]) {
        if pname == gl::CURRENT_VERTEX_ATTRIB {
            if let Some(c) = self.current(index) {
                let v: [f32; 4] = match c {
                    Current::Float(f) => f,
                    Current::Int(i) => i.map(|x| x as f32),
                    Current::Uint(u) => u.map(|x| x as f32),
                };
                fill(out, &v);
            }
            return;
        }
        if let Some(v) = self.vertex_attrib_state(index, pname) {
            fill(out, &[v as f32]);
        }
    }

    /// `glGetVertexAttribIiv`.
    pub fn get_vertex_attrib_iiv(&mut self, index: u32, pname: u32, out: &mut [i32]) {
        if pname == gl::CURRENT_VERTEX_ATTRIB {
            if let Some(c) = self.current(index) {
                fill(out, &c.bits().map(|x| x as i32));
            }
            return;
        }
        self.get_vertex_attribiv(index, pname, out);
    }

    /// `glGetVertexAttribIuiv`.
    pub fn get_vertex_attrib_iuiv(&mut self, index: u32, pname: u32, out: &mut [u32]) {
        if pname == gl::CURRENT_VERTEX_ATTRIB {
            if let Some(c) = self.current(index) {
                fill(out, &c.bits());
            }
            return;
        }
        if let Some(v) = self.vertex_attrib_state(index, pname) {
            fill(out, &[v as u32]);
        }
    }

    /// `glGetVertexAttribPointerv`: the array's offset.
    pub fn get_vertex_attrib_pointerv(&mut self, index: u32, pname: u32) -> usize {
        if index as usize >= VERTEX_ATTRIBS {
            self.err(gl::INVALID_VALUE);
            return 0;
        }
        if pname != gl::VERTEX_ATTRIB_ARRAY_POINTER {
            self.err(gl::INVALID_ENUM);
            return 0;
        }
        self.vao().attribs[index as usize].offset
    }

    fn current(&mut self, index: u32) -> Option<Current> {
        if index as usize >= VERTEX_ATTRIBS {
            self.err(gl::INVALID_VALUE);
            return None;
        }
        Some(self.current_attribs[index as usize])
    }
}

/// Copies as many of `v` as fit into `out`.
pub(crate) fn fill<T: Copy>(out: &mut [T], v: &[T]) {
    let n = out.len().min(v.len());
    out[..n].copy_from_slice(&v[..n]);
}
