//! RIFF/WAVE files.
//!
//! [`WavInfo::parse`] understands uncompressed PCM (8-bit unsigned, 16, 24
//! and 32-bit signed), IEEE float (32 and 64-bit), `WAVE_FORMAT_EXTENSIBLE`
//! headers, 1 to 8 channels and the `LIST/INFO` metadata chunk (title,
//! artist, album, genre, year, track). [`read_stereo`] converts any of these
//! to interleaved stereo `i16` (multi-channel files are downmixed), and
//! [`write`] produces 16-bit PCM files.
//!
//! Parsing never panics: truncated files are clamped to the bytes that are
//! present and malformed headers yield an [`AudioError`].

use alloc::string::String;
use alloc::vec::Vec;

use crate::tags::Tags;
use crate::{AudioError, le16, le32};

/// Storage format of one sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    U8,
    S16,
    S24,
    S32,
    F32,
    F64,
}

impl SampleFormat {
    /// Bytes per sample.
    pub fn bytes(self) -> usize {
        match self {
            SampleFormat::U8 => 1,
            SampleFormat::S16 => 2,
            SampleFormat::S24 => 3,
            SampleFormat::S32 | SampleFormat::F32 => 4,
            SampleFormat::F64 => 8,
        }
    }
}

/// A parsed WAV header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavInfo {
    pub rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
    /// Bytes per frame (all channels).
    pub block_align: u16,
    /// Offset of the first sample in the file.
    pub data_offset: usize,
    /// Length of the sample data in bytes (whole frames, clamped to the file).
    pub data_len: usize,
    /// Number of frames.
    pub frames: u64,
    /// Metadata from `LIST/INFO` chunks found in the parsed bytes.
    pub tags: Tags,
    /// Offset of the first chunk after the sample data (where trailing
    /// metadata may live), or 0 if the file ends with the data.
    pub tail_offset: usize,
}

const FORMAT_PCM: u16 = 1;
const FORMAT_FLOAT: u16 = 3;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

impl WavInfo {
    /// Parses a complete WAV file.
    pub fn parse(bytes: &[u8]) -> Result<WavInfo, AudioError> {
        WavInfo::parse_prefix(bytes, bytes.len() as u64)
    }

    /// Parses the first part of a WAV file whose total length is
    /// `file_len`. The header and `fmt `/`data` chunks must be inside
    /// `prefix`; the data size is validated against `file_len`, so the
    /// duration is right even when only the beginning was read.
    pub fn parse_prefix(prefix: &[u8], file_len: u64) -> Result<WavInfo, AudioError> {
        if prefix.len() < 12 || &prefix[0..4] != b"RIFF" || &prefix[8..12] != b"WAVE" {
            return Err(AudioError::UnknownFormat);
        }
        let file_len = (file_len as usize).max(prefix.len());
        let mut fmt: Option<(u16, u16, u32, u16, u16)> = None;
        let mut tags = Tags::default();
        let mut off = 12usize;
        let mut data: Option<(usize, usize)> = None;
        let mut tail_offset = 0;
        while off + 8 <= prefix.len() {
            let id = &prefix[off..off + 4];
            let size = le32(prefix, off + 4) as usize;
            let body = off + 8;
            if id == b"data" {
                let avail = file_len - body;
                let len = if size == 0 || size == u32::MAX as usize { avail } else { size.min(avail) };
                data = Some((body, len));
                let next = body.saturating_add(len).saturating_add(len & 1);
                if next + 8 <= file_len {
                    tail_offset = next;
                }
                off = next;
                continue;
            }
            let end = body.saturating_add(size).min(prefix.len());
            let chunk = &prefix[body..end];
            if id == b"fmt " {
                if chunk.len() < 16 {
                    return Err(AudioError::Malformed);
                }
                let mut tag = le16(chunk, 0);
                if tag == FORMAT_EXTENSIBLE {
                    if chunk.len() < 26 {
                        return Err(AudioError::Malformed);
                    }
                    tag = le16(chunk, 24);
                }
                fmt = Some((tag, le16(chunk, 2), le32(chunk, 4), le16(chunk, 12), le16(chunk, 14)));
            } else if id == b"LIST" {
                parse_list(chunk, &mut tags);
            }
            off = body.saturating_add(size).saturating_add(size & 1);
        }
        let (tag, channels, rate, block_align, bits) = fmt.ok_or(AudioError::Malformed)?;
        let (data_offset, data_len) = data.ok_or(AudioError::Malformed)?;
        if channels == 0 || channels > 8 || !(1000..=768_000).contains(&rate) || block_align == 0 {
            return Err(AudioError::Unsupported);
        }
        if block_align % channels != 0 {
            return Err(AudioError::Malformed);
        }
        let container = block_align / channels;
        let format = match (tag, container) {
            (FORMAT_PCM, 1) if bits <= 8 => SampleFormat::U8,
            (FORMAT_PCM, 2) => SampleFormat::S16,
            (FORMAT_PCM, 3) => SampleFormat::S24,
            (FORMAT_PCM, 4) => SampleFormat::S32,
            (FORMAT_FLOAT, 4) => SampleFormat::F32,
            (FORMAT_FLOAT, 8) => SampleFormat::F64,
            _ => return Err(AudioError::Unsupported),
        };
        let data_len = data_len - data_len % block_align as usize;
        let frames = (data_len / block_align as usize) as u64;
        if tags.duration_ms == 0 {
            tags.duration_ms = frames * 1000 / rate as u64;
        }
        Ok(WavInfo { rate, channels, format, block_align, data_offset, data_len, frames, tags, tail_offset })
    }

    /// Duration in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.frames * 1000 / self.rate as u64
    }
}

/// Reads metadata from chunks that follow the sample data (pass the bytes
/// starting at [`WavInfo::tail_offset`]).
pub fn parse_tail_tags(bytes: &[u8]) -> Tags {
    let mut tags = Tags::default();
    let mut off = 0;
    while off + 8 <= bytes.len() {
        let size = le32(bytes, off + 4) as usize;
        let end = (off + 8).saturating_add(size).min(bytes.len());
        if &bytes[off..off + 4] == b"LIST" {
            parse_list(&bytes[off + 8..end], &mut tags);
        }
        off = (off + 8).saturating_add(size).saturating_add(size & 1);
    }
    tags
}

/// Parses a `LIST` chunk body; only `INFO` lists carry tags.
fn parse_list(chunk: &[u8], tags: &mut Tags) {
    if chunk.len() < 4 || &chunk[0..4] != b"INFO" {
        return;
    }
    let mut off = 4;
    while off + 8 <= chunk.len() {
        let id = &chunk[off..off + 4];
        let size = le32(chunk, off + 4) as usize;
        let end = (off + 8).saturating_add(size).min(chunk.len());
        let raw = &chunk[off + 8..end];
        let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
        let text = String::from_utf8_lossy(raw);
        let text = text.trim();
        match id {
            b"INAM" => tags.set("title", text),
            b"IART" => tags.set("artist", text),
            b"IPRD" => tags.set("album", text),
            b"IGNR" => tags.set("genre", text),
            b"ICRD" => tags.year = text.get(..4).and_then(|y| y.parse().ok()).unwrap_or(0),
            b"ITRK" | b"IPRT" => tags.track = text.split('/').next().and_then(|t| t.trim().parse().ok()).unwrap_or(0),
            b"ICMT" => tags.set("comment", text),
            _ => {}
        }
        off = (off + 8).saturating_add(size).saturating_add(size & 1);
    }
}

/// Left/right downmix weights for each channel of a multi-channel layout
/// (the WAVE default order: FL, FR, C, LFE, BL, BR, SL, SR).
fn downmix_weights(channels: usize) -> [(f32, f32); 8] {
    const H: f32 = core::f32::consts::FRAC_1_SQRT_2;
    let mut w = [(0.0f32, 0.0f32); 8];
    let layout: &[(f32, f32)] = match channels {
        3 => &[(1.0, 0.0), (0.0, 1.0), (H, H)],
        4 => &[(1.0, 0.0), (0.0, 1.0), (H, 0.0), (0.0, H)],
        5 => &[(1.0, 0.0), (0.0, 1.0), (H, H), (H, 0.0), (0.0, H)],
        6 => &[(1.0, 0.0), (0.0, 1.0), (H, H), (0.0, 0.0), (H, 0.0), (0.0, H)],
        7 => &[(1.0, 0.0), (0.0, 1.0), (H, H), (0.0, 0.0), (0.5, 0.5), (H, 0.0), (0.0, H)],
        _ => &[(1.0, 0.0), (0.0, 1.0), (H, H), (0.0, 0.0), (H, 0.0), (0.0, H), (H, 0.0), (0.0, H)],
    };
    let left: f32 = layout.iter().map(|p| p.0).sum();
    let norm = if left > 0.0 { 1.0 / left } else { 1.0 };
    for (i, &(l, r)) in layout.iter().enumerate().take(8) {
        w[i] = (l * norm, r * norm);
    }
    w
}

/// One sample as a float in [-1, 1).
#[inline]
fn sample_f32(format: SampleFormat, b: &[u8]) -> f32 {
    match format {
        SampleFormat::U8 => (b[0] as f32 - 128.0) / 128.0,
        SampleFormat::S16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        SampleFormat::S24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
        SampleFormat::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        SampleFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFormat::F64 => f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32,
    }
}

/// One sample as `i16`.
#[inline]
fn sample_i16(format: SampleFormat, b: &[u8]) -> i16 {
    match format {
        SampleFormat::U8 => ((b[0] as i16) - 128) << 8,
        SampleFormat::S16 => i16::from_le_bytes([b[0], b[1]]),
        SampleFormat::S24 => i16::from_le_bytes([b[1], b[2]]),
        SampleFormat::S32 => i16::from_le_bytes([b[2], b[3]]),
        _ => crate::mix::f32_to_i16(sample_f32(format, b)),
    }
}

/// Converts frames `start..` of the file `bytes` (described by `info`) to
/// interleaved stereo `i16` in `out`. Returns the number of frames written
/// (fewer than `out.len() / 2` at the end of the data).
pub fn read_stereo(info: &WavInfo, bytes: &[u8], start: u64, out: &mut [i16]) -> usize {
    let ba = info.block_align as usize;
    let avail_end = (info.data_offset + info.data_len).min(bytes.len());
    if start >= info.frames {
        return 0;
    }
    let first = info.data_offset + start as usize * ba;
    if first >= avail_end {
        return 0;
    }
    let frames = ((avail_end - first) / ba).min(out.len() / 2);
    let data = &bytes[first..first + frames * ba];
    let out = &mut out[..frames * 2];
    let sb = info.format.bytes();
    match (info.format, info.channels) {
        (SampleFormat::S16, 2) => {
            for (o, b) in out.iter_mut().zip(data.as_chunks::<2>().0) {
                *o = i16::from_le_bytes([b[0], b[1]]);
            }
        }
        (SampleFormat::S16, 1) => {
            for (o, b) in out.as_chunks_mut::<2>().0.iter_mut().zip(data.as_chunks::<2>().0) {
                let s = i16::from_le_bytes([b[0], b[1]]);
                o[0] = s;
                o[1] = s;
            }
        }
        (f, 1) => {
            for (o, b) in out.as_chunks_mut::<2>().0.iter_mut().zip(data.chunks_exact(sb)) {
                let s = sample_i16(f, b);
                o[0] = s;
                o[1] = s;
            }
        }
        (f, 2) => {
            for (o, b) in out.iter_mut().zip(data.chunks_exact(sb)) {
                *o = sample_i16(f, b);
            }
        }
        (f, ch) => {
            let w = downmix_weights(ch as usize);
            for (o, frame) in out.as_chunks_mut::<2>().0.iter_mut().zip(data.chunks_exact(ba)) {
                let (mut l, mut r) = (0.0f32, 0.0f32);
                for (c, b) in frame.chunks_exact(sb).enumerate().take(8) {
                    let s = sample_f32(f, b);
                    l += s * w[c].0;
                    r += s * w[c].1;
                }
                o[0] = crate::mix::f32_to_i16(l);
                o[1] = crate::mix::f32_to_i16(r);
            }
        }
    }
    frames
}

fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() & 1 == 1 {
        out.push(0);
    }
}

/// Builds a `LIST/INFO` chunk body from tags (empty if there are none).
fn info_list(tags: &Tags) -> Vec<u8> {
    let mut body = Vec::new();
    let fields: [(&[u8; 4], String); 5] = [
        (b"INAM", tags.title.clone()),
        (b"IART", tags.artist.clone()),
        (b"IPRD", tags.album.clone()),
        (b"IGNR", tags.genre.clone()),
        (b"ICRD", if tags.year != 0 { alloc::format!("{}", tags.year) } else { String::new() }),
    ];
    for (id, text) in fields.iter() {
        if !text.is_empty() {
            let mut t = text.clone().into_bytes();
            t.push(0);
            push_chunk(&mut body, id, &t);
        }
    }
    if tags.track != 0 {
        let mut t = alloc::format!("{}", tags.track).into_bytes();
        t.push(0);
        push_chunk(&mut body, b"ITRK", &t);
    }
    if body.is_empty() {
        return body;
    }
    let mut list = b"INFO".to_vec();
    list.extend_from_slice(&body);
    list
}

/// Writes interleaved 16-bit PCM as a WAV file (with a `LIST/INFO` chunk
/// when `tags` has fields).
pub fn write(samples: &[i16], channels: u16, rate: u32, tags: &Tags) -> Vec<u8> {
    let channels = channels.max(1);
    let mut fmt = Vec::with_capacity(16);
    fmt.extend_from_slice(&FORMAT_PCM.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    fmt.extend_from_slice(&(channels * 2).to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    let mut body = b"WAVE".to_vec();
    push_chunk(&mut body, b"fmt ", &fmt);
    let list = info_list(tags);
    if !list.is_empty() {
        push_chunk(&mut body, b"LIST", &list);
    }
    let mut data = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        data.extend_from_slice(&s.to_le_bytes());
    }
    push_chunk(&mut body, b"data", &data);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn header(tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let ba = channels * bits.div_ceil(8);
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * ba as u32).to_le_bytes());
        fmt.extend_from_slice(&ba.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        let mut body = b"WAVE".to_vec();
        push_chunk(&mut body, b"fmt ", &fmt);
        push_chunk(&mut body, b"data", data);
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn roundtrip_16bit_with_tags() {
        let samples: Vec<i16> = (0..200).map(|i| (i * 100 - 10_000) as i16).collect();
        let tags =
            Tags { title: "Test".into(), artist: "Me".into(), album: "LP".into(), year: 1999, ..Default::default() };
        let file = write(&samples, 2, 44_100, &tags);
        let info = WavInfo::parse(&file).unwrap();
        assert_eq!((info.rate, info.channels, info.frames), (44_100, 2, 100));
        assert_eq!(info.tags.title, "Test");
        assert_eq!(info.tags.album, "LP");
        assert_eq!(info.tags.year, 1999);
        let mut out = vec![0i16; 400];
        assert_eq!(read_stereo(&info, &file, 0, &mut out), 100);
        assert_eq!(&out[..200], &samples[..]);
        assert_eq!(read_stereo(&info, &file, 99, &mut out), 1);
        assert_eq!(read_stereo(&info, &file, 100, &mut out), 0);
    }

    #[test]
    fn decodes_all_sample_formats() {
        // One mono frame at half scale in every format.
        let cases: [(u16, u16, Vec<u8>); 6] = [
            (FORMAT_PCM, 8, vec![192]),
            (FORMAT_PCM, 16, 16384i16.to_le_bytes().to_vec()),
            (FORMAT_PCM, 24, vec![0, 0, 64]),
            (FORMAT_PCM, 32, (1i32 << 30).to_le_bytes().to_vec()),
            (FORMAT_FLOAT, 32, 0.5f32.to_le_bytes().to_vec()),
            (FORMAT_FLOAT, 64, 0.5f64.to_le_bytes().to_vec()),
        ];
        for (tag, bits, data) in cases {
            let file = header(tag, 1, 8000, bits, &data);
            let info = WavInfo::parse(&file).unwrap();
            let mut out = [0i16; 2];
            assert_eq!(read_stereo(&info, &file, 0, &mut out), 1, "bits {bits}");
            assert!((out[0] as i32 - 16384).abs() <= 1, "bits {bits}: {}", out[0]);
            assert_eq!(out[0], out[1]);
        }
    }

    #[test]
    fn downmixes_surround() {
        let mut data = Vec::new();
        for v in [1000i16, -1000, 2000, 0, 0, 0] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let file = header(FORMAT_PCM, 6, 48_000, 16, &data);
        let info = WavInfo::parse(&file).unwrap();
        let mut out = [0i16; 2];
        assert_eq!(read_stereo(&info, &file, 0, &mut out), 1);
        assert!(out[0] > out[1]);
    }

    #[test]
    fn rejects_malformed_files() {
        assert_eq!(WavInfo::parse(b"RIFF"), Err(AudioError::UnknownFormat));
        assert_eq!(WavInfo::parse(b"RIFF\0\0\0\0WAVEjunk"), Err(AudioError::Malformed));
        let bad_rate = header(FORMAT_PCM, 2, 5, 16, &[0; 8]);
        assert_eq!(WavInfo::parse(&bad_rate), Err(AudioError::Unsupported));
        let adpcm = header(2, 2, 8000, 4, &[0; 8]);
        assert_eq!(WavInfo::parse(&adpcm), Err(AudioError::Unsupported));
        // Truncated data is clamped to what exists.
        let mut file = header(FORMAT_PCM, 2, 8000, 16, &[0; 400]);
        file.truncate(file.len() - 101);
        let info = WavInfo::parse(&file).unwrap();
        assert_eq!(info.frames, 74);
        // A huge chunk size must not overflow.
        let mut weird = header(FORMAT_PCM, 1, 8000, 16, &[0; 4]);
        weird.extend_from_slice(b"junk\xff\xff\xff\xff");
        assert!(WavInfo::parse(&weird).is_ok());
    }

    #[test]
    fn prefix_parse_reports_full_duration() {
        let samples = vec![0i16; 48_000 * 2];
        let file = write(&samples, 2, 48_000, &Tags::default());
        let info = WavInfo::parse_prefix(&file[..512], file.len() as u64).unwrap();
        assert_eq!(info.frames, 48_000);
        assert_eq!(info.duration_ms(), 1000);
    }
}
