//! DEFLATE ([RFC 1951]) and zlib ([RFC 1950]) compression.
//!
//! The compressor follows the design of zlib's `deflate`:
//!
//! * **LZ77 with hash chains.** Every position is hashed on its next three bytes; `head` holds the
//!   most recent position per hash and `prev` links each position to the previous one with the
//!   same hash inside the 32 KiB window. Candidate matches are compared eight bytes at a time.
//! * **Effort levels 0..=9** with zlib's parameters (chain length, "good"/"nice" lengths). Levels
//!   1..=3 match greedily; levels 4..=9 use lazy evaluation (a match is deferred by one byte if the
//!   next position has a longer one).
//! * **Block coding.** Symbols are buffered and flushed in blocks. For each block the encoder
//!   builds length-limited Huffman codes from the symbol frequencies and emits whichever of
//!   dynamic Huffman, fixed Huffman or stored coding is smallest.
//!
//! Every tree is given at least two codes so that each emitted code is complete, which strict
//! decoders (including zlib's) require.
//!
//! [RFC 1951]: https://www.rfc-editor.org/rfc/rfc1951
//! [RFC 1950]: https://www.rfc-editor.org/rfc/rfc1950

use alloc::vec;
use alloc::vec::Vec;

use crate::checksum::adler32;
use crate::inflate::{DIST_BASE, DIST_EXTRA, LEN_BASE, LEN_EXTRA, PRECODE_ORDER};

const WINDOW: usize = 32768;
const WMASK: usize = WINDOW - 1;
const HASH_BITS: u32 = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
/// Length-3 matches farther away than this are not worth their distance code.
const TOO_FAR: usize = 4096;
/// Symbols buffered per block.
const BLOCK_SYMBOLS: usize = 16384 - 1;
/// Largest payload of one stored block.
const MAX_STORED: usize = 65535;

/// zlib's per-level matcher parameters.
#[derive(Clone, Copy)]
struct Params {
    /// Reduce the chain search to a quarter once a match this long is found.
    good: usize,
    /// Lazy levels: do not search for a better match once one this long is pending. Greedy
    /// levels: only insert the positions inside matches up to this length into the hash table.
    lazy: usize,
    /// Stop searching once a match this long is found.
    nice: usize,
    /// Maximum hash chain entries examined.
    chain: u32,
    /// Lazy (deferred) matching.
    lazy_mode: bool,
}

const PARAMS: [Params; 10] = [
    Params { good: 0, lazy: 0, nice: 0, chain: 0, lazy_mode: false },
    Params { good: 4, lazy: 4, nice: 8, chain: 4, lazy_mode: false },
    Params { good: 4, lazy: 5, nice: 16, chain: 8, lazy_mode: false },
    Params { good: 4, lazy: 6, nice: 32, chain: 32, lazy_mode: false },
    Params { good: 4, lazy: 4, nice: 16, chain: 16, lazy_mode: true },
    Params { good: 8, lazy: 16, nice: 32, chain: 32, lazy_mode: true },
    Params { good: 8, lazy: 16, nice: 128, chain: 128, lazy_mode: true },
    Params { good: 8, lazy: 32, nice: 128, chain: 256, lazy_mode: true },
    Params { good: 32, lazy: 128, nice: 258, chain: 1024, lazy_mode: true },
    Params { good: 32, lazy: 258, nice: 258, chain: 4096, lazy_mode: true },
];

/// Length symbol index (0..=28) for every match length 3..=258.
const LEN_SYM: [u8; 259] = {
    let mut t = [0u8; 259];
    let mut sym = 0;
    let mut len = 3;
    while len <= 258 {
        while sym < 28 && LEN_BASE[sym + 1] as usize <= len {
            sym += 1;
        }
        t[len] = sym as u8;
        len += 1;
    }
    t
};

/// Distance symbol (0..=29) for a distance 1..=32768.
#[inline(always)]
fn dist_sym(dist: usize) -> usize {
    let x = (dist - 1) as u32;
    if x < 4 {
        x as usize
    } else {
        let l = 31 - x.leading_zeros();
        (2 * l + ((x >> (l - 1)) & 1)) as usize
    }
}

/// LSB-first bit writer.
struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    count: u32,
}

impl BitWriter {
    /// Appends the low `n` bits of `v` (`n <= 32`; higher bits of `v` must be zero).
    #[inline(always)]
    fn put(&mut self, v: u32, n: u32) {
        self.buf |= (v as u64) << self.count;
        self.count += n;
        if self.count >= 32 {
            self.out.extend_from_slice(&(self.buf as u32).to_le_bytes());
            self.buf >>= 32;
            self.count -= 32;
        }
    }

    /// Pads with zero bits to a byte boundary and flushes the buffer.
    fn align(&mut self) {
        while self.count > 0 {
            self.out.push(self.buf as u8);
            self.buf >>= 8;
            self.count = self.count.saturating_sub(8);
        }
        self.buf = 0;
    }
}

/// Computes Huffman code lengths limited to `max_bits` for the symbol frequencies `freqs`.
///
/// At least two symbols always get a code (unused ones are added with a pseudo-frequency), so
/// the resulting code is complete.
fn huffman_lengths(freqs: &[u32], max_bits: u32, lens: &mut [u8]) {
    lens.fill(0);
    let mut syms: Vec<(u32, u16)> = Vec::with_capacity(freqs.len());
    syms.extend(freqs.iter().enumerate().filter(|&(_, &f)| f > 0).map(|(i, &f)| (f, i as u16)));
    let mut filler = 0;
    while syms.len() < 2 && filler < freqs.len() {
        if freqs[filler] == 0 {
            syms.push((1, filler as u16));
        }
        filler += 1;
    }
    syms.sort_unstable();
    let n = syms.len();
    if n < 2 {
        return;
    }

    // Two-queue Huffman construction: leaves are sorted, and internal nodes are created in
    // non-decreasing weight order, so the two smallest are always at the queue fronts.
    let total = 2 * n - 1;
    let mut weight = vec![0u64; total];
    let mut parent = vec![0usize; total];
    for (w, s) in weight.iter_mut().zip(&syms) {
        *w = s.0 as u64;
    }
    let (mut leaf, mut inner, mut next) = (0, n, n);
    while next < total {
        let mut pick = || {
            if leaf < n && (inner >= next || weight[leaf] <= weight[inner]) {
                leaf += 1;
                leaf - 1
            } else {
                inner += 1;
                inner - 1
            }
        };
        let a = pick();
        let b = pick();
        weight[next] = weight[a] + weight[b];
        parent[a] = next;
        parent[b] = next;
        next += 1;
    }
    let mut depth = vec![0u32; total];
    for i in (0..total - 1).rev() {
        depth[i] = depth[parent[i]] + 1;
    }

    // Clamp to `max_bits`, then repair the Kraft sum: each step moves one code from the longest
    // length down a level and splits the deepest shorter code (miniz's method).
    let mut bl_count = [0u32; 16];
    for &d in &depth[..n] {
        bl_count[d.min(max_bits) as usize] += 1;
    }
    let mut kraft: u32 = (1..=max_bits).map(|l| bl_count[l as usize] << (max_bits - l)).sum();
    while kraft > 1 << max_bits {
        bl_count[max_bits as usize] -= 1;
        for l in (1..max_bits as usize).rev() {
            if bl_count[l] != 0 {
                bl_count[l] -= 1;
                bl_count[l + 1] += 2;
                break;
            }
        }
        kraft -= 1;
    }
    // Hand out the lengths: the rarest symbols get the longest codes.
    let mut it = syms.iter();
    for l in (1..=max_bits).rev() {
        for _ in 0..bl_count[l as usize] {
            if let Some(&(_, s)) = it.next() {
                lens[s as usize] = l as u8;
            }
        }
    }
}

/// Assigns canonical codes (already bit-reversed for LSB-first output) to code lengths.
fn canonical_codes(lens: &[u8], codes: &mut [u16]) {
    let mut bl_count = [0u16; 16];
    for &l in lens {
        bl_count[l as usize] += 1;
    }
    bl_count[0] = 0;
    let mut next = [0u16; 16];
    let mut code = 0u16;
    for bits in 1..16 {
        code = (code + bl_count[bits - 1]) << 1;
        next[bits] = code;
    }
    for (c, &l) in codes.iter_mut().zip(lens) {
        if l != 0 {
            *c = next[l as usize].reverse_bits() >> (16 - l);
            next[l as usize] += 1;
        }
    }
}

/// Run-length encodes a code-length sequence with the code-length alphabet (16/17/18 repeats).
fn rle_lengths(lens: &[u8], out: &mut Vec<(u8, u8)>) {
    out.clear();
    let mut i = 0;
    while i < lens.len() {
        let l = lens[i];
        let mut run = 1;
        while i + run < lens.len() && lens[i + run] == l {
            run += 1;
        }
        i += run;
        if l == 0 {
            while run >= 11 {
                let k = run.min(138);
                out.push((18, (k - 11) as u8));
                run -= k;
            }
            if run >= 3 {
                out.push((17, (run - 3) as u8));
                run = 0;
            }
        } else {
            out.push((l, 0));
            run -= 1;
            while run >= 3 {
                let k = run.min(6);
                out.push((16, (k - 3) as u8));
                run -= k;
            }
        }
        for _ in 0..run {
            out.push((l, 0));
        }
    }
}

/// Extra bits carried by a code-length symbol.
fn precode_extra(sym: u8) -> u32 {
    match sym {
        16 => 2,
        17 => 3,
        18 => 7,
        _ => 0,
    }
}

/// The fixed literal/length code lengths.
fn fixed_lit_lens() -> [u8; 288] {
    let mut l = [8u8; 288];
    l[144..256].fill(9);
    l[256..280].fill(7);
    l
}

struct Encoder<'a> {
    data: &'a [u8],
    params: Params,
    w: BitWriter,
    head: Vec<u32>,
    prev: Vec<u32>,
    /// Buffered symbols: a literal byte (< 256) or `(dist << 9) | len` for a match.
    syms: Vec<u32>,
    lit_freq: [u32; 286],
    dist_freq: [u32; 30],
    /// Input range covered by the buffered symbols: `block_start..emitted`.
    block_start: usize,
    emitted: usize,
    /// Scratch for block coding.
    rle: Vec<(u8, u8)>,
}

impl<'a> Encoder<'a> {
    fn new(data: &'a [u8], level: u8, out: Vec<u8>) -> Self {
        let params = PARAMS[level.min(9) as usize];
        let (head, prev) = if level == 0 { (Vec::new(), Vec::new()) } else { (vec![0; HASH_SIZE], vec![0; WINDOW]) };
        Encoder {
            data,
            params,
            w: BitWriter { out, buf: 0, count: 0 },
            head,
            prev,
            syms: Vec::with_capacity(if level == 0 { 0 } else { BLOCK_SYMBOLS + 1 }),
            lit_freq: [0; 286],
            dist_freq: [0; 30],
            block_start: 0,
            emitted: 0,
            rle: Vec::new(),
        }
    }

    /// Inserts position `pos` (which must have 3 bytes of lookahead) into the hash chains and
    /// returns the previous chain head (position + 1, or 0 for none).
    #[inline(always)]
    fn insert(&mut self, pos: usize) -> u32 {
        let d = self.data;
        let v = d[pos] as u32 | (d[pos + 1] as u32) << 8 | (d[pos + 2] as u32) << 16;
        let h = (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize;
        let old = self.head[h];
        self.prev[pos & WMASK] = old;
        self.head[h] = pos as u32 + 1;
        old
    }

    /// Searches the hash chain starting at `cand` (position + 1) for the longest match at `pos`
    /// that is longer than `best_len`. Returns `(length, distance)`; the distance is 0 if nothing
    /// better was found.
    #[inline]
    fn longest_match(&self, pos: usize, mut cand: u32, mut best_len: usize, mut chain: u32) -> (usize, usize) {
        let data = self.data;
        let max_len = MAX_MATCH.min(data.len() - pos);
        let nice = self.params.nice.min(max_len);
        let mut best_dist = 0;
        if best_len >= max_len {
            return (best_len, 0);
        }
        let first = u16::from_le_bytes([data[pos], data[pos + 1]]);
        while cand != 0 && chain > 0 {
            let c = (cand - 1) as usize;
            let dist = pos - c;
            if dist > WINDOW {
                break;
            }
            // Cheap rejection: the byte that would extend the best match, and the first two.
            if data[c + best_len] == data[pos + best_len] && u16::from_le_bytes([data[c], data[c + 1]]) == first {
                let len = match_length(data, c, pos, max_len);
                if len > best_len {
                    best_len = len;
                    best_dist = dist;
                    if len >= nice {
                        break;
                    }
                }
            }
            let next = self.prev[c & WMASK];
            // Chains only point backwards; anything else is a stale entry from an older window.
            if next >= cand {
                break;
            }
            cand = next;
            chain -= 1;
        }
        (best_len, best_dist)
    }

    #[inline]
    fn literal(&mut self, b: u8) {
        self.syms.push(b as u32);
        self.lit_freq[b as usize] += 1;
        self.emitted += 1;
        if self.syms.len() >= BLOCK_SYMBOLS {
            self.flush_block(false);
        }
    }

    #[inline]
    fn matched(&mut self, len: usize, dist: usize) {
        self.syms.push(((dist as u32) << 9) | len as u32);
        self.lit_freq[257 + LEN_SYM[len] as usize] += 1;
        self.dist_freq[dist_sym(dist)] += 1;
        self.emitted += len;
        if self.syms.len() >= BLOCK_SYMBOLS {
            self.flush_block(false);
        }
    }

    /// Greedy matching (levels 1..=3).
    fn compress_greedy(&mut self) {
        let n = self.data.len();
        let p = self.params;
        let mut pos = 0;
        while pos < n {
            let mut len = 0;
            let mut dist = 0;
            if pos + MIN_MATCH <= n {
                let head = self.insert(pos);
                if head != 0 {
                    (len, dist) = self.longest_match(pos, head, MIN_MATCH - 1, p.chain);
                    if len == MIN_MATCH && dist > TOO_FAR {
                        len = 0;
                    }
                }
            }
            if len >= MIN_MATCH {
                self.matched(len, dist);
                if len <= p.lazy {
                    for q in pos + 1..(pos + len).min(n.saturating_sub(MIN_MATCH - 1)) {
                        self.insert(q);
                    }
                }
                pos += len;
            } else {
                self.literal(self.data[pos]);
                pos += 1;
            }
        }
    }

    /// Lazy matching (levels 4..=9), as zlib's `deflate_slow`.
    fn compress_lazy(&mut self) {
        let n = self.data.len();
        let p = self.params;
        let mut pos = 0;
        let mut match_len = MIN_MATCH - 1;
        let mut match_dist = 0;
        let mut pending = false;
        while pos < n {
            let head = if pos + MIN_MATCH <= n { self.insert(pos) } else { 0 };
            let prev_len = match_len;
            let prev_dist = match_dist;
            match_len = MIN_MATCH - 1;
            if head != 0 && prev_len < p.lazy {
                let chain = if prev_len >= p.good { p.chain >> 2 } else { p.chain };
                let (l, d) = self.longest_match(pos, head, prev_len, chain);
                if d != 0 {
                    match_len = l;
                    match_dist = d;
                    if l == MIN_MATCH && d > TOO_FAR {
                        match_len = MIN_MATCH - 1;
                    }
                }
            }
            if prev_len >= MIN_MATCH && match_len <= prev_len {
                // The match found at the previous position wins.
                self.matched(prev_len, prev_dist);
                let end = pos - 1 + prev_len;
                for q in pos + 1..end.min(n.saturating_sub(MIN_MATCH - 1)) {
                    self.insert(q);
                }
                pos = end;
                pending = false;
                match_len = MIN_MATCH - 1;
            } else {
                if pending {
                    self.literal(self.data[pos - 1]);
                }
                pending = true;
                pos += 1;
            }
        }
        if pending {
            self.literal(self.data[n - 1]);
        }
    }

    /// Writes the buffered symbols as one block using the cheapest coding.
    fn flush_block(&mut self, last: bool) {
        self.lit_freq[256] = 1;
        let mut lit_lens = [0u8; 286];
        let mut dist_lens = [0u8; 30];
        huffman_lengths(&self.lit_freq, 15, &mut lit_lens);
        huffman_lengths(&self.dist_freq, 15, &mut dist_lens);
        let hlit = 257.max(lit_lens.iter().rposition(|&l| l != 0).map_or(0, |i| i + 1));
        let hdist = 1.max(dist_lens.iter().rposition(|&l| l != 0).map_or(0, |i| i + 1));
        let mut all = [0u8; 286 + 30];
        all[..hlit].copy_from_slice(&lit_lens[..hlit]);
        all[hlit..hlit + hdist].copy_from_slice(&dist_lens[..hdist]);
        let mut rle = core::mem::take(&mut self.rle);
        rle_lengths(&all[..hlit + hdist], &mut rle);
        let mut pre_freq = [0u32; 19];
        for &(s, _) in &rle {
            pre_freq[s as usize] += 1;
        }
        let mut pre_lens = [0u8; 19];
        huffman_lengths(&pre_freq, 7, &mut pre_lens);
        let mut hclen = 19;
        while hclen > 4 && pre_lens[PRECODE_ORDER[hclen - 1]] == 0 {
            hclen -= 1;
        }

        let fixed_lits = fixed_lit_lens();
        let fixed_dists = [5u8; 30];
        let data_bits = |ll: &[u8], dl: &[u8]| -> u64 {
            let mut bits = 0u64;
            for (i, &f) in self.lit_freq.iter().enumerate() {
                if f != 0 {
                    let extra = if i >= 257 { LEN_EXTRA[i - 257] as u64 } else { 0 };
                    bits += f as u64 * (ll[i] as u64 + extra);
                }
            }
            for (i, &f) in self.dist_freq.iter().enumerate() {
                bits += f as u64 * (dl[i] as u64 + DIST_EXTRA[i] as u64);
            }
            bits
        };
        let header_bits: u64 = 14
            + 3 * hclen as u64
            + rle.iter().map(|&(s, _)| pre_lens[s as usize] as u64 + precode_extra(s) as u64).sum::<u64>();
        let dynamic_bits = 3 + header_bits + data_bits(&lit_lens, &dist_lens);
        let fixed_bits = 3 + data_bits(&fixed_lits, &fixed_dists);
        let raw_len = self.emitted - self.block_start;
        let stored_bits = (raw_len.div_ceil(MAX_STORED).max(1) as u64) * (3 + 7 + 32) + 8 * raw_len as u64;

        if stored_bits <= dynamic_bits.min(fixed_bits) {
            let raw = &self.data[self.block_start..self.emitted];
            write_stored(&mut self.w, raw, last);
        } else if fixed_bits <= dynamic_bits {
            self.w.put(last as u32 | (1 << 1), 3);
            self.write_symbols(&fixed_lits, &fixed_dists);
        } else {
            self.w.put(last as u32 | (2 << 1), 3);
            self.w.put((hlit - 257) as u32, 5);
            self.w.put((hdist - 1) as u32, 5);
            self.w.put((hclen - 4) as u32, 4);
            for &i in &PRECODE_ORDER[..hclen] {
                self.w.put(pre_lens[i] as u32, 3);
            }
            let mut pre_codes = [0u16; 19];
            canonical_codes(&pre_lens, &mut pre_codes);
            for &(s, extra) in &rle {
                self.w.put(pre_codes[s as usize] as u32, pre_lens[s as usize] as u32);
                let n = precode_extra(s);
                if n != 0 {
                    self.w.put(extra as u32, n);
                }
            }
            self.write_symbols(&lit_lens, &dist_lens);
        }
        self.rle = rle;
        self.syms.clear();
        self.lit_freq = [0; 286];
        self.dist_freq = [0; 30];
        self.block_start = self.emitted;
    }

    /// Writes the buffered symbols and the end-of-block code with the given code lengths.
    fn write_symbols(&mut self, lit_lens: &[u8], dist_lens: &[u8]) {
        let mut lit_codes = [0u16; 288];
        let mut dist_codes = [0u16; 30];
        canonical_codes(lit_lens, &mut lit_codes);
        canonical_codes(dist_lens, &mut dist_codes);
        let w = &mut self.w;
        for &s in &self.syms {
            if s < 256 {
                w.put(lit_codes[s as usize] as u32, lit_lens[s as usize] as u32);
            } else {
                let len = (s & 0x1FF) as usize;
                let dist = (s >> 9) as usize;
                let ls = LEN_SYM[len] as usize;
                let code_len = lit_lens[257 + ls] as u32;
                let extra = LEN_EXTRA[ls] as u32;
                w.put(
                    lit_codes[257 + ls] as u32 | (((len - LEN_BASE[ls] as usize) as u32) << code_len),
                    code_len + extra,
                );
                let ds = dist_sym(dist);
                let code_len = dist_lens[ds] as u32;
                let extra = DIST_EXTRA[ds] as u32;
                w.put(dist_codes[ds] as u32 | (((dist - DIST_BASE[ds] as usize) as u32) << code_len), code_len + extra);
            }
        }
        w.put(lit_codes[256] as u32, lit_lens[256] as u32);
    }
}

/// Writes `raw` as one or more stored blocks; the last one carries BFINAL if `last`.
fn write_stored(w: &mut BitWriter, raw: &[u8], last: bool) {
    let mut chunks = raw.chunks(MAX_STORED).peekable();
    loop {
        let chunk = chunks.next().unwrap_or(&[]);
        let final_chunk = chunks.peek().is_none();
        w.put((last && final_chunk) as u32, 3);
        w.align();
        let len = chunk.len() as u16;
        w.out.extend_from_slice(&len.to_le_bytes());
        w.out.extend_from_slice(&(!len).to_le_bytes());
        w.out.extend_from_slice(chunk);
        if final_chunk {
            break;
        }
    }
}

/// Compresses `data` as a raw DEFLATE stream appended to `out`.
fn deflate_append(data: &[u8], level: u8, out: Vec<u8>) -> Vec<u8> {
    let level = level.min(9);
    let mut enc = Encoder::new(data, level, out);
    if level == 0 {
        write_stored(&mut enc.w, data, true);
    } else {
        if enc.params.lazy_mode {
            enc.compress_lazy();
        } else {
            enc.compress_greedy();
        }
        enc.flush_block(true);
    }
    enc.w.align();
    enc.w.out
}

/// Returns the number of equal bytes at `a` and `b` (`a < b`), up to `max`.
#[inline(always)]
fn match_length(data: &[u8], a: usize, b: usize, max: usize) -> usize {
    let mut n = 0;
    while n + 8 <= max {
        let (Some(x), Some(y)) = (data.get(a + n..a + n + 8), data.get(b + n..b + n + 8)) else { break };
        let (Ok(x), Ok(y)) = (<[u8; 8]>::try_from(x), <[u8; 8]>::try_from(y)) else { break };
        let diff = u64::from_le_bytes(x) ^ u64::from_le_bytes(y);
        if diff != 0 {
            return n + (diff.trailing_zeros() / 8) as usize;
        }
        n += 8;
    }
    while n < max && data[a + n] == data[b + n] {
        n += 1;
    }
    n
}

/// Compresses `data` into a raw DEFLATE stream. `level` ranges from 0 (store only) to 9 (best
/// compression); 6 is a good default. Values above 9 are treated as 9.
pub fn deflate(data: &[u8], level: u8) -> Vec<u8> {
    deflate_append(data, level, Vec::with_capacity(data.len() / 2 + 64))
}

/// Compresses `data` into a zlib stream (2-byte header, DEFLATE data, Adler-32 trailer).
/// `level` ranges from 0 (store only) to 9 (best compression).
pub fn zlib_compress(data: &[u8], level: u8) -> Vec<u8> {
    let level = level.min(9);
    let flevel: u8 = match level {
        0 | 1 => 0,
        2..=5 => 1,
        6 => 2,
        _ => 3,
    };
    let cmf = 0x78u8;
    let mut flg = flevel << 6;
    flg |= ((31 - ((cmf as u16) << 8 | flg as u16) % 31) % 31) as u8;
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    out.push(cmf);
    out.push(flg);
    let mut out = deflate_append(data, level, out);
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflate::{inflate, zlib_decompress};

    fn sample(n: usize, seed: u32) -> Vec<u8> {
        // Mixture of runs, text-like repetition and noise.
        let mut s = seed;
        let mut v = Vec::with_capacity(n);
        while v.len() < n {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            match s % 4 {
                0 => v.extend(core::iter::repeat_n((s >> 8) as u8, (s >> 16) as usize % 40)),
                1 => {
                    let back = (s >> 8) as usize % 3000 + 1;
                    let len = (s >> 20) as usize % 300;
                    for _ in 0..len {
                        let b = if v.len() >= back { v[v.len() - back] } else { 7 };
                        v.push(b);
                    }
                }
                2 => v.extend_from_slice(b"the quick brown fox jumps over the lazy dog "),
                _ => v.push((s >> 3) as u8),
            }
        }
        v.truncate(n);
        v
    }

    #[test]
    fn length_and_distance_symbols() {
        for (len, &s) in LEN_SYM.iter().enumerate().skip(3) {
            let s = s as usize;
            assert!(LEN_BASE[s] as usize <= len);
            assert!(len - (LEN_BASE[s] as usize) < 1 << LEN_EXTRA[s]);
        }
        assert_eq!(LEN_SYM[258], 28);
        assert_eq!(LEN_SYM[257], 27);
        for dist in 1..=32768usize {
            let s = dist_sym(dist);
            assert!(DIST_BASE[s] as usize <= dist, "{dist}");
            assert!(dist - (DIST_BASE[s] as usize) < 1 << DIST_EXTRA[s], "{dist}");
        }
    }

    #[test]
    fn huffman_lengths_are_valid() {
        // Fibonacci frequencies force very deep trees that must be limited.
        let mut freqs = [0u32; 40];
        let (mut a, mut b) = (1u32, 1u32);
        for f in freqs.iter_mut() {
            *f = a;
            (a, b) = (b, a.saturating_add(b));
        }
        for max in [7u32, 9, 15] {
            let mut lens = [0u8; 40];
            huffman_lengths(&freqs, max, &mut lens);
            let kraft: u64 = lens.iter().map(|&l| 1u64 << (32 - l as u32)).sum();
            assert_eq!(kraft, 1 << 32, "code must be complete");
            assert!(lens.iter().all(|&l| l >= 1 && l as u32 <= max));
        }
        // One used symbol still yields two codes.
        let mut lens = [0u8; 5];
        huffman_lengths(&[0, 0, 9, 0, 0], 7, &mut lens);
        assert_eq!(lens, [1, 0, 1, 0, 0]);
    }

    #[test]
    fn round_trip_all_levels() {
        let inputs: [Vec<u8>; 6] = [
            Vec::new(),
            alloc::vec![42],
            alloc::vec![0; 100_000],
            sample(200_000, 1),
            (0..70_000u32).map(|i| (i * 7 + i / 300) as u8).collect(),
            (0..50_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect(),
        ];
        for data in &inputs {
            for level in 0..=9 {
                let z = zlib_compress(data, level);
                assert_eq!(&zlib_decompress(&z, data.len()).unwrap(), data, "level {level}, len {}", data.len());
                let raw = deflate(data, level);
                assert_eq!(&inflate(&raw, data.len()).unwrap(), data);
            }
        }
    }

    #[test]
    fn compresses_well() {
        let data = sample(300_000, 7);
        let z6 = zlib_compress(&data, 6).len();
        let z1 = zlib_compress(&data, 1).len();
        let z9 = zlib_compress(&data, 9).len();
        assert!(z6 < data.len() / 3, "{z6}");
        assert!(z9 <= z6 && z6 <= z1, "{z9} {z6} {z1}");
        // Incompressible data falls back to stored blocks with little overhead.
        let noise: Vec<u8> = (0..100_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        assert!(zlib_compress(&noise, 6).len() < noise.len() + 100);
    }
}
