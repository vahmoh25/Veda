//! `audiocheck` — analyses a recording of Veda's audio output (the WAV
//! file QEMU writes in headless runs) to prove that playback works.
//!
//! ```text
//! audiocheck stats   REC.wav                 levels, silence, gaps, pitch per second
//! audiocheck tone    REC.wav FREQ [FROM TO]  phase continuity of a pure tone
//! audiocheck compare REC.wav REF.qoa|wav     aligns the recording with the source
//!                                            file: speed, drift, correlation, dropouts
//! ```

use std::process::ExitCode;

use vaudio::fft::RealFft;
use vaudio::resample::Resampler;
use vaudio::source::Source;
use vaudio::wav::{self, WavInfo};

/// A mono signal in floating point.
struct Signal {
    rate: u32,
    data: Vec<f32>,
}

fn load(path: &str) -> Result<Signal, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let mut src = Source::open(bytes.as_slice()).map_err(|e| format!("{path}: {e}"))?;
    let rate = src.rate();
    let mut pcm = vec![0i16; src.frames() as usize * 2];
    let n = src.read(&mut pcm);
    let data = pcm[..n * 2].chunks(2).map(|f| (f[0] as f32 + f[1] as f32) / 65536.0).collect();
    Ok(Signal { rate, data })
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

/// Dominant frequency of a block (Hann window, FFT, parabolic peak).
fn dominant(x: &[f32], rate: u32) -> f32 {
    let n = x.len().next_power_of_two() / 2 * 2;
    let n = n.min(x.len()).next_power_of_two().max(4);
    let mut fft = RealFft::new(n);
    let w = vaudio::fft::hann(n);
    let input: Vec<f32> = (0..n).map(|i| x.get(i).copied().unwrap_or(0.0) * w[i]).collect();
    let (mut re, mut im) = (vec![0.0; n / 2 + 1], vec![0.0; n / 2 + 1]);
    fft.forward(&input, &mut re, &mut im);
    let mag: Vec<f32> = re.iter().zip(&im).map(|(r, i)| (r * r + i * i).sqrt()).collect();
    let (k, _) = mag.iter().enumerate().skip(2).fold((0, 0.0f32), |b, (i, &m)| if m > b.1 { (i, m) } else { b });
    if k == 0 || k + 1 >= mag.len() {
        return 0.0;
    }
    let (a, b, c) = (mag[k - 1].ln(), mag[k].ln(), mag[k + 1].ln());
    let p = 0.5 * (a - c) / (a - 2.0 * b + c);
    (k as f32 + p) * rate as f32 / n as f32
}

/// Runs of near-silence (|x| below `thresh`) longer than `min_len`
/// samples, excluding leading and trailing silence.
fn silent_runs(x: &[f32], thresh: f32, min_len: usize) -> Vec<(usize, usize)> {
    let first = x.iter().position(|v| v.abs() >= thresh).unwrap_or(x.len());
    let last = x.iter().rposition(|v| v.abs() >= thresh).unwrap_or(0);
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &v) in x.iter().enumerate().take(last + 1).skip(first) {
        if v.abs() < thresh {
            start.get_or_insert(i);
        } else if let Some(s) = start.take()
            && i - s >= min_len
        {
            runs.push((s, i - s));
        }
    }
    runs
}

fn stats(path: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let info = WavInfo::parse(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let s = load(path)?;
    let secs = s.data.len() as f32 / s.rate as f32;
    let peak = s.data.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    println!("file: {} Hz, {} channel(s), {:?}, {:.2} s", info.rate, info.channels, info.format, secs);
    println!("level: peak {:.1} dBFS, RMS {:.1} dBFS", db(peak), db(rms(&s.data)));
    let mut pcm = vec![0i16; info.frames as usize * 2];
    let n = wav::read_stereo(&info, &bytes, 0, &mut pcm);
    let clipped = pcm[..n * 2].iter().filter(|&&v| v == i16::MAX || v == i16::MIN).count();
    println!("clipped samples: {clipped}");
    let block = s.rate as usize;
    let silent_blocks = s.data.chunks(block).filter(|b| rms(b) < 1e-4).count();
    println!("silent seconds: {silent_blocks} of {}", s.data.chunks(block).count());
    let runs = silent_runs(&s.data, 1.5 / 32768.0, s.rate as usize / 500);
    println!("silent gaps over 2 ms inside the audio: {}", runs.len());
    for (start, len) in runs.iter().take(10) {
        println!("  at {:.3} s for {:.1} ms", *start as f32 / s.rate as f32, *len as f32 * 1000.0 / s.rate as f32);
    }
    println!("per second: RMS dBFS / dominant frequency Hz");
    for (i, b) in s.data.chunks(block).enumerate() {
        println!("  {:3} s  {:6.1}  {:8.1}", i, db(rms(b)), dominant(&b[..b.len().min(16384)], s.rate));
    }
    Ok(())
}

/// Tracks the phase of a pure tone in 10 ms blocks and reports jumps.
fn tone(path: &str, freq: f64, from: f64, to: f64) -> Result<(), String> {
    let s = load(path)?;
    let rate = s.rate as f64;
    let (a, b) = ((from * rate) as usize, ((to * rate) as usize).min(s.data.len()));
    if b <= a + 1000 {
        return Err("range too short".into());
    }
    let block = (rate / 100.0) as usize;
    let mut phases = Vec::new();
    let mut amps = Vec::new();
    let mut i = a;
    while i + block <= b {
        let (mut sr, mut si) = (0f64, 0f64);
        for (k, &v) in s.data[i..i + block].iter().enumerate() {
            let ph = std::f64::consts::TAU * freq * (i + k) as f64 / rate;
            sr += v as f64 * ph.cos();
            si += v as f64 * ph.sin();
        }
        phases.push(si.atan2(sr));
        amps.push(2.0 * (sr * sr + si * si).sqrt() / block as f64);
        i += block;
    }
    // Unwrap and fit a line: the slope is the frequency error.
    let mut un = vec![phases[0]];
    for w in phases.windows(2) {
        let mut d = w[1] - w[0];
        while d > std::f64::consts::PI {
            d -= std::f64::consts::TAU;
        }
        while d < -std::f64::consts::PI {
            d += std::f64::consts::TAU;
        }
        un.push(un.last().unwrap() + d);
    }
    let n = un.len() as f64;
    let mx = (n - 1.0) / 2.0;
    let my = un.iter().sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0f64, 0f64);
    for (k, y) in un.iter().enumerate() {
        sxy += (k as f64 - mx) * (y - my);
        sxx += (k as f64 - mx).powi(2);
    }
    let slope = sxy / sxx; // radians per block
    let freq_err = slope / std::f64::consts::TAU * 100.0;
    let mut jumps = 0;
    let mut worst = 0f64;
    for (k, w) in un.windows(2).enumerate() {
        let d = (w[1] - w[0] - slope).abs();
        worst = worst.max(d);
        if d > 0.2 {
            jumps += 1;
            if jumps <= 10 {
                println!("  phase jump of {:.2} rad at {:.3} s", d, from + (k + 1) as f64 * 0.01);
            }
        }
    }
    let amp_min = amps.iter().cloned().fold(f64::MAX, f64::min);
    let amp_max = amps.iter().cloned().fold(0.0, f64::max);
    println!(
        "tone {freq} Hz over {:.1} s: measured {:.4} Hz ({:+.1} cents)",
        (b - a) as f64 / rate,
        freq + freq_err,
        1200.0 * ((freq + freq_err) / freq).log2()
    );
    println!(
        "amplitude {:.4} .. {:.4}; worst phase deviation {:.3} rad; jumps over 0.2 rad: {}",
        amp_min, amp_max, worst, jumps
    );
    if jumps == 0 { Ok(()) } else { Err(format!("{jumps} discontinuities")) }
}

/// Normalised cross-correlation of `a` and `b` (same length).
fn ncc(a: &[f32], b: &[f32]) -> f32 {
    let (mut ab, mut aa, mut bb) = (0f64, 0f64, 0f64);
    for (&x, &y) in a.iter().zip(b) {
        ab += x as f64 * y as f64;
        aa += x as f64 * x as f64;
        bb += y as f64 * y as f64;
    }
    if aa < 1e-12 || bb < 1e-12 {
        return 0.0;
    }
    (ab / (aa * bb).sqrt()) as f32
}

/// Best lag of `win` (from the recording) inside `reference` around `guess`.
fn best_lag(rec: &[f32], start: usize, len: usize, reference: &[f32], guess: i64, radius: i64) -> Option<(i64, f32)> {
    let win = rec.get(start..start + len)?;
    let mut best = (0i64, -2.0f32);
    for lag in guess - radius..=guess + radius {
        let s = start as i64 - lag;
        if s < 0 || s as usize + len > reference.len() {
            continue;
        }
        let c = ncc(win, &reference[s as usize..s as usize + len]);
        if c > best.1 {
            best = (lag, c);
        }
    }
    (best.1 > -2.0).then_some(best)
}

fn compare(rec_path: &str, ref_path: &str) -> Result<(), String> {
    let rec = load(rec_path)?;
    let reference = load(ref_path)?;
    // Bring the reference to the recording's rate.
    let ref_i16: Vec<i16> = reference.data.iter().map(|v| (v * 32767.0).round() as i16).collect();
    let conv = Resampler::process_all(reference.rate, rec.rate, 1, &ref_i16);
    let refd: Vec<f32> = conv.iter().map(|&v| v as f32 / 32768.0).collect();
    let rate = rec.rate as usize;
    // Coarse alignment: find where the recording starts in the reference.
    let first = rec.data.iter().position(|v| v.abs() > 0.003).ok_or("the recording is silent")?;
    let probe_len = rate / 5;
    let start = first + rate / 10;
    let mut best = (0i64, -1.0f32);
    let step = 16;
    let max_lag = (start as i64).min(refd.len() as i64);
    let mut lag = start as i64 - refd.len() as i64 + probe_len as i64;
    // Lag = rec index - ref index; search ref positions in coarse steps.
    while lag <= max_lag {
        let s = start as i64 - lag;
        if s >= 0 && s as usize + probe_len <= refd.len() {
            let c = ncc(&rec.data[start..start + probe_len], &refd[s as usize..s as usize + probe_len]);
            if c > best.1 {
                best = (lag, c);
            }
        }
        lag += step;
    }
    let (lag0, c0) = best_lag(&rec.data, start, probe_len, &refd, best.0, step * 2).ok_or("no alignment")?;
    println!(
        "alignment: recording sample {} = reference {:.3} s (correlation {:.4})",
        start,
        (start as i64 - lag0) as f64 / rate as f64,
        c0
    );
    // Track the lag through the recording in 100 ms windows.
    let win = rate / 10;
    let mut pos = start;
    let mut lag = lag0;
    let mut lags = Vec::new();
    let mut bad = 0;
    let mut low = Vec::new();
    while pos + win <= rec.data.len() {
        let s = pos as i64 - lag;
        if s < 0 || s as usize + win > refd.len() {
            break;
        }
        if rms(&rec.data[pos..pos + win]) < 1e-3 {
            pos += win;
            continue;
        }
        match best_lag(&rec.data, pos, win, &refd, lag, 48) {
            Some((l, c)) => {
                if l != lag {
                    println!(
                        "  lag changed by {} samples at {:.2} s (correlation {:.4})",
                        l - lag,
                        pos as f64 / rate as f64,
                        c
                    );
                }
                if c < 0.98 {
                    bad += 1;
                    low.push((pos, c));
                }
                lag = l;
                lags.push((pos, l, c));
            }
            None => break,
        }
        pos += win;
    }
    let n = lags.len();
    if n == 0 {
        return Err("nothing to compare".into());
    }
    let min_c = lags.iter().map(|x| x.2).fold(1.0f32, f32::min);
    let mean_c = lags.iter().map(|x| x.2).sum::<f32>() / n as f32;
    let drift = lags.last().unwrap().1 - lags[0].1;
    let span = (lags.last().unwrap().0 - lags[0].0) as f64 / rate as f64;
    println!("windows: {n} x 100 ms over {span:.2} s");
    println!("correlation with the source: mean {mean_c:.4}, minimum {min_c:.4}, windows below 0.98: {bad}");
    for (p, c) in low.iter().take(10) {
        println!("  low correlation {:.3} at {:.2} s", c, *p as f64 / rate as f64);
    }
    println!(
        "drift: {} samples over {:.1} s => speed error {:.4} %",
        drift,
        span,
        if span > 0.0 { drift as f64 / (span * rate as f64) * 100.0 } else { 0.0 }
    );
    // Gain: ratio of RMS (recording / reference) over the aligned span.
    let s0 = (lags[0].0 as i64 - lags[0].1) as usize;
    let len = (lags.last().unwrap().0 - lags[0].0).min(refd.len() - s0);
    let g = rms(&rec.data[lags[0].0..lags[0].0 + len]) / rms(&refd[s0..s0 + len]).max(1e-9);
    println!("level relative to the source: {:.2} dB", db(g));
    if bad == 0 && drift == 0 { Ok(()) } else { Err("the recording deviates from the source".into()) }
}

/// A magma-like colour map (0..1 -> 0xAARRGGBB).
fn heat(v: f32) -> u32 {
    let stops: [(f32, [f32; 3]); 5] = [
        (0.0, [0.0, 0.0, 0.02]),
        (0.3, [0.25, 0.05, 0.45]),
        (0.55, [0.75, 0.15, 0.45]),
        (0.8, [0.98, 0.55, 0.25]),
        (1.0, [1.0, 0.98, 0.75]),
    ];
    let v = v.clamp(0.0, 1.0);
    let i = stops.iter().position(|s| s.0 >= v).unwrap_or(4).max(1);
    let (a, b) = (stops[i - 1], stops[i]);
    let t = (v - a.0) / (b.0 - a.0);
    let c: Vec<u32> = (0..3).map(|k| ((a.1[k] + (b.1[k] - a.1[k]) * t) * 255.0) as u32).collect();
    0xFF00_0000 | c[0] << 16 | c[1] << 8 | c[2]
}

/// Draws a log-frequency spectrogram (40 Hz - 16 kHz) with a level strip.
fn spectrogram(path: &str, out: &str, from: f64, to: f64) -> Result<(), String> {
    let s = load(path)?;
    let rate = s.rate as f64;
    let a = (from * rate) as usize;
    let b = ((to * rate) as usize).min(s.data.len());
    if b <= a + 4096 {
        return Err("range too short".into());
    }
    let (w, h, strip) = (1400usize, 360usize, 60usize);
    let n = 4096;
    let mut fft = RealFft::new(n);
    let win = vaudio::fft::hann(n);
    let (mut re, mut im) = (vec![0.0f32; n / 2 + 1], vec![0.0f32; n / 2 + 1]);
    let mut img = vec![0xFF10_1014u32; w * (h + strip)];
    let bin_hz = rate / n as f64;
    for x in 0..w {
        let centre = a + (b - a) * x / w;
        let start = centre.saturating_sub(n / 2);
        let input: Vec<f32> = (0..n).map(|i| s.data.get(start + i).copied().unwrap_or(0.0) * win[i]).collect();
        fft.forward(&input, &mut re, &mut im);
        for y in 0..h {
            let frac = 1.0 - y as f64 / h as f64;
            let f = 40.0 * (16_000.0f64 / 40.0).powf(frac);
            let k = ((f / bin_hz) as usize).clamp(1, n / 2 - 1);
            let m = (re[k] * re[k] + im[k] * im[k]).sqrt() * 4.0 / n as f32;
            let v = (db(m) + 90.0) / 80.0;
            img[(strip + y) * w + x] = heat(v);
        }
        // Level strip: RMS of the 50 ms around this column.
        let half = (rate * 0.025) as usize;
        let lo = centre.saturating_sub(half);
        let level = (db(rms(&s.data[lo..(centre + half).min(s.data.len())])) + 60.0) / 60.0;
        let bar = (level.clamp(0.0, 1.0) * (strip - 4) as f32) as usize;
        for y in 0..bar {
            img[(strip - 2 - y) * w + x] = 0xFF5B_8CFF;
        }
    }
    let image = vimage::Image { width: w as u32, height: (h + strip) as u32, pixels: img };
    let png = vimage::encode(&image, vimage::Format::Png).map_err(|e| format!("{e:?}"))?;
    std::fs::write(out, png).map_err(|e| format!("{out}: {e}"))?;
    println!("wrote {out} ({:.1} s to {:.1} s)", a as f64 / rate, b as f64 / rate);
    Ok(())
}

/// Finds where a window of the recording lies in a reference: a coarse
/// global search (stride 8) refined to the exact sample.
fn locate(win: &[f32], reference: &[f32]) -> Option<(usize, f32)> {
    if reference.len() < win.len() {
        return None;
    }
    // Decimated envelope-free search on every 8th sample, then refine.
    let step = 8;
    let dec: Vec<f32> = win.iter().step_by(step).copied().collect();
    let mut best = (0usize, -2.0f32);
    let mut s = 0;
    while s + win.len() <= reference.len() {
        let r: Vec<f32> = reference[s..s + win.len()].iter().step_by(step).copied().collect();
        let c = ncc(&dec, &r);
        if c > best.1 {
            best = (s, c);
        }
        s += step / 2;
    }
    let lo = best.0.saturating_sub(step * 2);
    let hi = (best.0 + step * 2).min(reference.len() - win.len());
    let mut fine = (best.0, -2.0f32);
    for s in lo..=hi {
        let c = ncc(win, &reference[s..s + win.len()]);
        if c > fine.1 {
            fine = (s, c);
        }
    }
    Some(fine)
}

/// Maps every 100 ms of a recording onto the reference files: prints the
/// continuous segments (which track, where), seeks and silences.
fn timeline(rec_path: &str, refs: &[String]) -> Result<(), String> {
    let rec = load(rec_path)?;
    let rate = rec.rate as usize;
    let mut tracks = Vec::new();
    for r in refs {
        let s = load(r)?;
        let i16s: Vec<i16> = s.data.iter().map(|v| (v * 32767.0).round() as i16).collect();
        let conv = Resampler::process_all(s.rate, rec.rate, 1, &i16s);
        let name = std::path::Path::new(r).file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        tracks.push((name, conv.iter().map(|&v| v as f32 / 32768.0).collect::<Vec<f32>>()));
    }
    let win = rate / 10;
    let mut pos = 0;
    // Current segment: (track, rec start, ref start, windows, min corr).
    let mut seg: Option<(usize, usize, usize, usize, f32)> = None;
    let mut silent_since: Option<usize> = None;
    let mut bad = 0;
    let mut unmatched_run = 0;
    let fmt = |samples: usize| samples as f64 / rate as f64;
    let flush = |seg: &mut Option<(usize, usize, usize, usize, f32)>, end: usize| {
        if let Some((t, r0, f0, n, c)) = seg.take() {
            println!(
                "  {:7.2}-{:7.2} s  {:<16} {:7.2}-{:7.2} s  ({} windows, min correlation {:.4})",
                fmt(r0),
                fmt(end),
                tracks[t].0,
                fmt(f0),
                fmt(f0 + (end - r0)),
                n,
                c
            );
        }
    };
    println!("recording -> source");
    while pos + win <= rec.data.len() {
        let w = &rec.data[pos..pos + win];
        if rms(w) < 2e-4 {
            if silent_since.is_none() {
                flush(&mut seg, pos);
                silent_since = Some(pos);
            }
            // A fade into silence and the fade-in after it are two separate
            // transitions.
            unmatched_run = 0;
            pos += win;
            continue;
        }
        if let Some(s) = silent_since.take() {
            println!("  {:7.2}-{:7.2} s  silence", fmt(s), fmt(pos));
        }
        // Continue the current segment if it still matches.
        if let Some((t, r0, f0, n, c)) = seg {
            let expect = f0 + (pos - r0);
            let refd = &tracks[t].1;
            if expect + win <= refd.len() {
                let cc = ncc(w, &refd[expect..expect + win]);
                if cc > 0.95 {
                    seg = Some((t, r0, f0, n + 1, c.min(cc)));
                    pos += win;
                    continue;
                }
            }
            flush(&mut seg, pos);
        }
        // Search every track for this window.
        let mut best: Option<(usize, usize, f32)> = None;
        for (t, (_, refd)) in tracks.iter().enumerate() {
            if let Some((s, c)) = locate(w, refd)
                && best.is_none_or(|b| c > b.2)
            {
                best = Some((t, s, c));
            }
        }
        match best {
            Some((t, s, c)) if c > 0.9 => {
                seg = Some((t, pos, s, 1, c));
                unmatched_run = 0;
            }
            _ => {
                // A single window straddling a seek, a fade-in or a track
                // change matches nothing; two in a row means corruption.
                unmatched_run += 1;
                if unmatched_run >= 2 {
                    bad += 1;
                }
                println!("  {:7.2} s  {}", fmt(pos), if unmatched_run >= 2 { "UNMATCHED" } else { "transition" });
            }
        }
        pos += win;
    }
    flush(&mut seg, pos);
    if let Some(s) = silent_since {
        println!("  {:7.2}-{:7.2} s  silence", fmt(s), fmt(pos));
    }
    if bad == 0 { Ok(()) } else { Err(format!("{bad} windows match no source")) }
}

/// Long-term octave-band energy relative to the 1 kHz octave.
fn balance(path: &str) -> Result<(), String> {
    let s = load(path)?;
    let n = 8192;
    let mut fft = RealFft::new(n);
    let win = vaudio::fft::hann(n);
    let (mut re, mut im) = (vec![0.0f32; n / 2 + 1], vec![0.0f32; n / 2 + 1]);
    let centres = [31.5f64, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
    let mut energy = vec![0f64; centres.len()];
    let bin_hz = s.rate as f64 / n as f64;
    let mut pos = 0;
    while pos + n <= s.data.len() {
        let input: Vec<f32> = (0..n).map(|i| s.data[pos + i] * win[i]).collect();
        fft.forward(&input, &mut re, &mut im);
        for (k, (r, i)) in re.iter().zip(&im).enumerate().skip(1) {
            let f = k as f64 * bin_hz;
            if let Some(b) = centres.iter().position(|&c| f >= c / 2f64.sqrt() && f < c * 2f64.sqrt()) {
                energy[b] += (r * r + i * i) as f64;
            }
        }
        pos += n / 2;
    }
    let refe = energy[5].max(1e-30);
    let line: Vec<String> = centres
        .iter()
        .zip(&energy)
        .map(|(c, e)| {
            format!(
                "{}:{:+.1}",
                if *c >= 1000.0 { format!("{}k", c / 1000.0) } else { format!("{c}") },
                10.0 * (e / refe).log10()
            )
        })
        .collect();
    println!("{}", line.join("  "));
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("balance") if args.len() >= 2 => balance(&args[1]),
        Some("timeline") if args.len() >= 3 => timeline(&args[1], &args[2..]),
        Some("spectrogram") if args.len() >= 3 => {
            let from = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let to = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(1e9);
            spectrogram(&args[1], &args[2], from, to)
        }
        Some("stats") if args.len() >= 2 => stats(&args[1]),
        Some("tone") if args.len() >= 3 => {
            let f: f64 = args[2].parse().unwrap_or(440.0);
            let from = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let to = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(1e9);
            tone(&args[1], f, from, to)
        }
        Some("compare") if args.len() >= 3 => compare(&args[1], &args[2]),
        _ => Err("usage: audiocheck stats REC.wav | tone REC.wav FREQ [FROM TO] | compare REC.wav REF".into()),
    };
    match r {
        Ok(()) => {
            println!("result: PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("result: FAIL ({e})");
            ExitCode::FAILURE
        }
    }
}
