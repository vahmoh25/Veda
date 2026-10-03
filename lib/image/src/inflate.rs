//! DEFLATE ([RFC 1951]) and zlib ([RFC 1950]) decompression.
//!
//! The decoder is table driven. Every Huffman code is resolved with one lookup in a primary
//! table indexed by the next 10 (literal/length) or 8 (distance) input bits; the rare longer codes
//! chain to a small secondary table. Each table entry carries everything needed to finish the
//! symbol: the number of bits to consume, the literal byte or the length/distance base, and the
//! count of extra bits. Input is read through a 64-bit bit buffer refilled eight bytes at a time,
//! so one refill always covers a complete literal/length + distance pair (at most 48 bits).
//!
//! Output goes into a buffer that is kept [`SLACK`] bytes larger than the data written so far.
//! That lets the hot loop write literals and copy matches in 8-byte chunks without per-byte
//! capacity checks; the buffer is truncated to the real length at the end.
//!
//! Malformed streams are rejected with an [`ImageError`]; truncated input is detected by tracking
//! how many zero bytes were synthesized past the end of the input (at most a few are ever
//! consumed before an error is reported). Output is limited by a caller-supplied byte limit, so a
//! hostile stream cannot exhaust memory.
//!
//! [RFC 1951]: https://www.rfc-editor.org/rfc/rfc1951
//! [RFC 1950]: https://www.rfc-editor.org/rfc/rfc1950

use alloc::vec::Vec;

use crate::checksum::adler32;
use crate::error::ImageError;

/// Bits resolved by the primary literal/length table.
const LITLEN_BITS: u32 = 10;
/// Bits resolved by the primary distance table.
const DIST_BITS: u32 = 8;
/// The code-length code is at most 7 bits long, so its table needs no secondary level.
const PRECODE_BITS: u32 = 7;

/// Extra bytes kept at the end of the output buffer: one maximal match (258 bytes) plus room for
/// the 8-byte chunked copy overshoot.
pub(crate) const SLACK: usize = 258 + 16;

// Decode table entry layout (u32):
//   bits 0..4   number of input bits to consume (code length; the primary width for pointers)
//   bits 4..8   number of extra bits (length/distance) or the secondary table width (pointers)
//   bits 8..12  flags
//   bits 16..32 value: literal byte, length/distance base, code-length symbol or table offset
const NBITS_MASK: u32 = 0xF;
const F_LITERAL: u32 = 1 << 8;
const F_SUBTABLE: u32 = 1 << 9;
const F_EOB: u32 = 1 << 10;
const F_INVALID: u32 = 1 << 11;

/// Base lengths for length symbols 257..=285.
pub(crate) const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
/// Extra bits for length symbols 257..=285.
pub(crate) const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
/// Base distances for distance symbols 0..=29.
pub(crate) const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
/// Extra bits for distance symbols 0..=29.
pub(crate) const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// Order in which code-length code lengths are transmitted.
pub(crate) const PRECODE_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Maximum number of zero bytes synthesized past the end of the input before giving up. A valid
/// stream never consumes them; they only exist so the bit buffer can always be topped up.
const MAX_OVERRUN: usize = 16;

/// LSB-first bit reader over a byte slice with a 64-bit buffer.
struct Bits<'a> {
    input: &'a [u8],
    /// Next input byte to load.
    pos: usize,
    /// Buffered bits; bit 0 is the next bit of the stream. Bits at and above `count` may hold a
    /// prefix of `input[pos]` (left over from a wide refill), which is harmless: the next refill
    /// ORs the same bits into the same place.
    buf: u64,
    /// Number of valid bits in `buf`.
    count: u32,
    /// Number of zero bytes appended past the end of the input.
    overrun: usize,
}

impl<'a> Bits<'a> {
    fn new(input: &'a [u8]) -> Self {
        Bits { input, pos: 0, buf: 0, count: 0, overrun: 0 }
    }

    /// Ensures at least 56 bits are buffered.
    #[inline(always)]
    fn refill(&mut self) -> Result<(), ImageError> {
        if let Some(chunk) = self.input.get(self.pos..self.pos + 8) {
            let Ok(chunk) = <[u8; 8]>::try_from(chunk) else { return self.refill_slow() };
            self.buf |= u64::from_le_bytes(chunk) << self.count;
            self.pos += ((63 - self.count) >> 3) as usize;
            self.count |= 56;
            Ok(())
        } else {
            self.refill_slow()
        }
    }

    /// Byte-at-a-time refill used near the end of the input.
    #[inline(never)]
    fn refill_slow(&mut self) -> Result<(), ImageError> {
        while self.count <= 56 {
            let byte = match self.input.get(self.pos) {
                Some(&b) => {
                    self.pos += 1;
                    b
                }
                None => {
                    self.overrun += 1;
                    0
                }
            };
            self.buf |= (byte as u64) << self.count;
            self.count += 8;
        }
        if self.overrun > MAX_OVERRUN { Err(ImageError::Truncated) } else { Ok(()) }
    }

    /// Removes and returns the next `n` bits (`n <= 32`, and at least `n` bits must be buffered).
    #[inline(always)]
    fn take(&mut self, n: u32) -> u32 {
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.consume(n);
        v
    }

    /// Discards the next `n` buffered bits.
    #[inline(always)]
    fn consume(&mut self, n: u32) {
        self.buf >>= n;
        self.count -= n;
    }

    /// Skips to the next byte boundary and hands the buffered whole bytes back to the input, so
    /// that `pos` is the position of the next unread byte.
    fn align_and_rewind(&mut self) -> Result<(), ImageError> {
        self.consume(self.count & 7);
        let buffered = (self.count / 8) as usize;
        if self.overrun > buffered {
            return Err(ImageError::Truncated);
        }
        self.pos -= buffered - self.overrun;
        self.buf = 0;
        self.count = 0;
        self.overrun = 0;
        Ok(())
    }
}

/// Builds a canonical Huffman decode table for code lengths `lens` (0 = unused symbol).
///
/// Incomplete codes are accepted (unused entries decode as errors); over-subscribed codes are
/// rejected.
fn build_table(table: &mut Vec<u32>, lens: &[u8], root: u32, payload: fn(usize) -> u32) -> Result<(), ImageError> {
    let mut count = [0u16; 16];
    for &l in lens {
        count[(l & 15) as usize] += 1;
    }
    count[0] = 0;
    let mut left: i32 = 1;
    for &c in &count[1..] {
        left = (left << 1) - c as i32;
        if left < 0 {
            return Err(ImageError::Invalid("over-subscribed Huffman code"));
        }
    }
    // Sort the symbols by code length, then by symbol value (canonical order).
    let mut offs = [0u16; 16];
    for len in 1..15 {
        offs[len + 1] = offs[len] + count[len];
    }
    let mut sorted = [0u16; 320];
    for (sym, &l) in lens.iter().enumerate() {
        if l != 0 {
            let o = &mut offs[(l & 15) as usize];
            sorted[*o as usize] = sym as u16;
            *o += 1;
        }
    }

    let root_size = 1usize << root;
    table.clear();
    table.resize(root_size, F_INVALID);
    let max_len = (1..16).rev().find(|&l| count[l] != 0).unwrap_or(0) as u32;
    let mut remaining = count;
    let mut code: u32 = 0;
    let mut next_sym = 0usize;
    let mut cur_prefix = usize::MAX;
    let mut sub_start = 0usize;
    let mut sub_bits = 0u32;
    for len in 1..=max_len {
        for _ in 0..count[len as usize] {
            let sym = sorted[next_sym] as usize;
            next_sym += 1;
            // Deflate packs codes MSB-first into an LSB-first stream: index tables by the
            // bit-reversed code.
            let rev = (code.reverse_bits() >> (32 - len)) as usize;
            let entry = payload(sym);
            if len <= root {
                let mut k = rev;
                while k < root_size {
                    table[k] = entry | len;
                    k += 1 << len;
                }
            } else {
                let prefix = rev & (root_size - 1);
                if prefix != cur_prefix {
                    // New secondary table: find the smallest width that the remaining codes with
                    // this prefix fill (zlib's method; for the final group of an incomplete code
                    // this over-estimates, which is harmless).
                    let mut w = len - root;
                    let mut slots: i32 = 1 << w;
                    while root + w < max_len {
                        slots -= remaining[(root + w) as usize] as i32;
                        if slots <= 0 {
                            break;
                        }
                        w += 1;
                        slots <<= 1;
                    }
                    sub_bits = w;
                    sub_start = table.len();
                    table.resize(sub_start + (1 << w), F_INVALID);
                    table[prefix] = ((sub_start as u32) << 16) | F_SUBTABLE | (w << 4) | root;
                    cur_prefix = prefix;
                }
                let sub_len = len - root;
                let mut k = rev >> root;
                while k < (1 << sub_bits) {
                    table[sub_start + k] = entry | sub_len;
                    k += 1 << sub_len;
                }
            }
            remaining[len as usize] -= 1;
            code += 1;
        }
        code <<= 1;
    }
    Ok(())
}

fn litlen_payload(sym: usize) -> u32 {
    if sym < 256 {
        F_LITERAL | ((sym as u32) << 16)
    } else if sym == 256 {
        F_EOB
    } else if sym < 286 {
        let i = sym - 257;
        ((LEN_BASE[i] as u32) << 16) | ((LEN_EXTRA[i] as u32) << 4)
    } else {
        F_INVALID
    }
}

fn dist_payload(sym: usize) -> u32 {
    if sym < 30 { ((DIST_BASE[sym] as u32) << 16) | ((DIST_EXTRA[sym] as u32) << 4) } else { F_INVALID }
}

fn precode_payload(sym: usize) -> u32 {
    (sym as u32) << 16
}

/// Decode tables, kept across blocks so they are allocated once per stream.
#[derive(Default)]
struct Tables {
    litlen: Vec<u32>,
    dist: Vec<u32>,
    precode: Vec<u32>,
    fixed_litlen: Vec<u32>,
    fixed_dist: Vec<u32>,
}

impl Tables {
    fn build_fixed(&mut self) -> Result<(), ImageError> {
        let mut lens = [8u8; 288];
        lens[144..256].fill(9);
        lens[256..280].fill(7);
        build_table(&mut self.fixed_litlen, &lens, LITLEN_BITS, litlen_payload)?;
        build_table(&mut self.fixed_dist, &[5u8; 32], DIST_BITS, dist_payload)
    }

    /// Reads the code lengths of a dynamic block and builds its tables.
    fn read_dynamic(&mut self, bits: &mut Bits) -> Result<(), ImageError> {
        bits.refill()?;
        let hlit = bits.take(5) as usize + 257;
        let hdist = bits.take(5) as usize + 1;
        let hclen = bits.take(4) as usize + 4;
        if hlit > 286 || hdist > 30 {
            return Err(ImageError::Invalid("too many length or distance codes"));
        }
        let mut pre = [0u8; 19];
        for &i in &PRECODE_ORDER[..hclen] {
            bits.refill()?;
            pre[i] = bits.take(3) as u8;
        }
        build_table(&mut self.precode, &pre, PRECODE_BITS, precode_payload)?;

        let mut lens = [0u8; 286 + 30];
        let total = hlit + hdist;
        let mut i = 0;
        while i < total {
            bits.refill()?;
            let e = self.precode[(bits.buf & 0x7F) as usize];
            if e & F_INVALID != 0 {
                return Err(ImageError::Invalid("invalid code length code"));
            }
            bits.consume(e & NBITS_MASK);
            let sym = (e >> 16) as u8;
            if sym < 16 {
                lens[i] = sym;
                i += 1;
                continue;
            }
            let (value, n) = match sym {
                16 => {
                    if i == 0 {
                        return Err(ImageError::Invalid("code length repeat without a previous length"));
                    }
                    (lens[i - 1], 3 + bits.take(2) as usize)
                }
                17 => (0, 3 + bits.take(3) as usize),
                _ => (0, 11 + bits.take(7) as usize),
            };
            if i + n > total {
                return Err(ImageError::Invalid("code lengths overflow the alphabet"));
            }
            lens[i..i + n].fill(value);
            i += n;
        }
        if lens[256] == 0 {
            return Err(ImageError::Invalid("missing end-of-block code"));
        }
        build_table(&mut self.litlen, &lens[..hlit], LITLEN_BITS, litlen_payload)?;
        build_table(&mut self.dist, &lens[hlit..total], DIST_BITS, dist_payload)
    }
}

/// Grows `out` (zero-filled) so that `out.len() >= need`, doubling but never beyond
/// `limit + SLACK`.
#[cold]
fn reserve(out: &mut Vec<u8>, need: usize, limit: usize) -> Result<(), ImageError> {
    if need <= out.len() {
        return Ok(());
    }
    let cap = limit.saturating_add(SLACK);
    let new_len = out.len().saturating_mul(2).max(need).max(1024).min(cap).max(need);
    out.try_reserve_exact(new_len - out.len())?;
    out.resize(new_len, 0);
    Ok(())
}

/// Copies a `len`-byte match from `dist` bytes back. `buf` must extend at least 8 bytes past
/// `pos + len`.
#[inline(always)]
fn copy_match(buf: &mut [u8], pos: usize, dist: usize, len: usize) {
    let src = pos - dist;
    if dist >= 8 {
        // Each 8-byte chunk only reads bytes at least 8 positions back, which are final even when
        // source and destination overlap. The last chunk may write past `pos + len`; those bytes
        // are overwritten later or truncated away.
        let mut s = src;
        let mut d = pos;
        let end = pos + len;
        while d < end {
            buf.copy_within(s..s + 8, d);
            s += 8;
            d += 8;
        }
    } else if dist == 1 {
        let b = buf[src];
        buf[pos..pos + len].fill(b);
    } else {
        // Short period: first replicate the pattern until it is at least 8 bytes long, then copy
        // in chunks of a whole number of periods.
        let mut done = 0;
        while done < len && done < 8 {
            buf[pos + done] = buf[src + done];
            done += 1;
        }
        let period = dist * (8 / dist).max(1);
        while done < len {
            let at = pos + done;
            buf.copy_within(at - period..at - period + 8, at);
            done += 8.min(period);
        }
    }
}

/// Decodes the symbols of one Huffman block. Returns the new output position together with the
/// result so the caller keeps the progress made before an error.
fn decode_block(
    bits: &mut Bits,
    lt: &[u32],
    dt: &[u32],
    out: &mut Vec<u8>,
    mut pos: usize,
    limit: usize,
) -> (usize, Result<(), ImageError>) {
    const LITLEN_MASK: u64 = (1 << LITLEN_BITS) - 1;
    const DIST_MASK: u64 = (1 << DIST_BITS) - 1;
    loop {
        if pos + SLACK > out.len() {
            if pos > limit {
                return (pos, Err(ImageError::OutputLimit));
            }
            if let Err(e) = reserve(out, pos + SLACK, limit) {
                return (pos, Err(e));
            }
        }
        if let Err(e) = bits.refill() {
            return (pos, Err(e));
        }
        let buf = out.as_mut_slice();

        let mut e = lt[(bits.buf & LITLEN_MASK) as usize];
        if e & F_SUBTABLE != 0 {
            bits.consume(LITLEN_BITS);
            e = lt[((e >> 16) + (bits.buf as u32 & ((1 << ((e >> 4) & 0xF)) - 1))) as usize];
        }
        bits.consume(e & NBITS_MASK);
        if e & F_LITERAL != 0 {
            buf[pos] = (e >> 16) as u8;
            pos += 1;
            // A second literal usually follows; at least 41 bits are still buffered, enough for
            // one more 15-bit code without refilling.
            let e2 = lt[(bits.buf & LITLEN_MASK) as usize];
            if e2 & F_LITERAL != 0 {
                bits.consume(e2 & NBITS_MASK);
                buf[pos] = (e2 >> 16) as u8;
                pos += 1;
            }
            continue;
        }
        if e & (F_EOB | F_INVALID) != 0 {
            if e & F_EOB != 0 {
                return (pos, Ok(()));
            }
            return (pos, Err(ImageError::Invalid("invalid literal/length code")));
        }
        let len = (e >> 16) as usize + bits.take((e >> 4) & 0xF) as usize;

        let mut d = dt[(bits.buf & DIST_MASK) as usize];
        if d & F_SUBTABLE != 0 {
            bits.consume(DIST_BITS);
            d = dt[((d >> 16) + (bits.buf as u32 & ((1 << ((d >> 4) & 0xF)) - 1))) as usize];
        }
        bits.consume(d & NBITS_MASK);
        if d & F_INVALID != 0 {
            return (pos, Err(ImageError::Invalid("invalid distance code")));
        }
        let dist = (d >> 16) as usize + bits.take((d >> 4) & 0xF) as usize;
        if dist > pos {
            return (pos, Err(ImageError::Invalid("distance points before the start of the output")));
        }
        copy_match(buf, pos, dist, len);
        pos += len;
    }
}

/// Copies a stored (uncompressed) block.
fn stored_block(bits: &mut Bits, out: &mut Vec<u8>, pos: &mut usize, limit: usize) -> Result<(), ImageError> {
    bits.align_and_rewind()?;
    let p = bits.pos;
    let header: [u8; 4] = crate::util::bytes(bits.input, p)?;
    let len = u16::from_le_bytes([header[0], header[1]]);
    let nlen = u16::from_le_bytes([header[2], header[3]]);
    if len != !nlen {
        return Err(ImageError::Invalid("stored block length check failed"));
    }
    let len = len as usize;
    let src = bits.input.get(p + 4..p + 4 + len).ok_or(ImageError::Truncated)?;
    bits.pos = p + 4 + len;
    let n = len.min(limit.saturating_sub(*pos));
    reserve(out, *pos + n + SLACK, limit)?;
    out[*pos..*pos + n].copy_from_slice(&src[..n]);
    *pos += n;
    if n < len { Err(ImageError::OutputLimit) } else { Ok(()) }
}

/// Decodes a raw DEFLATE stream into `out`, starting at offset 0 and using (and growing) the
/// buffer's current length as working space. Returns the number of input bytes consumed. On
/// return `out` holds exactly the decoded bytes; after an [`ImageError::OutputLimit`] error it
/// holds the first `limit` bytes.
fn inflate_raw(input: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize, ImageError> {
    let mut pos = 0usize;
    let result = inflate_blocks(input, out, &mut pos, limit);
    out.truncate(pos.min(limit));
    result
}

fn inflate_blocks(input: &[u8], out: &mut Vec<u8>, pos: &mut usize, limit: usize) -> Result<usize, ImageError> {
    let mut bits = Bits::new(input);
    let mut tables = Tables::default();
    loop {
        bits.refill()?;
        let header = bits.take(3);
        match header >> 1 {
            0 => stored_block(&mut bits, out, pos, limit)?,
            1 => {
                if tables.fixed_litlen.is_empty() {
                    tables.build_fixed()?;
                }
                let (p, r) = decode_block(&mut bits, &tables.fixed_litlen, &tables.fixed_dist, out, *pos, limit);
                *pos = p;
                r?;
            }
            2 => {
                tables.read_dynamic(&mut bits)?;
                let (p, r) = decode_block(&mut bits, &tables.litlen, &tables.dist, out, *pos, limit);
                *pos = p;
                r?;
            }
            _ => return Err(ImageError::Invalid("reserved DEFLATE block type")),
        }
        if header & 1 != 0 {
            break;
        }
    }
    if *pos > limit {
        return Err(ImageError::OutputLimit);
    }
    // Hand back unused whole bytes; fail if the stream needed bytes past the end of the input.
    bits.align_and_rewind()?;
    Ok(bits.pos)
}

/// Prepares `out` as decode buffer: clears it and makes all of its capacity (at least a small
/// minimum, at most `limit + SLACK`) available as zero-initialized working space.
fn prepare(out: &mut Vec<u8>, limit: usize, hint: usize) -> Result<(), ImageError> {
    out.clear();
    let want = hint.max(out.capacity()).min(limit.saturating_add(SLACK)).max(SLACK + 64);
    out.try_reserve_exact(want)?;
    out.resize(want, 0);
    Ok(())
}

/// Decompresses the raw DEFLATE stream at the start of `data` into `out` (replacing its contents).
///
/// At most `limit` bytes are produced; a longer stream fails with [`ImageError::OutputLimit`].
/// Pre-reserving capacity in `out` (for example the expected output size plus a few hundred
/// bytes) avoids reallocation. Returns the number of input bytes the stream occupied; bytes after
/// the stream are ignored.
pub fn inflate_into(data: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize, ImageError> {
    let hint = data.len().saturating_mul(4);
    prepare(out, limit, hint)?;
    inflate_raw(data, out, limit)
}

/// Decompresses a raw DEFLATE stream, producing at most `limit` bytes.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    inflate_into(data, &mut out, limit)?;
    Ok(out)
}

/// Validates a two-byte zlib header.
fn zlib_header(data: &[u8]) -> Result<(), ImageError> {
    let [cmf, flg]: [u8; 2] = crate::util::bytes(data, 0)?;
    if cmf & 0x0F != 8 {
        return Err(ImageError::Invalid("zlib stream does not use DEFLATE"));
    }
    if cmf >> 4 > 7 {
        return Err(ImageError::Invalid("zlib window size is too large"));
    }
    if ((cmf as u16) << 8 | flg as u16) % 31 != 0 {
        return Err(ImageError::Invalid("zlib header check failed"));
    }
    if flg & 0x20 != 0 {
        return Err(ImageError::Unsupported("zlib preset dictionary"));
    }
    Ok(())
}

/// Decompresses a zlib stream (header, DEFLATE data, Adler-32) into `out`, replacing its contents.
///
/// At most `limit` bytes are produced. With `verify_checksum` the Adler-32 trailer must be
/// present and match. Pre-reserving capacity in `out` avoids reallocation.
pub fn zlib_decompress_into(
    data: &[u8],
    out: &mut Vec<u8>,
    limit: usize,
    verify_checksum: bool,
) -> Result<(), ImageError> {
    zlib_header(data)?;
    let body = &data[2..];
    let hint = body.len().saturating_mul(4);
    prepare(out, limit, hint)?;
    let used = inflate_raw(body, out, limit)?;
    if verify_checksum {
        let stored = crate::util::be32(body, used)?;
        if stored != adler32(out) {
            return Err(ImageError::ChecksumMismatch("zlib Adler-32"));
        }
    }
    Ok(())
}

/// Decompresses a zlib stream, producing at most `limit` bytes and verifying the Adler-32
/// checksum.
pub fn zlib_decompress(data: &[u8], limit: usize) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    zlib_decompress_into(data, &mut out, limit, true)?;
    Ok(out)
}

/// Decompresses a zlib stream whose decoded size is known exactly (PNG image data). Fails with
/// [`ImageError::Truncated`] if the stream produces less; extra data is ignored (like libpng).
pub(crate) fn zlib_decompress_exact(data: &[u8], expected: usize, verify_checksum: bool) -> Result<Vec<u8>, ImageError> {
    zlib_header(data)?;
    let body = &data[2..];
    let mut out = Vec::new();
    out.try_reserve_exact(expected.saturating_add(SLACK))?;
    prepare(&mut out, expected, expected.saturating_add(SLACK))?;
    match inflate_raw(body, &mut out, expected) {
        Ok(used) => {
            if out.len() < expected {
                return Err(ImageError::Truncated);
            }
            if verify_checksum {
                let stored = crate::util::be32(body, used)?;
                if stored != adler32(&out) {
                    return Err(ImageError::ChecksumMismatch("zlib Adler-32"));
                }
            }
            Ok(out)
        }
        // More data than the image needs: keep the image, skip the checksum.
        Err(ImageError::OutputLimit) if out.len() >= expected => Ok(out),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn known_streams() {
        // zlib.compress(b"hello") from CPython.
        let hello = [0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x06, 0x2c, 0x02, 0x15];
        assert_eq!(zlib_decompress(&hello, 100).unwrap(), b"hello");
        // Empty fixed block.
        assert_eq!(inflate(&[0x03, 0x00], 10).unwrap(), b"");
        // Stored block "abc".
        assert_eq!(inflate(&[0x01, 0x03, 0x00, 0xFC, 0xFF, b'a', b'b', b'c'], 10).unwrap(), b"abc");
        // Two stored blocks.
        let two = [0x00, 0x01, 0x00, 0xFE, 0xFF, b'x', 0x01, 0x02, 0x00, 0xFD, 0xFF, b'y', b'z'];
        assert_eq!(inflate(&two, 10).unwrap(), b"xyz");
        // zlib.compress(b"a" * 1000, 9): long overlapping match with distance 1.
        let run = [0x78, 0xda, 0x4b, 0x4c, 0x1c, 0x05, 0xa3, 0x60, 0x14, 0x0c, 0x77, 0x00, 0x00, 0xf9, 0xd8, 0x7a, 0xf8];
        assert_eq!(zlib_decompress(&run, 2000).unwrap(), vec![b'a'; 1000]);
        // Raw deflate of b"abc" * 7 (distance-3 overlapping match).
        assert_eq!(inflate(&[0x4b, 0x4c, 0x4a, 0x4e, 0xc4, 0x40, 0x00], 100).unwrap(), b"abc".repeat(7));
    }

    #[test]
    fn errors() {
        assert_eq!(inflate(&[0x07], 10), Err(ImageError::Invalid("reserved DEFLATE block type")));
        assert_eq!(inflate(&[0x01, 0x03, 0x00, 0xFC, 0xFF, b'a'], 10), Err(ImageError::Truncated));
        assert_eq!(inflate(&[0x01, 0x03, 0x00, 0xFC, 0xFE, b'a', b'b', b'c'], 10).is_err(), true);
        assert_eq!(inflate(&[0x01, 0x03, 0x00, 0xFC, 0xFF, b'a', b'b', b'c'], 2), Err(ImageError::OutputLimit));
        // Truncated in the middle of a Huffman block.
        let hello = [0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x06, 0x2c, 0x02, 0x15];
        assert!(zlib_decompress(&hello[..5], 100).is_err());
        // Bad checksum.
        let mut bad = hello;
        bad[12] ^= 1;
        assert_eq!(zlib_decompress(&bad, 100), Err(ImageError::ChecksumMismatch("zlib Adler-32")));
        // Bad header.
        assert!(zlib_decompress(&[0x78, 0x9d, 0x03, 0x00], 10).is_err());
        // Empty input.
        assert_eq!(inflate(&[], 10), Err(ImageError::Truncated));
    }

    #[test]
    fn exact_ignores_extra_data() {
        let hello = [0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x06, 0x2c, 0x02, 0x15];
        assert_eq!(zlib_decompress_exact(&hello, 3, true).unwrap(), b"hel");
        assert_eq!(zlib_decompress_exact(&hello, 5, true).unwrap(), b"hello");
        assert_eq!(zlib_decompress_exact(&hello, 6, true), Err(ImageError::Truncated));
    }

    #[test]
    fn huffman_table_shapes() {
        // A code where every length 1..=15 is used (forces secondary tables of every width).
        let mut lens = vec![0u8; 288];
        for (i, l) in lens.iter_mut().enumerate().take(15) {
            *l = (i + 1) as u8;
        }
        lens[15] = 15;
        let mut t = Vec::new();
        build_table(&mut t, &lens, LITLEN_BITS, litlen_payload).unwrap();
        // Over-subscribed.
        assert!(build_table(&mut t, &[1, 1, 1], 7, precode_payload).is_err());
        // Incomplete and empty codes are accepted.
        build_table(&mut t, &[1, 0, 0], 7, precode_payload).unwrap();
        build_table(&mut t, &[0, 0, 0], 7, precode_payload).unwrap();
        assert!(t.iter().all(|&e| e & F_INVALID != 0));
    }
}
