//! QOA — the "Quite OK Audio" format.
//!
//! QOA is a lossy codec with a fixed bit rate of 3.2 bits per sample. Each
//! sample is predicted from the previous four by a sign-sign LMS filter;
//! the prediction error is quantised to 3 bits with one of 16 scale
//! factors chosen per slice of 20 samples. Decoding needs only integer
//! arithmetic and a few operations per sample, which makes it ideal for an
//! emulated machine, and the quality is close to transparent for music.
//!
//! File layout (all integers big-endian):
//!
//! ```text
//! "qoaf" | u32 samples per channel
//! frames: u8 channels | u24 rate | u16 samples in frame | u16 frame bytes
//!         per channel: 4 x i16 LMS history, 4 x i16 LMS weights
//!         up to 256 x channels slices of 8 bytes, channels interleaved:
//!         4-bit scale factor index + 20 x 3-bit quantised residuals
//! ```
//!
//! Every frame except the last holds 5120 samples per channel, so frames
//! can be decoded independently, which gives cheap seeking.

use alloc::vec;
use alloc::vec::Vec;

use crate::AudioError;

/// Samples per slice.
pub const SLICE_LEN: usize = 20;
/// Slices per channel in a full frame.
pub const SLICES_PER_FRAME: usize = 256;
/// Samples per channel in a full frame.
pub const FRAME_LEN: usize = SLICES_PER_FRAME * SLICE_LEN;
/// Most channels a file may have.
pub const MAX_CHANNELS: usize = 8;
const LMS_LEN: usize = 4;
const MAGIC: &[u8; 4] = b"qoaf";

/// Bytes of a frame with `slices` slices per channel.
pub const fn frame_size(channels: usize, slices: usize) -> usize {
    8 + LMS_LEN * 4 * channels + 8 * slices * channels
}

const SCALEFACTORS: [i32; 16] = [1, 7, 21, 45, 84, 138, 211, 304, 421, 562, 731, 928, 1157, 1419, 1715, 2048];

/// 1 / scalefactor in .16 fixed point, rounded up.
const RECIPROCALS: [i32; 16] = {
    let mut t = [0; 16];
    let mut i = 0;
    while i < 16 {
        t[i] = ((1 << 16) + SCALEFACTORS[i] - 1) / SCALEFACTORS[i];
        i += 1;
    }
    t
};

/// Maps a scaled residual in -8..=8 to its 3-bit code.
const QUANT: [u8; 17] = [7, 7, 7, 5, 5, 3, 3, 1, 0, 0, 2, 2, 4, 4, 6, 6, 6];

/// Dequantised residuals: `round(scalefactor * {0.75, -0.75, 2.5, -2.5,
/// 4.5, -4.5, 7, -7})`, rounding ties away from zero.
const DEQUANT: [[i32; 8]; 16] = {
    const QUARTERS: [i32; 8] = [3, -3, 10, -10, 18, -18, 28, -28];
    let mut t = [[0; 8]; 16];
    let mut s = 0;
    while s < 16 {
        let mut q = 0;
        while q < 8 {
            let x = SCALEFACTORS[s] * QUARTERS[q];
            t[s][q] = if x >= 0 { (x + 2) / 4 } else { -((-x + 2) / 4) };
            q += 1;
        }
        s += 1;
    }
    t
};

/// The sign-sign LMS predictor state of one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lms {
    history: [i32; LMS_LEN],
    weights: [i32; LMS_LEN],
}

impl Lms {
    /// Initial encoder state; helps predicting the first milliseconds.
    const START: Lms = Lms { history: [0; 4], weights: [0, 0, -(1 << 13), 1 << 14] };

    #[inline(always)]
    fn predict(&self) -> i32 {
        let h = &self.history;
        let w = &self.weights;
        w[0].wrapping_mul(h[0])
            .wrapping_add(w[1].wrapping_mul(h[1]))
            .wrapping_add(w[2].wrapping_mul(h[2]))
            .wrapping_add(w[3].wrapping_mul(h[3]))
            >> 13
    }

    #[inline(always)]
    fn update(&mut self, sample: i32, residual: i32) {
        let delta = residual >> 4;
        for i in 0..LMS_LEN {
            let d = if self.history[i] < 0 { delta.wrapping_neg() } else { delta };
            self.weights[i] = self.weights[i].wrapping_add(d);
        }
        self.history = [self.history[1], self.history[2], self.history[3], sample];
    }

    /// The state as stored in a frame header (16-bit fields).
    fn stored(&self) -> Lms {
        let clamp = |v: i32| v.clamp(-32768, 32767);
        Lms { history: self.history.map(clamp), weights: self.weights.map(clamp) }
    }
}

#[inline(always)]
fn clamp_s16(v: i32) -> i32 {
    v.clamp(-32768, 32767)
}

/// Rounding division by a scale factor that never rounds non-zero values
/// to zero.
#[inline(always)]
fn div(v: i32, sf: usize) -> i32 {
    let v = v as i64;
    let n = (v * RECIPROCALS[sf] as i64 + (1 << 15)) >> 16;
    (n + (v.signum() - n.signum())).clamp(-(1 << 20), 1 << 20) as i32
}

#[inline]
fn be16(b: &[u8], p: usize) -> u16 {
    u16::from_be_bytes([b[p], b[p + 1]])
}

#[inline]
fn be64(b: &[u8], p: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[p..p + 8]);
    u64::from_be_bytes(a)
}

/// Format of a QOA file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QoaInfo {
    pub channels: usize,
    pub rate: u32,
    /// Samples per channel.
    pub frames: u64,
}

impl QoaInfo {
    pub fn duration_ms(&self) -> u64 {
        self.frames * 1000 / self.rate.max(1) as u64
    }
}

/// True if `data` starts like a QOA file.
pub fn is_qoa(data: &[u8]) -> bool {
    data.len() >= 8 && &data[0..4] == MAGIC
}

/// Reads the format from the file header and the first frame header (only
/// the first 16 bytes are needed).
pub fn probe(data: &[u8]) -> Result<QoaInfo, AudioError> {
    if !is_qoa(data) {
        return Err(AudioError::UnknownFormat);
    }
    if data.len() < 16 {
        return Err(AudioError::Malformed);
    }
    let frames = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as u64;
    let channels = data[8] as usize;
    let rate = u32::from_be_bytes([0, data[9], data[10], data[11]]);
    if channels == 0 || channels > MAX_CHANNELS || rate == 0 {
        return Err(AudioError::Malformed);
    }
    Ok(QoaInfo { channels, rate, frames })
}

/// The validated frame index of a QOA file (it does not borrow the data;
/// pass the same bytes to [`QoaIndex::decode_frame`]).
#[derive(Debug, Clone)]
pub struct QoaIndex {
    info: QoaInfo,
    /// (byte offset, first sample) of every frame.
    frames: Vec<(usize, u64)>,
    /// Bytes used by the QOA stream (anything after it is a trailer).
    end: usize,
    /// Length of the data the index was built from.
    len: usize,
}

/// A QOA file in memory with an index of its frames.
pub struct QoaFile<'a> {
    data: &'a [u8],
    index: QoaIndex,
}

impl<'a> QoaFile<'a> {
    /// Validates the file and indexes its frames.
    pub fn open(data: &'a [u8]) -> Result<QoaFile<'a>, AudioError> {
        Ok(QoaFile { data, index: QoaIndex::build(data)? })
    }

    pub fn info(&self) -> QoaInfo {
        self.index.info
    }

    /// Number of frames.
    pub fn frame_count(&self) -> usize {
        self.index.frame_count()
    }

    /// First sample (per channel) of frame `index`.
    pub fn frame_start(&self, index: usize) -> u64 {
        self.index.frame_start(index)
    }

    /// The frame containing sample `sample`.
    pub fn frame_of(&self, sample: u64) -> Option<usize> {
        self.index.frame_of(sample)
    }

    /// Bytes after the QOA stream (where a tag trailer may live).
    pub fn trailing_bytes(&self) -> &'a [u8] {
        &self.data[self.index.end.min(self.data.len())..]
    }

    /// Decodes frame `index` into `out` (interleaved; must hold
    /// `FRAME_LEN * channels` samples). Returns samples per channel.
    pub fn decode_frame(&self, index: usize, out: &mut [i16]) -> Result<usize, AudioError> {
        self.index.decode_frame(self.data, index, out)
    }
}

impl QoaIndex {
    /// Validates `data` and indexes its frames.
    pub fn build(data: &[u8]) -> Result<QoaIndex, AudioError> {
        let first = probe(data)?;
        let declared = first.frames;
        let mut frames = Vec::new();
        let mut p = 8;
        let mut total = 0u64;
        while p + 8 <= data.len() && (declared == 0 || total < declared) {
            let ch = data[p] as usize;
            let rate = u32::from_be_bytes([0, data[p + 1], data[p + 2], data[p + 3]]);
            let fsamples = be16(data, p + 4) as usize;
            let fsize = be16(data, p + 6) as usize;
            let header = 8 + LMS_LEN * 4 * ch;
            let valid = ch == first.channels
                && rate == first.rate
                && fsamples > 0
                && fsamples <= FRAME_LEN
                && fsize >= header
                && p + fsize <= data.len()
                && fsamples.div_ceil(SLICE_LEN) * ch <= (fsize - header) / 8;
            if !valid {
                if frames.is_empty() || declared != 0 {
                    return Err(AudioError::Malformed);
                }
                break; // end of a streaming file (e.g. a tag trailer follows)
            }
            frames.push((p, total));
            total += fsamples as u64;
            p += fsize;
        }
        if frames.is_empty() {
            return Err(AudioError::Malformed);
        }
        let frames_total = if declared == 0 { total } else { total.min(declared) };
        Ok(QoaIndex { info: QoaInfo { frames: frames_total, ..first }, frames, end: p, len: data.len() })
    }

    pub fn info(&self) -> QoaInfo {
        self.info
    }

    /// Number of frames.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// First sample (per channel) of frame `index`.
    pub fn frame_start(&self, index: usize) -> u64 {
        self.frames.get(index).map(|f| f.1).unwrap_or(self.info.frames)
    }

    /// The frame containing sample `sample`.
    pub fn frame_of(&self, sample: u64) -> Option<usize> {
        if sample >= self.info.frames {
            return None;
        }
        match self.frames.binary_search_by(|f| f.1.cmp(&sample)) {
            Ok(i) => Some(i),
            Err(i) => Some(i - 1),
        }
    }

    /// Byte offset where the QOA stream ends (a trailer may follow).
    pub fn stream_end(&self) -> usize {
        self.end
    }

    /// Decodes frame `index` of `data` (the bytes the index was built
    /// from) into `out` (interleaved; must hold `FRAME_LEN * channels`
    /// samples). Returns samples per channel.
    pub fn decode_frame(&self, data: &[u8], index: usize, out: &mut [i16]) -> Result<usize, AudioError> {
        let &(p, first) = self.frames.get(index).ok_or(AudioError::OutOfRange)?;
        if data.len() != self.len {
            return Err(AudioError::Malformed);
        }
        let ch = self.info.channels;
        let d = data;
        let mut fsamples = be16(d, p + 4) as usize;
        // The last frame may announce more samples than the file has.
        fsamples = fsamples.min((self.info.frames - first) as usize);
        if out.len() < fsamples * ch {
            return Err(AudioError::OutOfRange);
        }
        let mut lms = [Lms::START; MAX_CHANNELS];
        let mut q = p + 8;
        for l in lms.iter_mut().take(ch) {
            let h = be64(d, q);
            let w = be64(d, q + 8);
            q += 16;
            for i in 0..LMS_LEN {
                l.history[i] = (h >> (48 - 16 * i)) as i16 as i32;
                l.weights[i] = (w >> (48 - 16 * i)) as i16 as i32;
            }
        }
        let mut s = 0;
        while s < fsamples {
            let len = SLICE_LEN.min(fsamples - s);
            for (c, l) in lms.iter_mut().enumerate().take(ch) {
                let mut slice = be64(d, q);
                q += 8;
                let sf = (slice >> 60) as usize;
                slice <<= 4;
                let dq = &DEQUANT[sf];
                let mut o = s * ch + c;
                for _ in 0..len {
                    let predicted = l.predict();
                    let dequantized = dq[(slice >> 61) as usize];
                    let reconstructed = clamp_s16(predicted.wrapping_add(dequantized));
                    out[o] = reconstructed as i16;
                    slice <<= 3;
                    l.update(reconstructed, dequantized);
                    o += ch;
                }
            }
            s += SLICE_LEN;
        }
        Ok(fsamples)
    }
}

/// Decodes a whole file into interleaved samples.
pub fn decode(data: &[u8]) -> Result<(QoaInfo, Vec<i16>), AudioError> {
    let f = QoaFile::open(data)?;
    let info = f.info();
    let mut out = vec![0i16; info.frames as usize * info.channels];
    let mut frame = vec![0i16; FRAME_LEN * info.channels];
    for i in 0..f.frame_count() {
        let n = f.decode_frame(i, &mut frame)?;
        let start = f.frame_start(i) as usize * info.channels;
        out[start..start + n * info.channels].copy_from_slice(&frame[..n * info.channels]);
    }
    Ok((info, out))
}

/// Encodes interleaved samples (`channels` per frame) as a QOA file.
pub fn encode(samples: &[i16], channels: usize, rate: u32) -> Result<Vec<u8>, AudioError> {
    if channels == 0
        || channels > MAX_CHANNELS
        || rate == 0
        || rate > 0xFF_FFFF
        || !samples.len().is_multiple_of(channels)
    {
        return Err(AudioError::Unsupported);
    }
    let total = samples.len() / channels;
    if total == 0 || total > u32::MAX as usize {
        return Err(AudioError::Unsupported);
    }
    let frames = total.div_ceil(FRAME_LEN);
    let mut out = Vec::with_capacity(8 + frames * frame_size(channels, SLICES_PER_FRAME));
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    let mut lms = [Lms::START; MAX_CHANNELS];
    let mut prev_sf = [0usize; MAX_CHANNELS];
    for f in 0..frames {
        let start = f * FRAME_LEN;
        let len = FRAME_LEN.min(total - start);
        let slices = len.div_ceil(SLICE_LEN);
        let size = frame_size(channels, slices);
        let header = (channels as u64) << 56 | (rate as u64) << 32 | (len as u64) << 16 | size as u64;
        out.extend_from_slice(&header.to_be_bytes());
        for l in lms.iter_mut().take(channels) {
            // The decoder starts from the 16-bit stored state; so do we.
            *l = l.stored();
            let (mut h, mut w) = (0u64, 0u64);
            for i in 0..LMS_LEN {
                h = h << 16 | (l.history[i] as u16) as u64;
                w = w << 16 | (l.weights[i] as u16) as u64;
            }
            out.extend_from_slice(&h.to_be_bytes());
            out.extend_from_slice(&w.to_be_bytes());
        }
        let mut s = 0;
        while s < len {
            let slice_len = SLICE_LEN.min(len - s);
            for c in 0..channels {
                let base = (start + s) * channels + c;
                let (bits, best_lms, best_sf) = encode_slice(samples, base, channels, slice_len, &lms[c], prev_sf[c]);
                lms[c] = best_lms;
                prev_sf[c] = best_sf;
                out.extend_from_slice(&bits.to_be_bytes());
            }
            s += SLICE_LEN;
        }
    }
    Ok(out)
}

/// Finds the scale factor with the smallest error for one slice. Returns
/// the encoded 64 bits, the LMS state after the slice and the scale factor.
fn encode_slice(
    samples: &[i16],
    base: usize,
    stride: usize,
    len: usize,
    lms: &Lms,
    prev_sf: usize,
) -> (u64, Lms, usize) {
    let mut best_rank = u64::MAX;
    let mut best = (0u64, *lms, 0usize);
    for i in 0..16 {
        // Neighbouring slices usually share a scale factor: try it first.
        let sf = (i + prev_sf) % 16;
        let mut l = *lms;
        let mut bits = sf as u64;
        let mut rank = 0u64;
        let mut aborted = false;
        for k in 0..len {
            let sample = samples[base + k * stride] as i32;
            let predicted = l.predict();
            let residual = sample.wrapping_sub(predicted);
            let scaled = div(residual, sf).clamp(-8, 8);
            let quantized = QUANT[(scaled + 8) as usize] as usize;
            let dequantized = DEQUANT[sf][quantized];
            let reconstructed = clamp_s16(predicted.wrapping_add(dequantized));
            // Penalise runaway weights, which cause clicks.
            let energy = l.weights.iter().fold(0i64, |a, &w| a.saturating_add(w as i64 * w as i64)) >> 18;
            let penalty = (energy - 0x8ff).max(0) as u64;
            let err = (sample - reconstructed) as i64;
            rank = rank.saturating_add((err * err) as u64).saturating_add(penalty.saturating_mul(penalty));
            if rank > best_rank {
                aborted = true;
                break;
            }
            l.update(reconstructed, dequantized);
            bits = bits << 3 | quantized as u64;
        }
        if !aborted && rank < best_rank {
            best_rank = rank;
            best = (bits << ((SLICE_LEN - len) * 3), l, sf);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmath::FloatExt;

    fn music_like(frames: usize, channels: usize) -> Vec<i16> {
        let mut v = Vec::with_capacity(frames * channels);
        let mut rng = vmath::Rng::new(3);
        for i in 0..frames {
            let t = i as f32 / 44_100.0;
            for c in 0..channels {
                let s = 0.4 * FloatExt::sin(t * 220.0 * core::f32::consts::TAU + c as f32)
                    + 0.2 * FloatExt::sin(t * 1234.5 * core::f32::consts::TAU)
                    + 0.05 * (rng.next_f32() - 0.5);
                v.push(crate::mix::f32_to_i16(s));
            }
        }
        v
    }

    fn snr_db(a: &[i16], b: &[i16]) -> f64 {
        let (mut sig, mut noise) = (0f64, 0f64);
        for (&x, &y) in a.iter().zip(b) {
            sig += (x as f64) * (x as f64);
            noise += ((x - y) as f64) * ((x - y) as f64);
        }
        10.0 * (sig / noise.max(1.0)).log10()
    }

    #[test]
    fn tables_match_the_reference() {
        assert_eq!(RECIPROCALS, [65536, 9363, 3121, 1457, 781, 475, 311, 216, 156, 117, 90, 71, 57, 47, 39, 32]);
        assert_eq!(DEQUANT[0], [1, -1, 3, -3, 5, -5, 7, -7]);
        assert_eq!(DEQUANT[2], [16, -16, 53, -53, 95, -95, 147, -147]);
        assert_eq!(DEQUANT[15], [1536, -1536, 5120, -5120, 9216, -9216, 14336, -14336]);
    }

    #[test]
    fn roundtrip_quality_and_layout() {
        for channels in [1usize, 2] {
            // Not a multiple of the frame or slice length.
            let frames = FRAME_LEN * 2 + 1234;
            let input = music_like(frames, channels);
            let file = encode(&input, channels, 44_100).unwrap();
            let expected = 8 + 2 * frame_size(channels, 256) + frame_size(channels, 1234usize.div_ceil(20));
            assert_eq!(file.len(), expected);
            let (info, output) = decode(&file).unwrap();
            assert_eq!(info, QoaInfo { channels, rate: 44_100, frames: frames as u64 });
            assert_eq!(output.len(), input.len());
            let snr = snr_db(&input, &output);
            assert!(snr > 30.0, "SNR {snr:.1} dB");
        }
    }

    #[test]
    fn frames_decode_independently() {
        let input = music_like(FRAME_LEN * 3, 2);
        let file = encode(&input, 2, 48_000).unwrap();
        let (_, all) = decode(&file).unwrap();
        let q = QoaFile::open(&file).unwrap();
        assert_eq!(q.frame_count(), 3);
        assert_eq!(q.frame_of(FRAME_LEN as u64 + 7), Some(1));
        assert_eq!(q.frame_of(FRAME_LEN as u64 * 3), None);
        let mut buf = vec![0i16; FRAME_LEN * 2];
        assert_eq!(q.decode_frame(2, &mut buf).unwrap(), FRAME_LEN);
        assert_eq!(&buf[..], &all[FRAME_LEN * 4..]);
    }

    #[test]
    fn trailing_tags_are_ignored() {
        let input = music_like(3000, 2);
        let mut file = encode(&input, 2, 44_100).unwrap();
        let clean = decode(&file).unwrap().1;
        let tags = crate::tags::Tags { title: "x".into(), ..Default::default() };
        file.extend(tags.encode_trailer());
        let q = QoaFile::open(&file).unwrap();
        assert_eq!(crate::tags::Tags::from_trailer(q.trailing_bytes()).unwrap().title, "x");
        assert_eq!(decode(&file).unwrap().1, clean);
    }

    #[test]
    fn rejects_corrupt_files_without_panicking() {
        let input = music_like(6000, 2);
        let file = encode(&input, 2, 44_100).unwrap();
        assert!(QoaFile::open(&file[..20]).is_err());
        assert!(QoaFile::open(b"qoaf").is_err());
        let mut rng = vmath::Rng::new(9);
        for _ in 0..200 {
            let mut bad = file.clone();
            for _ in 0..8 {
                let i = rng.below(bad.len() as u32) as usize;
                bad[i] = rng.next_u32() as u8;
            }
            if let Ok(q) = QoaFile::open(&bad) {
                let mut buf = vec![0i16; FRAME_LEN * 8];
                for i in 0..q.frame_count() {
                    let _ = q.decode_frame(i, &mut buf);
                }
            }
        }
    }
}
