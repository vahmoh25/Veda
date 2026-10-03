//! One streaming, seekable decoder interface over the supported formats.
//!
//! A [`Source`] owns (or borrows, through any `AsRef<[u8]>`) the bytes of a
//! WAV or QOA file and produces interleaved **stereo** `i16` frames at the
//! file's own sample rate (mono files are duplicated, surround files are
//! downmixed). [`probe`] reads the format, duration and tags of a file from
//! just its first and last few kilobytes, which is what a music library
//! scanner needs.

use alloc::vec;
use alloc::vec::Vec;

use crate::AudioError;
use crate::qoa::{self, QoaIndex};
use crate::tags::Tags;
use crate::wav::{self, WavInfo};

/// The container format of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Wav,
    Qoa,
}

impl Format {
    /// Guesses the format from the first bytes of a file.
    pub fn detect(head: &[u8]) -> Option<Format> {
        if qoa::is_qoa(head) {
            Some(Format::Qoa)
        } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE" {
            Some(Format::Wav)
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Wav => "WAV",
            Format::Qoa => "QOA",
        }
    }
}

/// What [`probe`] learns about a file without decoding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub format: Format,
    pub rate: u32,
    pub channels: usize,
    pub frames: u64,
    pub tags: Tags,
}

/// Reads format, length and tags from the beginning (`head`, at least the
/// first 4 KiB or the whole file) and end (`tail`, the last few KiB) of a
/// file of `file_len` bytes.
pub fn probe(head: &[u8], tail: &[u8], file_len: u64) -> Result<Probe, AudioError> {
    match Format::detect(head).ok_or(AudioError::UnknownFormat)? {
        Format::Qoa => {
            let info = qoa::probe(head)?;
            let mut frames = info.frames;
            if frames == 0 {
                // A streaming file: estimate from the size (full frames).
                let fs = qoa::frame_size(info.channels, qoa::SLICES_PER_FRAME) as u64;
                frames = file_len.saturating_sub(8) / fs * qoa::FRAME_LEN as u64;
            }
            let mut tags = Tags::from_trailer(tail).unwrap_or_default();
            tags.duration_ms = frames * 1000 / info.rate as u64;
            Ok(Probe { format: Format::Qoa, rate: info.rate, channels: info.channels, frames, tags })
        }
        Format::Wav => {
            let info = WavInfo::parse_prefix(head, file_len)?;
            let mut tags = info.tags.clone();
            if info.tail_offset != 0 {
                // Chunks after the data: the tail holds them if they are short.
                let tail_start = file_len.saturating_sub(tail.len() as u64);
                if (info.tail_offset as u64) >= tail_start {
                    let off = (info.tail_offset as u64 - tail_start) as usize;
                    tags.merge_missing(&wav::parse_tail_tags(&tail[off.min(tail.len())..]));
                }
            }
            tags.duration_ms = info.duration_ms();
            Ok(Probe {
                format: Format::Wav,
                rate: info.rate,
                channels: info.channels as usize,
                frames: info.frames,
                tags,
            })
        }
    }
}

enum Kind {
    Wav(WavInfo),
    Qoa {
        index: QoaIndex,
        /// Decoded frame (interleaved, file channel count).
        buf: Vec<i16>,
        /// Which frame `buf` holds.
        buf_frame: Option<usize>,
        buf_len: usize,
    },
}

/// A streaming decoder producing interleaved stereo `i16`.
pub struct Source<D: AsRef<[u8]>> {
    data: D,
    kind: Kind,
    format: Format,
    rate: u32,
    channels: usize,
    frames: u64,
    pos: u64,
    tags: Tags,
}

impl<D: AsRef<[u8]>> Source<D> {
    /// Opens a complete file.
    pub fn open(data: D) -> Result<Source<D>, AudioError> {
        let bytes = data.as_ref();
        let format = Format::detect(bytes).ok_or(AudioError::UnknownFormat)?;
        let (kind, rate, channels, frames, tags) = match format {
            Format::Wav => {
                let info = WavInfo::parse(bytes)?;
                let mut tags = info.tags.clone();
                if info.tail_offset != 0 {
                    tags.merge_missing(&wav::parse_tail_tags(&bytes[info.tail_offset..]));
                }
                let (rate, ch, frames) = (info.rate, info.channels as usize, info.frames);
                (Kind::Wav(info), rate, ch, frames, tags)
            }
            Format::Qoa => {
                let index = QoaIndex::build(bytes)?;
                let info = index.info();
                let tags = Tags::from_trailer(&bytes[index.stream_end().min(bytes.len())..]).unwrap_or_default();
                let buf = vec![0i16; qoa::FRAME_LEN * info.channels];
                (Kind::Qoa { index, buf, buf_frame: None, buf_len: 0 }, info.rate, info.channels, info.frames, tags)
            }
        };
        let mut tags = tags;
        tags.duration_ms = frames * 1000 / rate.max(1) as u64;
        Ok(Source { data, kind, format, rate, channels, frames, pos: 0, tags })
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// Sample rate of the file.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Channels stored in the file (output is always stereo).
    pub fn file_channels(&self) -> usize {
        self.channels
    }

    /// Length in frames.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Current position in frames.
    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn tags(&self) -> &Tags {
        &self.tags
    }

    /// Moves to frame `frame` (clamped to the end).
    pub fn seek(&mut self, frame: u64) {
        self.pos = frame.min(self.frames);
    }

    /// True when every frame has been read.
    pub fn at_end(&self) -> bool {
        self.pos >= self.frames
    }

    /// Reads up to `out.len() / 2` stereo frames; returns how many were
    /// read (0 at the end or on a decoding error).
    pub fn read(&mut self, out: &mut [i16]) -> usize {
        let want = out.len() / 2;
        let mut done = 0;
        while done < want && self.pos < self.frames {
            let bytes = self.data.as_ref();
            let n = match &mut self.kind {
                Kind::Wav(info) => wav::read_stereo(info, bytes, self.pos, &mut out[done * 2..want * 2]),
                Kind::Qoa { index, buf, buf_frame, buf_len } => {
                    let Some(fi) = index.frame_of(self.pos) else { break };
                    if *buf_frame != Some(fi) {
                        match index.decode_frame(bytes, fi, buf) {
                            Ok(n) => {
                                *buf_frame = Some(fi);
                                *buf_len = n;
                            }
                            Err(_) => break,
                        }
                    }
                    let start = (self.pos - index.frame_start(fi)) as usize;
                    let avail = buf_len.saturating_sub(start);
                    let n = avail.min(want - done);
                    let ch = self.channels;
                    let src = &buf[start * ch..(start + n) * ch];
                    let dst = &mut out[done * 2..(done + n) * 2];
                    match ch {
                        2 => dst.copy_from_slice(src),
                        1 => {
                            for (d, &s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(src) {
                                d[0] = s;
                                d[1] = s;
                            }
                        }
                        _ => {
                            for (d, s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(src.chunks_exact(ch)) {
                                d[0] = s[0];
                                d[1] = s[1];
                            }
                        }
                    }
                    n
                }
            };
            if n == 0 {
                break;
            }
            done += n;
            self.pos += n as u64;
        }
        done
    }

    /// Gives the underlying bytes back.
    pub fn into_inner(self) -> D {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, channels: usize) -> Vec<i16> {
        (0..frames * channels).map(|i| ((i / channels) as i32 * 37 % 20_000 - 10_000) as i16).collect()
    }

    #[test]
    fn wav_and_qoa_sources_agree_on_length_and_seek() {
        let pcm = tone(12_345, 2);
        let tags = Tags { title: "T".into(), artist: "A".into(), ..Default::default() };
        let wav_file = wav::write(&pcm, 2, 44_100, &tags);
        let mut qoa_file = qoa::encode(&pcm, 2, 44_100).unwrap();
        qoa_file.extend(tags.encode_trailer());
        for file in [&wav_file, &qoa_file] {
            let mut s = Source::open(file.as_slice()).unwrap();
            assert_eq!(s.frames(), 12_345);
            assert_eq!(s.rate(), 44_100);
            assert_eq!(s.tags().title, "T");
            assert_eq!(s.tags().duration_ms, 279);
            let mut out = vec![0i16; 1000 * 2];
            let mut total = 0;
            loop {
                let n = s.read(&mut out);
                if n == 0 {
                    break;
                }
                total += n;
            }
            assert_eq!(total, 12_345);
            assert!(s.at_end());
            s.seek(12_000);
            assert_eq!(s.read(&mut out), 345);
            s.seek(0);
            assert_eq!(s.read(&mut out), 1000);
        }
        // Seeking reproduces the same samples.
        let mut s = Source::open(qoa_file.as_slice()).unwrap();
        let mut all = vec![0i16; 12_345 * 2];
        assert_eq!(s.read(&mut all), 12_345);
        s.seek(5_555);
        let mut part = vec![0i16; 100 * 2];
        assert_eq!(s.read(&mut part), 100);
        assert_eq!(&part[..], &all[5_555 * 2..5_655 * 2]);
    }

    #[test]
    fn mono_is_duplicated() {
        let pcm = tone(100, 1);
        let file = wav::write(&pcm, 1, 22_050, &Tags::default());
        let mut s = Source::open(file).unwrap();
        let mut out = vec![0i16; 200];
        assert_eq!(s.read(&mut out), 100);
        assert!(out.chunks(2).zip(&pcm).all(|(f, &p)| f[0] == p && f[1] == p));
    }

    #[test]
    fn probe_reads_head_and_tail_only() {
        let pcm = tone(50_000, 2);
        let tags = Tags { title: "Song".into(), album: "LP".into(), ..Default::default() };
        let mut q = qoa::encode(&pcm, 2, 48_000).unwrap();
        q.extend(tags.encode_trailer());
        let p = probe(&q[..4096], &q[q.len() - 4096..], q.len() as u64).unwrap();
        assert_eq!((p.format, p.rate, p.frames), (Format::Qoa, 48_000, 50_000));
        assert_eq!(p.tags.title, "Song");
        assert_eq!(p.tags.duration_ms, 1041);
        let w = wav::write(&pcm, 2, 48_000, &tags);
        let p = probe(&w[..4096], &w[w.len() - 4096..], w.len() as u64).unwrap();
        assert_eq!((p.format, p.frames), (Format::Wav, 50_000));
        assert_eq!(p.tags.album, "LP");
        assert_eq!(probe(b"hello", b"", 5), Err(AudioError::UnknownFormat));
    }
}
