//! Effects for offline mixing: a ping-pong delay, a Freeverb-style stereo
//! reverb, a chorus, a compressor with optional side-chain, a look-ahead
//! limiter and a soft saturator. All process interleaved stereo `f32`.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use vmath::FloatExt;

use super::filter::OnePole;
use super::osc::sin_turns;

/// Smooth saturation: linear near zero, approaching ±1.
#[inline(always)]
pub fn soft_clip(x: f32) -> f32 {
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

/// A stereo ping-pong delay with a darkening feedback path.
pub struct Delay {
    l: Vec<f32>,
    r: Vec<f32>,
    pos: usize,
    len: usize,
    pub feedback: f32,
    damp_l: OnePole,
    damp_r: OnePole,
}

impl Delay {
    /// `secs` of delay; `damp` is the cut-off of the feedback low-pass.
    pub fn new(secs: f32, feedback: f32, damp: f32, sr: f32) -> Delay {
        let len = ((secs * sr) as usize).max(1);
        Delay {
            l: vec![0.0; len],
            r: vec![0.0; len],
            pos: 0,
            len,
            feedback: feedback.clamp(0.0, 0.95),
            damp_l: OnePole::new(damp, sr),
            damp_r: OnePole::new(damp, sr),
        }
    }

    /// Processes a stereo frame, returning only the wet signal.
    #[inline]
    pub fn process(&mut self, inl: f32, inr: f32) -> (f32, f32) {
        let (dl, dr) = (self.l[self.pos], self.r[self.pos]);
        // Ping-pong: the left input feeds the left line, whose output feeds
        // the right line and vice versa.
        let fl = self.damp_l.process(dr * self.feedback);
        let fr = self.damp_r.process(dl * self.feedback);
        self.l[self.pos] = (inl + inr) * 0.5 + fl;
        self.r[self.pos] = fr;
        self.pos += 1;
        if self.pos == self.len {
            self.pos = 0;
        }
        (dl, dr)
    }
}

struct Comb {
    buf: Vec<f32>,
    pos: usize,
    store: f32,
}

impl Comb {
    #[inline(always)]
    fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.buf[self.pos];
        self.store = y * (1.0 - damp) + self.store * damp;
        self.buf[self.pos] = x + self.store * feedback;
        self.pos += 1;
        if self.pos == self.buf.len() {
            self.pos = 0;
        }
        y
    }
}

struct Allpass {
    buf: Vec<f32>,
    pos: usize,
}

impl Allpass {
    #[inline(always)]
    fn process(&mut self, x: f32) -> f32 {
        let b = self.buf[self.pos];
        let y = b - x;
        self.buf[self.pos] = x + b * 0.5;
        self.pos += 1;
        if self.pos == self.buf.len() {
            self.pos = 0;
        }
        y
    }
}

/// A Freeverb-style stereo reverb (8 damped combs and 4 all-passes per
/// channel) with pre-delay and input filtering.
pub struct Reverb {
    combs: [Vec<Comb>; 2],
    allpasses: [Vec<Allpass>; 2],
    pre: Vec<f32>,
    pre_pos: usize,
    feedback: f32,
    damp: f32,
    width: f32,
    hp: OnePole,
    lp: OnePole,
}

impl Reverb {
    /// `size` 0..1 (decay time), `damp` 0..1 (high-frequency absorption),
    /// `width` 0..1, `predelay` in seconds.
    pub fn new(size: f32, damp: f32, width: f32, predelay: f32, sr: f32) -> Reverb {
        const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
        const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
        let scale = sr / 44_100.0;
        let make = |spread: usize| -> (Vec<Comb>, Vec<Allpass>) {
            (
                COMBS
                    .iter()
                    .map(|&n| Comb { buf: vec![0.0; ((n + spread) as f32 * scale) as usize], pos: 0, store: 0.0 })
                    .collect(),
                ALLPASSES
                    .iter()
                    .map(|&n| Allpass { buf: vec![0.0; ((n + spread) as f32 * scale) as usize], pos: 0 })
                    .collect(),
            )
        };
        let (cl, al) = make(0);
        let (cr, ar) = make(23);
        let pre_len = ((predelay * sr) as usize).max(1);
        Reverb {
            combs: [cl, cr],
            allpasses: [al, ar],
            pre: vec![0.0; pre_len * 2],
            pre_pos: 0,
            feedback: 0.7 + 0.28 * size.clamp(0.0, 1.0),
            damp: damp.clamp(0.0, 1.0) * 0.4,
            width: width.clamp(0.0, 1.0),
            hp: OnePole::new(180.0, sr),
            lp: OnePole::new(9_000.0, sr),
        }
    }

    /// Processes one stereo frame; returns the wet signal.
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let n = self.pre.len() / 2;
        let (pl, pr) = (self.pre[self.pre_pos * 2], self.pre[self.pre_pos * 2 + 1]);
        self.pre[self.pre_pos * 2] = l;
        self.pre[self.pre_pos * 2 + 1] = r;
        self.pre_pos = (self.pre_pos + 1) % n;
        let input = (pl + pr) * 0.015;
        let input = self.hp.highpass(input);
        let input = self.lp.process(input);
        let mut out = [0.0f32; 2];
        for (ch, o) in out.iter_mut().enumerate() {
            let mut acc = 0.0;
            for c in self.combs[ch].iter_mut() {
                acc += c.process(input, self.feedback, self.damp);
            }
            for a in self.allpasses[ch].iter_mut() {
                acc = a.process(acc);
            }
            *o = acc;
        }
        let wet1 = 0.5 + self.width * 0.5;
        let wet2 = (1.0 - self.width) * 0.5;
        let (a, b) = (out[0] * 3.0, out[1] * 3.0);
        (a * wet1 + b * wet2, b * wet1 + a * wet2)
    }
}

/// A two-voice stereo chorus.
pub struct Chorus {
    buf: Vec<f32>,
    pos: usize,
    phase: f32,
    rate: f32,
    base: f32,
    depth: f32,
    pub mix: f32,
}

impl Chorus {
    pub fn new(rate_hz: f32, depth_ms: f32, mix: f32, sr: f32) -> Chorus {
        let len = (sr * 0.05) as usize;
        Chorus {
            buf: vec![0.0; len * 2],
            pos: 0,
            phase: 0.0,
            rate: rate_hz / sr,
            base: 0.012 * sr,
            depth: depth_ms * 0.001 * sr,
            mix: mix.clamp(0.0, 1.0),
        }
    }

    fn tap(&self, delay: f32, ch: usize) -> f32 {
        let n = self.buf.len() / 2;
        let p = self.pos as f32 - delay;
        let p = if p < 0.0 { p + n as f32 } else { p };
        let i = p as usize % n;
        let f = p - p.floor_f();
        let j = (i + 1) % n;
        self.buf[i * 2 + ch] * (1.0 - f) + self.buf[j * 2 + ch] * f
    }

    /// Processes one stereo frame (returns the mixed signal).
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let n = self.buf.len() / 2;
        self.buf[self.pos * 2] = l;
        self.buf[self.pos * 2 + 1] = r;
        let ml = self.base + self.depth * (0.5 + 0.5 * sin_turns(self.phase));
        let mr = self.base + self.depth * (0.5 + 0.5 * sin_turns(self.phase + 0.25));
        let wl = self.tap(ml, 0);
        let wr = self.tap(mr, 1);
        self.phase += self.rate;
        if self.phase > 1.0 {
            self.phase -= 1.0;
        }
        self.pos = (self.pos + 1) % n;
        (l * (1.0 - self.mix * 0.5) + wl * self.mix, r * (1.0 - self.mix * 0.5) + wr * self.mix)
    }
}

trait FloorF {
    fn floor_f(self) -> f32;
}

impl FloorF for f32 {
    #[inline(always)]
    fn floor_f(self) -> f32 {
        let t = self as i64 as f32;
        if t > self { t - 1.0 } else { t }
    }
}

/// A feed-forward compressor working on stereo-linked peak levels.
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    attack: f32,
    release: f32,
    env_db: f32,
    pub makeup: f32,
}

impl Compressor {
    pub fn new(threshold_db: f32, ratio: f32, attack_ms: f32, release_ms: f32, sr: f32) -> Compressor {
        let c = |ms: f32| FloatExt::exp(-1.0 / (ms.max(0.01) * 0.001 * sr));
        Compressor {
            threshold_db,
            ratio: ratio.max(1.0),
            knee_db: 6.0,
            attack: c(attack_ms),
            release: c(release_ms),
            env_db: 0.0,
            makeup: 1.0,
        }
    }

    /// Gain reduction (in dB, <= 0) for an input level in dB.
    fn curve(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        let slope = 1.0 / self.ratio - 1.0;
        if over <= -self.knee_db / 2.0 {
            0.0
        } else if over >= self.knee_db / 2.0 {
            over * slope
        } else {
            let x = over + self.knee_db / 2.0;
            slope * x * x / (2.0 * self.knee_db)
        }
    }

    /// The gain to apply given the detector input (peak of both channels).
    #[inline]
    pub fn gain(&mut self, detector: f32) -> f32 {
        let level_db = 20.0 * FloatExt::log10(detector.abs().max(1e-6));
        let target = self.curve(level_db);
        let coef = if target < self.env_db { self.attack } else { self.release };
        self.env_db = target + (self.env_db - target) * coef;
        FloatExt::powf(10.0f32, self.env_db / 20.0) * self.makeup
    }
}

/// A look-ahead brick-wall limiter: the output never exceeds `ceiling`.
pub struct Limiter {
    ceiling: f32,
    delay: VecDeque<(f32, f32)>,
    /// Required gains over the look-ahead window (monotonic deque of
    /// (index, gain) for a sliding minimum).
    window: VecDeque<(u64, f32)>,
    lookahead: usize,
    n: u64,
    /// The lowest gain applied so far (1 = no limiting).
    pub min_gain: f32,
    gain: f32,
    release: f32,
}

impl Limiter {
    pub fn new(ceiling_db: f32, lookahead_ms: f32, release_ms: f32, sr: f32) -> Limiter {
        let lookahead = ((lookahead_ms * 0.001 * sr) as usize).max(1);
        Limiter {
            ceiling: FloatExt::powf(10.0f32, ceiling_db / 20.0),
            delay: VecDeque::with_capacity(lookahead + 1),
            window: VecDeque::new(),
            lookahead,
            n: 0,
            min_gain: 1.0,
            gain: 1.0,
            release: FloatExt::exp(-1.0 / (release_ms * 0.001 * sr)),
        }
    }

    /// Pushes a frame; returns the limited frame `lookahead` samples later
    /// (silence while the delay line fills).
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let peak = l.abs().max(r.abs());
        let need = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
        while self.window.back().is_some_and(|&(_, g)| g >= need) {
            self.window.pop_back();
        }
        self.window.push_back((self.n, need));
        while self.window.front().is_some_and(|&(i, _)| i + self.lookahead as u64 <= self.n) {
            self.window.pop_front();
        }
        let target = self.window.front().map(|&(_, g)| g).unwrap_or(1.0);
        // Attack instantly (the look-ahead hides it), release smoothly.
        self.gain = if target < self.gain { target } else { target + (self.gain - target) * self.release };
        self.min_gain = self.min_gain.min(self.gain);
        self.delay.push_back((l, r));
        self.n += 1;
        if self.delay.len() > self.lookahead {
            let (dl, dr) = self.delay.pop_front().unwrap_or((0.0, 0.0));
            let g = self.gain;
            ((dl * g).clamp(-self.ceiling, self.ceiling), (dr * g).clamp(-self.ceiling, self.ceiling))
        } else {
            (0.0, 0.0)
        }
    }

    /// Frames of delay.
    pub fn latency(&self) -> usize {
        self.lookahead
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_never_exceeds_the_ceiling() {
        let mut lim = Limiter::new(-1.0, 3.0, 80.0, 44_100.0);
        let ceiling = FloatExt::powf(10.0f32, -1.0 / 20.0);
        let mut max = 0.0f32;
        for i in 0..44_100 {
            let x = 1.8 * (i as f32 * 0.05).sin() * if i % 5000 < 100 { 1.5 } else { 0.5 };
            let (l, r) = lim.process(x, -x);
            max = max.max(l.abs()).max(r.abs());
        }
        assert!(max <= ceiling + 1e-6, "{max}");
        assert!(max > ceiling * 0.9);
    }

    #[test]
    fn reverb_tail_decays() {
        let mut rv = Reverb::new(0.6, 0.5, 1.0, 0.01, 44_100.0);
        let mut energy_early = 0.0;
        let mut energy_late = 0.0;
        for i in 0..(44_100 * 4) {
            let x = if i == 0 { 1.0 } else { 0.0 };
            let (l, r) = rv.process(x, x);
            if (2_000..20_000).contains(&i) {
                energy_early += l * l + r * r;
            }
            if i > 44_100 * 3 {
                energy_late += l * l + r * r;
            }
        }
        assert!(energy_early > 0.0 && energy_late < energy_early * 0.01);
    }

    #[test]
    fn compressor_reduces_loud_signals() {
        let mut c = Compressor::new(-20.0, 4.0, 5.0, 100.0, 44_100.0);
        let mut g = 1.0;
        for _ in 0..10_000 {
            g = c.gain(1.0);
        }
        // 20 dB over the threshold at 4:1 -> 15 dB of reduction.
        let db = 20.0 * g.log10();
        assert!((db + 15.0).abs() < 0.5, "{db}");
    }
}
