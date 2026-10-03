//! Bounds-checked big-endian readers for font data.
//!
//! All reads return `None` instead of panicking when the data is too short; callers map that to a
//! [`FontError`](crate::FontError).

/// Reads a `u8` at `off`.
#[inline]
pub(crate) fn u8_at(d: &[u8], off: usize) -> Option<u8> {
    d.get(off).copied()
}

/// Reads a big-endian `u16` at `off`.
#[inline]
pub(crate) fn u16_at(d: &[u8], off: usize) -> Option<u16> {
    let b = d.get(off..off.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

/// Reads a big-endian `i16` at `off`.
#[inline]
pub(crate) fn i16_at(d: &[u8], off: usize) -> Option<i16> {
    u16_at(d, off).map(|v| v as i16)
}

/// Reads a big-endian 24-bit unsigned integer at `off`.
#[inline]
pub(crate) fn u24_at(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off.checked_add(3)?)?;
    Some(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32)
}

/// Reads a big-endian `u32` at `off`.
#[inline]
pub(crate) fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Reads a 4-byte tag at `off`.
#[inline]
pub(crate) fn tag_at(d: &[u8], off: usize) -> Option<[u8; 4]> {
    let b = d.get(off..off.checked_add(4)?)?;
    Some([b[0], b[1], b[2], b[3]])
}

/// A sequential reader over a byte slice.
#[derive(Clone, Copy)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader starting at `pos`.
    #[inline]
    pub(crate) fn at(data: &'a [u8], pos: usize) -> Self {
        Reader { data, pos }
    }

    /// The current position.
    #[inline]
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    /// Advances by `n` bytes.
    #[inline]
    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        let p = self.pos.checked_add(n)?;
        if p > self.data.len() {
            return None;
        }
        self.pos = p;
        Some(())
    }

    #[inline]
    pub(crate) fn u8(&mut self) -> Option<u8> {
        let v = u8_at(self.data, self.pos)?;
        self.pos += 1;
        Some(v)
    }

    #[inline]
    pub(crate) fn i8(&mut self) -> Option<i8> {
        self.u8().map(|v| v as i8)
    }

    #[inline]
    pub(crate) fn u16(&mut self) -> Option<u16> {
        let v = u16_at(self.data, self.pos)?;
        self.pos += 2;
        Some(v)
    }

    #[inline]
    pub(crate) fn i16(&mut self) -> Option<i16> {
        self.u16().map(|v| v as i16)
    }

    #[inline]
    pub(crate) fn u32(&mut self) -> Option<u32> {
        let v = u32_at(self.data, self.pos)?;
        self.pos += 4;
        Some(v)
    }

    /// Reads an F2Dot14 fixed-point number.
    #[inline]
    pub(crate) fn f2dot14(&mut self) -> Option<f32> {
        self.i16().map(|v| v as f32 / 16384.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_are_bounds_checked() {
        let d = [0x12, 0x34, 0x56, 0x78, 0x9A];
        assert_eq!(u16_at(&d, 0), Some(0x1234));
        assert_eq!(u16_at(&d, 4), None);
        assert_eq!(u16_at(&d, usize::MAX), None);
        assert_eq!(u24_at(&d, 2), Some(0x56789A));
        assert_eq!(u32_at(&d, 1), Some(0x3456_789A));
        assert_eq!(i16_at(&[0xFF, 0xFE], 0), Some(-2));
        let mut r = Reader::at(&d, 3);
        assert_eq!(r.u16(), Some(0x789A));
        assert_eq!(r.u8(), None);
        assert!(r.skip(1).is_none());
        let mut r = Reader::at(&[0x40, 0x00, 0xC0, 0x00], 0);
        assert_eq!(r.f2dot14(), Some(1.0));
        assert_eq!(r.f2dot14(), Some(-1.0));
    }
}
