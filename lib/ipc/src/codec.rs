//! The message codec.
//!
//! Values are encoded little-endian in declaration order. Strings and
//! sequences are length-prefixed (`u32`). Handles travel in the channel
//! message's handle list; the byte stream records their index so a decoder
//! can validate the pairing.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use vrt::object::{Channel, Event, Handle, Interrupt, IoPorts, Process, Resource, Thread, Vmo};

/// Decoding failure (malformed or hostile message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadUtf8,
    BadTag(u32),
    MissingHandle,
    TooLarge,
    TrailingData,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => f.write_str("message truncated"),
            DecodeError::BadUtf8 => f.write_str("invalid UTF-8 string"),
            DecodeError::BadTag(t) => write!(f, "invalid tag {t}"),
            DecodeError::MissingHandle => f.write_str("missing handle"),
            DecodeError::TooLarge => f.write_str("length exceeds message"),
            DecodeError::TrailingData => f.write_str("trailing bytes"),
        }
    }
}

/// Builds the bytes and handles of one message.
#[derive(Default)]
pub struct Encoder {
    pub bytes: Vec<u8>,
    pub handles: Vec<Handle>,
}

impl Encoder {
    pub fn new() -> Encoder {
        Encoder { bytes: Vec::with_capacity(64), handles: Vec::new() }
    }

    #[inline]
    pub fn put_bytes(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
    }

    #[inline]
    pub fn put_u32(&mut self, v: u32) {
        self.put_bytes(&v.to_le_bytes());
    }

    #[inline]
    pub fn put_len(&mut self, n: usize) {
        self.put_u32(n as u32);
    }

    pub fn put_handle(&mut self, h: Handle) {
        let index = self.handles.len() as u32;
        self.put_u32(index);
        self.handles.push(h);
    }
}

/// Reads values back out of a message.
pub struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
    handles: Vec<Option<Handle>>,
}

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8], handles: Vec<Handle>) -> Decoder<'a> {
        Decoder { bytes, pos: 0, handles: handles.into_iter().map(Some).collect() }
    }

    #[inline]
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::TooLarge)?;
        let s = self.bytes.get(self.pos..end).ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    #[inline]
    pub fn get_u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a length prefix, rejecting lengths that cannot possibly fit
    /// (each element needs at least `min_elem` bytes).
    pub fn get_len(&mut self, min_elem: usize) -> Result<usize, DecodeError> {
        let n = self.get_u32()? as usize;
        if n.saturating_mul(min_elem) > self.remaining() {
            return Err(DecodeError::TooLarge);
        }
        Ok(n)
    }

    pub fn take_handle(&mut self) -> Result<Handle, DecodeError> {
        let i = self.get_u32()? as usize;
        self.handles.get_mut(i).and_then(Option::take).ok_or(DecodeError::MissingHandle)
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Fails if unread bytes remain (strict decoding of requests).
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.remaining() == 0 { Ok(()) } else { Err(DecodeError::TrailingData) }
    }
}

/// Types that can be written into a message (by value: handles move).
pub trait Encode {
    fn encode(self, e: &mut Encoder);
}

/// Types that can be read from a message.
pub trait Decode: Sized {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError>;
}

macro_rules! num {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            #[inline]
            fn encode(self, e: &mut Encoder) {
                e.put_bytes(&self.to_le_bytes());
            }
        }
        impl Decode for $t {
            #[inline]
            fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
                let b = d.take(core::mem::size_of::<$t>())?;
                Ok(<$t>::from_le_bytes(b.try_into().unwrap()))
            }
        }
    )*};
}

num!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

impl Encode for usize {
    fn encode(self, e: &mut Encoder) {
        (self as u64).encode(e)
    }
}

impl Decode for usize {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        Ok(u64::decode(d)? as usize)
    }
}

impl Encode for bool {
    fn encode(self, e: &mut Encoder) {
        (self as u8).encode(e)
    }
}

impl Decode for bool {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        match u8::decode(d)? {
            0 => Ok(false),
            1 => Ok(true),
            t => Err(DecodeError::BadTag(t as u32)),
        }
    }
}

impl Encode for () {
    fn encode(self, _: &mut Encoder) {}
}

impl Decode for () {
    fn decode(_: &mut Decoder) -> Result<Self, DecodeError> {
        Ok(())
    }
}

impl Encode for &str {
    fn encode(self, e: &mut Encoder) {
        e.put_len(self.len());
        e.put_bytes(self.as_bytes());
    }
}

impl Encode for String {
    fn encode(self, e: &mut Encoder) {
        self.as_str().encode(e)
    }
}

impl Encode for &String {
    fn encode(self, e: &mut Encoder) {
        self.as_str().encode(e)
    }
}

impl Decode for String {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        let n = d.get_len(1)?;
        let b = d.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| DecodeError::BadUtf8)
    }
}

/// A byte buffer, encoded in bulk (prefer this over `Vec<u8>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Encode for Bytes {
    fn encode(self, e: &mut Encoder) {
        e.put_len(self.0.len());
        e.put_bytes(&self.0);
    }
}

impl Encode for &[u8] {
    fn encode(self, e: &mut Encoder) {
        e.put_len(self.len());
        e.put_bytes(self);
    }
}

impl Decode for Bytes {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        let n = d.get_len(1)?;
        Ok(Bytes(d.take(n)?.to_vec()))
    }
}

impl<T: Encode> Encode for Vec<T> {
    fn encode(self, e: &mut Encoder) {
        e.put_len(self.len());
        for v in self {
            v.encode(e);
        }
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        let n = d.get_len(1)?;
        let mut v = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            v.push(T::decode(d)?);
        }
        Ok(v)
    }
}

impl<T: Encode> Encode for Option<T> {
    fn encode(self, e: &mut Encoder) {
        match self {
            None => 0u8.encode(e),
            Some(v) => {
                1u8.encode(e);
                v.encode(e);
            }
        }
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        match u8::decode(d)? {
            0 => Ok(None),
            1 => Ok(Some(T::decode(d)?)),
            t => Err(DecodeError::BadTag(t as u32)),
        }
    }
}

impl<T: Encode, E: Encode> Encode for Result<T, E> {
    fn encode(self, e: &mut Encoder) {
        match self {
            Ok(v) => {
                0u8.encode(e);
                v.encode(e);
            }
            Err(err) => {
                1u8.encode(e);
                err.encode(e);
            }
        }
    }
}

impl<T: Decode, E: Decode> Decode for Result<T, E> {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        match u8::decode(d)? {
            0 => Ok(Ok(T::decode(d)?)),
            1 => Ok(Err(E::decode(d)?)),
            t => Err(DecodeError::BadTag(t as u32)),
        }
    }
}

impl<T: Encode> Encode for Box<T> {
    fn encode(self, e: &mut Encoder) {
        (*self).encode(e)
    }
}

impl<T: Decode> Decode for Box<T> {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        Ok(Box::new(T::decode(d)?))
    }
}

macro_rules! tuple {
    ($($n:ident),+) => {
        impl<$($n: Encode),+> Encode for ($($n,)+) {
            #[allow(non_snake_case)]
            fn encode(self, e: &mut Encoder) {
                let ($($n,)+) = self;
                $($n.encode(e);)+
            }
        }
        impl<$($n: Decode),+> Decode for ($($n,)+) {
            fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
                Ok(($($n::decode(d)?,)+))
            }
        }
    };
}

tuple!(A);
tuple!(A, B);
tuple!(A, B, C);
tuple!(A, B, C, D);
tuple!(A, B, C, D, F);

impl<T: Encode + Copy, const N: usize> Encode for [T; N] {
    fn encode(self, e: &mut Encoder) {
        for v in self {
            v.encode(e);
        }
    }
}

impl<T: Decode + Copy + Default, const N: usize> Decode for [T; N] {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        let mut out = [T::default(); N];
        for slot in out.iter_mut() {
            *slot = T::decode(d)?;
        }
        Ok(out)
    }
}

impl Encode for Handle {
    fn encode(self, e: &mut Encoder) {
        e.put_handle(self)
    }
}

impl Decode for Handle {
    fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
        d.take_handle()
    }
}

macro_rules! handle_type {
    ($($t:ident),*) => {$(
        impl Encode for $t {
            fn encode(self, e: &mut Encoder) {
                e.put_handle(self.into_handle())
            }
        }
        impl Decode for $t {
            fn decode(d: &mut Decoder) -> Result<Self, DecodeError> {
                Ok($t::from_handle(d.take_handle()?))
            }
        }
    )*};
}

handle_type!(Channel, Vmo, Event, Process, Thread, Interrupt, IoPorts, Resource);

/// Declares a struct whose fields are encoded in order.
///
/// ```ignore
/// vipc::message! {
///     #[derive(Debug, Clone)]
///     pub struct FileInfo { pub name: String, pub size: u64 }
/// }
/// ```
#[macro_export]
macro_rules! message {
    ($(#[$meta:meta])* $vis:vis struct $name:ident { $($(#[$fmeta:meta])* $fvis:vis $field:ident : $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        $vis struct $name { $($(#[$fmeta])* $fvis $field: $ty),* }

        impl $crate::Encode for $name {
            fn encode(self, e: &mut $crate::Encoder) {
                $( $crate::Encode::encode(self.$field, e); )*
            }
        }

        impl $crate::Decode for $name {
            fn decode(d: &mut $crate::Decoder) -> Result<Self, $crate::DecodeError> {
                Ok($name { $( $field: <$ty as $crate::Decode>::decode(d)? ),* })
            }
        }
    };
}

/// Declares a C-like enum encoded as `u32`.
///
/// ```ignore
/// vipc::enumeration! {
///     #[derive(Debug, Clone, Copy, PartialEq, Eq)]
///     pub enum FsError { NotFound = 1, AccessDenied = 2 }
/// }
/// ```
#[macro_export]
macro_rules! enumeration {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($(#[$vmeta:meta])* $variant:ident = $value:literal),* $(,)? }) => {
        $(#[$meta])*
        #[repr(u32)]
        $vis enum $name { $($(#[$vmeta])* $variant = $value),* }

        impl $crate::Encode for $name {
            fn encode(self, e: &mut $crate::Encoder) {
                $crate::Encode::encode(self as u32, e);
            }
        }

        impl $crate::Decode for $name {
            fn decode(d: &mut $crate::Decoder) -> Result<Self, $crate::DecodeError> {
                match <u32 as $crate::Decode>::decode(d)? {
                    $( $value => Ok($name::$variant), )*
                    t => Err($crate::DecodeError::BadTag(t)),
                }
            }
        }
    };
}

/// Declares an enum with data, encoded as a `u32` tag plus fields.
///
/// ```ignore
/// vipc::union! {
///     #[derive(Debug)]
///     pub enum Event {
///         1 => Resized { width: u32, height: u32 },
///         2 => Closed {},
///     }
/// }
/// ```
#[macro_export]
macro_rules! union {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($(#[$vmeta:meta])* $tag:literal => $variant:ident { $($field:ident : $ty:ty),* $(,)? }),* $(,)? }) => {
        $(#[$meta])*
        $vis enum $name { $($(#[$vmeta])* $variant { $($field: $ty),* }),* }

        impl $crate::Encode for $name {
            #[allow(unused_variables)]
            fn encode(self, e: &mut $crate::Encoder) {
                match self {
                    $( $name::$variant { $($field),* } => {
                        $crate::Encode::encode($tag as u32, e);
                        $( $crate::Encode::encode($field, e); )*
                    } )*
                }
            }
        }

        impl $crate::Decode for $name {
            fn decode(d: &mut $crate::Decoder) -> Result<Self, $crate::DecodeError> {
                match <u32 as $crate::Decode>::decode(d)? {
                    $( $tag => Ok($name::$variant { $( $field: <$ty as $crate::Decode>::decode(d)? ),* }), )*
                    t => Err($crate::DecodeError::BadTag(t)),
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    crate::message! {
        #[derive(Debug, Clone, PartialEq)]
        pub struct Sample { pub id: u32, pub name: String, pub tags: Vec<String>, pub ratio: f32, pub parent: Option<u64> }
    }

    crate::enumeration! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Color { Red = 1, Green = 2 }
    }

    crate::union! {
        #[derive(Debug, PartialEq)]
        pub enum Shape {
            1 => Circle { r: f32 },
            2 => Rect { w: u32, h: u32 },
            3 => Empty {},
        }
    }

    fn roundtrip<T: Encode + Decode>(v: T) -> T {
        let mut e = Encoder::new();
        v.encode(&mut e);
        let mut d = Decoder::new(&e.bytes, Vec::new());
        let out = T::decode(&mut d).unwrap();
        d.finish().unwrap();
        out
    }

    #[test]
    fn primitives_and_containers() {
        assert_eq!(roundtrip(0xDEAD_BEEFu32), 0xDEAD_BEEF);
        assert_eq!(roundtrip(-5i64), -5);
        assert_eq!(roundtrip(1.5f64), 1.5);
        assert!(roundtrip(true));
        assert_eq!(roundtrip("héllo".to_string()), "héllo");
        assert_eq!(roundtrip(vec![1u16, 2, 3]), vec![1, 2, 3]);
        assert_eq!(roundtrip(Some(7u8)), Some(7));
        assert_eq!(roundtrip::<Result<u32, u8>>(Err(3)), Err(3));
        assert_eq!(roundtrip((1u8, "a".to_string(), 2u64)), (1, "a".to_string(), 2));
        assert_eq!(roundtrip(Bytes(vec![9; 1000])), Bytes(vec![9; 1000]));
    }

    #[test]
    fn macros() {
        let s = Sample { id: 4, name: "x".into(), tags: vec!["a".into(), "b".into()], ratio: 0.25, parent: Some(9) };
        assert_eq!(roundtrip(s.clone()), s);
        assert_eq!(roundtrip(Color::Green), Color::Green);
        assert_eq!(roundtrip(Shape::Rect { w: 3, h: 4 }), Shape::Rect { w: 3, h: 4 });
        assert_eq!(roundtrip(Shape::Empty {}), Shape::Empty {});
    }

    #[test]
    fn rejects_malformed_input() {
        let mut d = Decoder::new(&[5, 0, 0, 0, b'a'], Vec::new());
        assert_eq!(String::decode(&mut d), Err(DecodeError::TooLarge));
        let mut d = Decoder::new(&[9, 0, 0, 0], Vec::new());
        assert_eq!(Color::decode(&mut d), Err(DecodeError::BadTag(9)));
        let mut d = Decoder::new(&[0xFF, 0xFF, 0xFF, 0x7F], Vec::new());
        assert!(Vec::<u64>::decode(&mut d).is_err());
        let mut d = Decoder::new(&[0, 0, 0, 0], Vec::new());
        assert_eq!(Handle::decode(&mut d).map(|h| h.into_raw()), Err(DecodeError::MissingHandle));
    }
}
