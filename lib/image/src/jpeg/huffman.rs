//! JPEG Huffman tables: fast decoding tables, encoder code tables, the standard tables of
//! ITU-T T.81 Annex K.3, and optimal table generation (Annex K.2).

use alloc::vec::Vec;

use crate::error::ImageError;

/// Number of bits resolved by the primary decoding lookup.
pub(crate) const FAST_BITS: u32 = 9;
const FAST_SIZE: usize = 1 << FAST_BITS;

/// A Huffman table prepared for decoding.
pub(crate) struct DecodeTable {
    /// Indexed by the next [`FAST_BITS`] bits: `(code length << 8) | symbol`, or 0 when the
    /// code is longer than `FAST_BITS`.
    pub fast: [u16; FAST_SIZE],
    /// For AC tables: indexed like `fast`, the fully decoded coefficient when the code and its
    /// magnitude bits both fit: `(value << 8) | (run << 4) | total_bits`, or 0.
    pub fast_ac: [i16; FAST_SIZE],
    /// `maxcode[l]`: the largest code of length `l` (or -1 if there is none).
    pub maxcode: [i32; 18],
    /// `values[code + valoffset[l]]` is the symbol of the `l`-bit `code`.
    pub valoffset: [i32; 18],
    /// Symbols in code order.
    pub values: [u8; 256],
}

impl DecodeTable {
    /// Builds a decoding table from the `BITS` counts (codes per length 1..=16) and the symbols.
    pub(crate) fn new(counts: &[u8; 16], symbols: &[u8]) -> Result<DecodeTable, ImageError> {
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        if total > 256 || symbols.len() < total {
            return Err(ImageError::Invalid("invalid Huffman table"));
        }
        let mut t = DecodeTable {
            fast: [0; FAST_SIZE],
            fast_ac: [0; FAST_SIZE],
            maxcode: [-1; 18],
            valoffset: [0; 18],
            values: [0; 256],
        };
        t.values[..total].copy_from_slice(&symbols[..total]);
        let mut code: i32 = 0;
        let mut k: i32 = 0;
        for len in 1..=16u32 {
            let n = counts[len as usize - 1] as i32;
            t.valoffset[len as usize] = k - code;
            if code + n > 1 << len {
                return Err(ImageError::Invalid("over-subscribed Huffman table"));
            }
            if n > 0 {
                if len <= FAST_BITS {
                    for i in 0..n {
                        let c = (code + i) as usize;
                        let shift = FAST_BITS - len;
                        let entry = (len as u16) << 8 | t.values[(k + i) as usize] as u16;
                        for j in 0..(1usize << shift) {
                            t.fast[(c << shift) | j] = entry;
                        }
                    }
                }
                code += n;
                k += n;
                t.maxcode[len as usize] = code - 1;
            }
            code <<= 1;
        }
        t.maxcode[17] = i32::MAX;
        Ok(t)
    }

    /// Fills the combined run/size/value table used for AC coefficients.
    pub(crate) fn build_fast_ac(&mut self) {
        for i in 0..FAST_SIZE {
            let e = self.fast[i];
            if e == 0 {
                continue;
            }
            let len = (e >> 8) as u32;
            let rs = (e & 0xFF) as u32;
            let run = rs >> 4;
            let size = rs & 15;
            if size != 0 && len + size <= FAST_BITS {
                let bits = ((i as u32) >> (FAST_BITS - len - size)) & ((1 << size) - 1);
                let value = if bits < 1 << (size - 1) { bits as i32 - (1 << size) + 1 } else { bits as i32 };
                if (-128..=127).contains(&value) {
                    self.fast_ac[i] = (value * 256 + (run * 16) as i32 + (len + size) as i32) as i16;
                }
            }
        }
    }
}

/// A Huffman table in the form written to a `DHT` segment.
#[derive(Clone)]
pub(crate) struct Spec {
    /// Number of codes of each length 1..=16.
    pub counts: [u8; 16],
    /// Symbols in code order.
    pub symbols: Vec<u8>,
}

impl Spec {
    fn from_static(counts: &[u8; 16], symbols: &[u8]) -> Spec {
        Spec { counts: *counts, symbols: symbols.to_vec() }
    }

    /// Code (right-aligned) and length for every symbol (length 0 = no code).
    pub(crate) fn encode_table(&self) -> ([u16; 256], [u8; 256]) {
        let mut codes = [0u16; 256];
        let mut lens = [0u8; 256];
        let mut code = 0u32;
        let mut k = 0usize;
        for len in 1..=16u8 {
            for _ in 0..self.counts[len as usize - 1] {
                if let Some(&s) = self.symbols.get(k) {
                    codes[s as usize] = code as u16;
                    lens[s as usize] = len;
                }
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        (codes, lens)
    }

    /// Builds an optimal table (code lengths limited to 16 bits) for symbol frequencies, using
    /// the procedure of T.81 Annex K.2 (as libjpeg's `jpeg_gen_optimal_table`).
    pub(crate) fn optimal(freq_in: &[u32; 256]) -> Spec {
        const MAX_CLEN: usize = 32;
        let mut freq = [0u64; 257];
        for (f, &x) in freq.iter_mut().zip(freq_in.iter()) {
            *f = x as u64;
        }
        // A reserved pseudo-symbol guarantees that no real symbol gets the all-ones code.
        freq[256] = 1;
        let mut codesize = [0usize; 257];
        let mut others = [-1i32; 257];
        loop {
            let mut c1: i32 = -1;
            let mut v = u64::MAX;
            for (i, &f) in freq.iter().enumerate() {
                if f != 0 && f <= v {
                    v = f;
                    c1 = i as i32;
                }
            }
            let mut c2: i32 = -1;
            v = u64::MAX;
            for (i, &f) in freq.iter().enumerate() {
                if f != 0 && f <= v && i as i32 != c1 {
                    v = f;
                    c2 = i as i32;
                }
            }
            if c2 < 0 {
                break;
            }
            let (mut a, mut b) = (c1 as usize, c2 as usize);
            freq[a] += freq[b];
            freq[b] = 0;
            codesize[a] += 1;
            while others[a] >= 0 {
                a = others[a] as usize;
                codesize[a] += 1;
            }
            others[a] = b as i32;
            codesize[b] += 1;
            while others[b] >= 0 {
                b = others[b] as usize;
                codesize[b] += 1;
            }
        }
        let mut bits = [0usize; MAX_CLEN + 1];
        for &cs in &codesize {
            if cs != 0 {
                bits[cs.min(MAX_CLEN)] += 1;
            }
        }
        // Limit code lengths to 16 bits (Annex K.2, figure K.3).
        let mut i = MAX_CLEN;
        while i > 16 {
            while bits[i] > 0 {
                let mut j = i - 2;
                while bits[j] == 0 {
                    j -= 1;
                }
                bits[i] -= 2;
                bits[i - 1] += 1;
                bits[j + 1] += 2;
                bits[j] -= 1;
            }
            i -= 1;
        }
        // Remove the pseudo-symbol from the longest length in use.
        while bits[i] == 0 {
            i -= 1;
        }
        bits[i] -= 1;
        let mut counts = [0u8; 16];
        for l in 1..=16 {
            counts[l - 1] = bits[l] as u8;
        }
        let mut symbols = Vec::new();
        for len in 1..=MAX_CLEN {
            for (s, &cs) in codesize.iter().enumerate().take(256) {
                if cs == len {
                    symbols.push(s as u8);
                }
            }
        }
        Spec { counts, symbols }
    }
}

/// Standard luminance DC table (Annex K.3.1).
pub(crate) const DC_LUMA_COUNTS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
/// Standard chrominance DC table.
pub(crate) const DC_CHROMA_COUNTS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
/// Symbols of both standard DC tables.
pub(crate) const DC_SYMBOLS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// Standard luminance AC table (Annex K.3.2).
pub(crate) const AC_LUMA_COUNTS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D];
pub(crate) const AC_LUMA_SYMBOLS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14,
    0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09,
    0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A,
    0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65,
    0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88,
    0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9,
    0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA,
    0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
    0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
];

/// Standard chrominance AC table (Annex K.3.2).
pub(crate) const AC_CHROMA_COUNTS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
pub(crate) const AC_CHROMA_SYMBOLS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32,
    0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0, 0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16,
    0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64,
    0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86,
    0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8,
    0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9,
    0xEA, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
];

/// The standard table for a DC (`ac == false`) or AC table slot. Slot 0 gets the luminance
/// table, every other slot the chrominance table (libjpeg's convention for motion-JPEG streams
/// that omit `DHT`).
pub(crate) fn standard(ac: bool, slot: usize) -> Spec {
    match (ac, slot == 0) {
        (false, true) => Spec::from_static(&DC_LUMA_COUNTS, &DC_SYMBOLS),
        (false, false) => Spec::from_static(&DC_CHROMA_COUNTS, &DC_SYMBOLS),
        (true, true) => Spec::from_static(&AC_LUMA_COUNTS, &AC_LUMA_SYMBOLS),
        (true, false) => Spec::from_static(&AC_CHROMA_COUNTS, &AC_CHROMA_SYMBOLS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_tables_are_consistent() {
        for (counts, syms) in [
            (&DC_LUMA_COUNTS, &DC_SYMBOLS[..]),
            (&DC_CHROMA_COUNTS, &DC_SYMBOLS[..]),
            (&AC_LUMA_COUNTS, &AC_LUMA_SYMBOLS[..]),
            (&AC_CHROMA_COUNTS, &AC_CHROMA_SYMBOLS[..]),
        ] {
            assert_eq!(counts.iter().map(|&c| c as usize).sum::<usize>(), syms.len());
            DecodeTable::new(counts, syms).unwrap();
            // Every AC table must contain EOB, ZRL and all run/size combinations.
            if syms.len() == 162 {
                let mut seen = [false; 256];
                for &s in syms {
                    assert!(!seen[s as usize], "duplicate symbol {s:#x}");
                    seen[s as usize] = true;
                }
                for r in 0..16 {
                    for s in 1..=10 {
                        assert!(seen[r << 4 | s]);
                    }
                }
                assert!(seen[0] && seen[0xF0]);
            }
        }
    }

    #[test]
    fn optimal_table_round_trip() {
        let mut freq = [0u32; 256];
        for (i, f) in freq.iter_mut().enumerate() {
            *f = if i % 3 == 0 { (i as u32 * 7919) % 1000 + 1 } else { 0 };
        }
        freq[0] = 1_000_000; // very skewed
        let spec = Spec::optimal(&freq);
        let total: usize = spec.counts.iter().map(|&c| c as usize).sum();
        assert_eq!(total, freq.iter().filter(|&&f| f > 0).count());
        let (codes, lens) = spec.encode_table();
        let table = DecodeTable::new(&spec.counts, &spec.symbols).unwrap();
        // Every symbol's code decodes back to the symbol through the decoding tables.
        for s in 0..256 {
            if freq[s] == 0 {
                continue;
            }
            let (code, len) = (codes[s] as u32, lens[s] as u32);
            assert!((1..=16).contains(&len));
            let decoded = if len <= FAST_BITS {
                let e = table.fast[(code << (FAST_BITS - len)) as usize];
                assert_eq!((e >> 8) as u32, len);
                (e & 0xFF) as u8
            } else {
                assert!(code as i32 <= table.maxcode[len as usize]);
                table.values[(code as i32 + table.valoffset[len as usize]) as usize]
            };
            assert_eq!(decoded as usize, s);
        }
    }
}
