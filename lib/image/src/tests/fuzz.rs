//! Mutation fuzzing: valid files of every format are damaged in random ways (bit flips, byte
//! changes, "interesting" values, truncation, insertion, deletion, duplication) and decoded many
//! thousands of times. Decoders must return an image or an error without panicking, looping
//! forever or allocating excessively (the decode limits are lowered so that a mutated header
//! cannot make a test allocate much).

use std::println;
use std::vec::Vec;

use crate::bmp::tests::build as build_bmp;
use crate::jpeg::{EncodeOptions, Subsampling};
use crate::png::tests::TestPng;
use crate::{DecodeOptions, Image};

/// xorshift64* pseudo-random numbers (deterministic, so failures are reproducible).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

/// Applies 1..=4 random mutations to `data`.
fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    for _ in 0..1 + rng.below(4) {
        if d.is_empty() {
            d.push(rng.next() as u8);
            continue;
        }
        let pos = rng.below(d.len());
        match rng.below(9) {
            0 | 1 => d[pos] ^= 1 << rng.below(8),
            2 => d[pos] = rng.next() as u8,
            3 => d[pos] = [0x00, 0xFF, 0x7F, 0x80, 0x01, 0xFE, 0x10][rng.below(7)],
            4 => d.truncate(pos),
            5 => {
                let n = 1 + rng.below(16);
                let bytes: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
                d.splice(pos..pos, bytes);
            }
            6 => {
                let end = (pos + 1 + rng.below(32)).min(d.len());
                d.drain(pos..end);
            }
            7 => {
                let end = (pos + 1 + rng.below(64)).min(d.len());
                let chunk = d[pos..end].to_vec();
                let at = rng.below(d.len());
                d.splice(at..at, chunk);
            }
            _ => {
                // Large integers where headers keep sizes and lengths.
                let v = [0xFFFF_FFFFu32, 0x7FFF_FFFF, 0x8000_0000, 0x0001_0000, 0xFFFF][rng.below(5)].to_be_bytes();
                for (i, b) in v.iter().enumerate() {
                    if let Some(x) = d.get_mut(pos + i) {
                        *x = *b;
                    }
                }
            }
        }
    }
    d
}

fn photo(w: u32, h: u32) -> Image {
    Image::from_fn(w, h, |x, y| {
        let v = (x * 255 / w) ^ (y * 255 / h);
        0xFF00_0000 | v << 16 | (x * 17 % 256) << 8 | (y * 29 % 256)
    })
}

/// A collection of small, valid files covering the decoders' code paths.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut files = Vec::new();
    let rgba = Image::from_fn(23, 17, |x, y| ((x * 11 + y) % 256) << 24 | (x * 9) << 16 | (y * 13) << 8 | (x ^ y));
    files.push(("png rgba", crate::png::encode(&rgba, 6).unwrap()));
    files.push(("png rgb", crate::png::encode(&photo(16, 16), 9).unwrap()));
    files.push(("png stored", crate::png::encode(&photo(9, 5), 0).unwrap()));
    let samples: Vec<u16> = (0..12 * 10).map(|i| (i * 7 % 13) as u16).collect();
    let palette: Vec<u8> = (0..13 * 3).map(|i| (i * 19) as u8).collect();
    let trns = [0u8, 50, 100];
    let indexed = TestPng {
        width: 12,
        height: 10,
        color: 3,
        depth: 4,
        interlaced: true,
        samples: &samples,
        palette: &palette,
        trns: Some(&trns),
        idat_split: 9,
        level: 6,
    };
    files.push(("png palette interlaced", indexed.build()));
    let gray16: Vec<u16> = (0..11 * 9).map(|i| (i * 977 % 65536) as u16).collect();
    let g16 = TestPng {
        width: 11,
        height: 9,
        color: 0,
        depth: 16,
        interlaced: true,
        samples: &gray16,
        palette: &[],
        trns: Some(&[0, 0]),
        idat_split: 1000,
        level: 1,
    };
    files.push(("png gray16 interlaced", g16.build()));
    let bits: Vec<u16> = (0..20 * 6).map(|i| (i % 3 == 0) as u16).collect();
    let g1 = TestPng {
        width: 20,
        height: 6,
        color: 0,
        depth: 1,
        interlaced: false,
        samples: &bits,
        palette: &[],
        trns: None,
        idat_split: 1000,
        level: 6,
    };
    files.push(("png gray1", g1.build()));

    let img = photo(32, 24);
    let jpeg = |o: EncodeOptions| crate::jpeg::encode(&img, &o).unwrap();
    let d = EncodeOptions::default();
    files.push(("jpeg 420", jpeg(d)));
    files
        .push(("jpeg 444 restart", jpeg(EncodeOptions { subsampling: Subsampling::Yuv444, restart_interval: 2, ..d })));
    files.push((
        "jpeg 422 optimized",
        jpeg(EncodeOptions { subsampling: Subsampling::Yuv422, optimize_huffman: true, ..d }),
    ));
    files.push(("jpeg progressive", jpeg(EncodeOptions { progressive: true, ..d })));
    files.push((
        "jpeg progressive gray restart",
        jpeg(EncodeOptions { progressive: true, grayscale: true, restart_interval: 3, ..d }),
    ));
    files.push(("jpeg exif", crate::jpeg::tests::with_orientation(&jpeg(EncodeOptions { quality: 50, ..d }), 6)));
    files.push(("jpeg separate scans", crate::jpeg::encode_separate_scans(&img, &d, 2, 2).unwrap()));
    let plane: Vec<u8> = (0..32 * 24).map(|i| (i * 7 % 256) as u8).collect();
    let planes: [&[u8]; 4] = [&plane, &plane, &plane, &plane];
    files.push(("jpeg cmyk", crate::jpeg::encode_planes(&planes, 32, 24, Some(0), &d, false).unwrap()));
    let prog = EncodeOptions { progressive: true, ..d };
    files.push(("jpeg ycck progressive", crate::jpeg::encode_planes(&planes, 32, 24, Some(2), &prog, false).unwrap()));

    files.push(("bmp 32 v4", crate::bmp::encode(&rgba).unwrap()));
    let pal = [[0u8, 0, 255, 0], [0, 255, 0, 0], [255, 0, 0, 0], [9, 9, 9, 0]];
    let px8: Vec<u8> = (0..8 * 6).map(|i| (i % 4) as u8).collect();
    files.push(("bmp 8", build_bmp(40, 8, 6, 8, 0, &pal, &[], &px8)));
    let px4: Vec<u8> = (0..4 * 5).map(|i| (i * 0x13) as u8).collect();
    files.push(("bmp 4 core", build_bmp(12, 7, 5, 4, 0, &pal, &[], &px4)));
    let rle8 = [3, 1, 1, 2, 0, 0, 0, 2, 1, 0, 1, 3, 0, 0, 0, 4, 1, 2, 3, 1, 0, 1];
    files.push(("bmp rle8", build_bmp(40, 4, 3, 8, 1, &pal, &[], &rle8)));
    files.push(("bmp rle4", build_bmp(40, 5, 2, 4, 2, &pal, &[], &[5, 0x12, 0, 0, 0, 5, 0x12, 0x30, 0x20, 0, 0, 1])));
    let px16: Vec<u8> = (0..6 * 4 * 2).map(|i| (i * 37) as u8).collect();
    files.push(("bmp 565", build_bmp(40, 6, -4, 16, 3, &[], &[0xF800, 0x07E0, 0x001F], &px16)));
    let px24: Vec<u8> = (0..4 * 12).map(|i| (i * 5) as u8).collect();
    files.push(("bmp 24", build_bmp(40, 4, 4, 24, 0, &[], &[], &px24)));

    files.push(("qoi rgba", crate::qoi::encode(&rgba).unwrap()));
    files.push(("qoi rgb", crate::qoi::encode(&photo(20, 12)).unwrap()));
    files
}

const LIMITS: DecodeOptions =
    DecodeOptions { max_dimension: 4096, max_pixels: 1 << 18, verify_checksums: true, apply_orientation: true };

/// Decodes `data` with every entry point; all must return without panicking. Returns whether
/// the strict and the checksum-ignoring decodes succeeded.
fn exercise(data: &[u8]) -> (bool, bool) {
    let _ = crate::read_info(data);
    let lax = DecodeOptions { verify_checksums: false, ..LIMITS };
    let a = crate::decode_with(data, &LIMITS);
    let b = crate::decode_with(data, &lax);
    for img in [&a, &b].into_iter().flatten() {
        assert_eq!(img.pixels.len(), img.width as usize * img.height as usize);
        assert!(img.width as u64 * img.height as u64 <= LIMITS.max_pixels);
    }
    (a.is_ok(), b.is_ok())
}

#[test]
fn corpus_decodes() {
    for (name, data) in corpus() {
        let img = crate::decode_with(&data, &LIMITS).unwrap_or_else(|e| panic!("{name}: {e}"));
        let info = crate::read_info(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((img.width, img.height), (info.width, info.height), "{name}");
    }
}

fn run_mutations(seed: u64, iterations: usize) {
    let mut rng = Rng(seed);
    for (name, data) in corpus() {
        let (mut strict, mut lax) = (0, 0);
        for _ in 0..iterations {
            let m = mutate(&mut rng, &data);
            let (a, b) = exercise(&m);
            strict += a as usize;
            lax += b as usize;
        }
        println!("{name:<32} {iterations} mutations: {strict} decoded, {lax} without checksums");
    }
}

#[test]
fn mutations_never_panic() {
    run_mutations(0x9E37_79B9_7F4A_7C15, 4000);
}

/// A much longer run (about a million decodes); run manually.
#[test]
#[ignore]
fn mutations_never_panic_long() {
    for seed in 1..=10 {
        run_mutations(seed * 0x1234_5678_9ABC_DEF1, 5000);
    }
}

#[test]
fn every_truncation() {
    for (name, data) in corpus() {
        for len in 0..data.len() {
            exercise(&data[..len]);
        }
        let _ = name;
    }
}

#[test]
fn garbage_after_magic() {
    let mut rng = Rng(42);
    let magics: [&[u8]; 6] = [
        &crate::png::SIGNATURE,
        &[0xFF, 0xD8, 0xFF],
        b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00\x28\x00\x00\x00",
        b"qoif",
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR",
        &[0xFF, 0xD8, 0xFF, 0xC2],
    ];
    for magic in magics {
        for _ in 0..2000 {
            let mut d = magic.to_vec();
            let n = rng.below(300);
            d.extend((0..n).map(|_| rng.next() as u8));
            exercise(&d);
        }
    }
}

#[test]
fn zlib_mutations() {
    let mut rng = Rng(7);
    let mut text = Vec::new();
    for i in 0..3000u32 {
        text.extend_from_slice(&(i % 251).to_le_bytes()[..2]);
        if i % 7 == 0 {
            text.extend_from_slice(b"repetition repetition ");
        }
    }
    for level in [0u8, 1, 6, 9] {
        let z = crate::deflate::zlib_compress(&text, level);
        for _ in 0..2000 {
            let m = mutate(&mut rng, &z);
            if let Ok(out) = crate::inflate::zlib_decompress(&m, 1 << 16) {
                assert!(out.len() <= 1 << 16);
            }
            let _ = crate::inflate::inflate(m.get(2..).unwrap_or(&[]), 1 << 16);
        }
    }
}
