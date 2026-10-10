//! Keeping the agent from hearing itself.
//!
//! The microphone is echo-cancelled in the audio service, but no canceller
//! removes everything: loudspeakers and microphones a hand apart (a
//! laptop), a hypervisor's long audio path, the first second before the
//! canceller has learnt the room. What is left of the agent's own voice is
//! still speech to the recogniser, which then hears "the user" start
//! talking: the agent stops mid-sentence and answers words nobody said.
//!
//! [`EchoGate`] decides, for every 20 ms of microphone audio while the
//! agent's voice is audible (and for its echo's length after), whether it
//! is sent or replaced with silence. It is sent only when it is clearly
//! louder than the echo could be — the user talking over the agent — which
//! is how a person barges in anyway. The echo expected is that of
//! everything playing (the voice, and music under it, say). Music on its
//! own goes through: it holds none of the agent's words, and holding it
//! back would make talking to the agent over it hard.
//!
//! The echo's loudness is learnt while sound plays: how much louder or
//! quieter the microphone is than what is playing, at the loudest, slowly
//! forgotten. An echo is not steady: a laptop that cancels it in its own
//! audio path lets a burst through at the start of each sentence, before
//! it adapts, and is nearly silent in between — so the loudest echo heard
//! is remembered across sentences, not for a moment. Before anything is
//! learnt the gate assumes the echo may be a little louder than the sound
//! itself (a microphone's gain can be high).
//! It is learnt twice: in the echo-cancelled microphone (what the
//! recogniser hears) and in the microphone as recorded. Opening takes both:
//! the cancelled microphone clearly above its echo, and the recorded one
//! above its own. A canceller that loses track of the echo for a moment
//! makes the first louder but not the second; only someone talking adds
//! sound to the room.
//!
//! Only echo teaches the gate. While the recorded microphone is louder than
//! its echo can be, nothing is learnt until that ends: a burst shorter than
//! speech was echo after all, anything longer someone talking — whom the
//! canceller may hold back for a moment (it suppresses what starts while
//! the agent talks), so that the cancelled microphone does not show it yet.
//! Learning that as echo would set the echo expected above the voice,
//! which then could never open the gate.
//!
//! Levels are in dBFS, computed by the caller.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Packets of 20 ms.
const PACKET_MS: u32 = 20;
/// How long the sound played is taken to echo: playback and capture
/// latencies plus the room (a hypervisor adds hundreds of milliseconds).
const ECHO_SPAN: usize = (900 / PACKET_MS) as usize;
/// How fast the loudest echo heard is forgotten, per packet of sound
/// playing (a quarter of a dB a second).
const FORGET_DB: f32 = PACKET_MS as f32 / 4000.0;
/// The echo's loudness is trusted once it has been heard this long.
const WARM_UP_PACKETS: u32 = 1000 / PACKET_MS;
/// Below this nothing counts as playing (or the voice as audible).
const PLAYING_FLOOR_DB: f32 = -60.0;
/// The echo-cancelled microphone must be this much louder than its
/// expected echo ...
const MARGIN_DB: f32 = 6.0;
/// ... the recorded microphone this much louder than its own ...
const RAW_MARGIN_DB: f32 = 2.0;
/// ... for this many packets in a row (speech, not a click) ...
const OPEN_PACKETS: u32 = 5;
/// ... and louder than this at all.
const SPEECH_FLOOR_DB: f32 = -50.0;
/// Once open, the gate stays open this long after the speech was last
/// loud enough.
const HANGOVER_PACKETS: u32 = 600 / PACKET_MS;
/// The echo is assumed this much louder than what plays until learnt ...
const UNKNOWN_COUPLING_DB: f32 = 6.0;
/// ... and never taken to be louder than this (any louder, and nothing the
/// user says could be told from it).
const MAX_COUPLING_DB: f32 = 12.0;
/// Audio kept from before the gate opens, so the user's first words are
/// not lost.
pub const PREROLL_PACKETS: usize = (300 / PACKET_MS) as usize;

/// What the gate did with one packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing plays: send the microphone.
    Pass,
    /// Echo (or too little to tell): send silence.
    Hold,
    /// The user talks over the agent: send what was held back from just
    /// before (the pre-roll), then the microphone.
    Open,
    /// Still open (the user is talking): send the microphone.
    Through,
}

/// How loud an echo is relative to what plays (dB): the loudest learnt,
/// slowly forgotten.
#[derive(Debug, Clone)]
struct Coupling {
    peak: f32,
    heard: u32,
}

impl Coupling {
    fn new() -> Coupling {
        Coupling { peak: -80.0, heard: 0 }
    }

    fn learn(&mut self, db: f32) {
        self.peak = db.max(self.peak - FORGET_DB);
        self.heard = self.heard.saturating_add(1);
    }

    /// The loudest heard, or the cautious guess until a second of it has
    /// been heard.
    fn estimate(&self) -> f32 {
        if self.heard < WARM_UP_PACKETS {
            return UNKNOWN_COUPLING_DB.max(self.peak);
        }
        self.peak.clamp(-80.0, MAX_COUPLING_DB)
    }
}

#[derive(Debug, Clone)]
pub struct EchoGate {
    /// The level of the agent's voice and of everything playing, packet by
    /// packet, over the echo span.
    voice: VecDeque<f32>,
    playing: VecDeque<f32>,
    /// The echo in the cancelled microphone and in the recorded one.
    coupling: Coupling,
    raw_coupling: Coupling,
    /// The current run of packets in which the recorded microphone is
    /// louder than its echo can be, as (cancelled, recorded) levels
    /// relative to what plays — the first `OPEN_PACKETS` of them: echo
    /// after all if the run ends sooner.
    burst: Vec<(f32, f32)>,
    /// Packets in a row that sound like the user.
    talking: u32,
    hangover: u32,
    open: bool,
    /// The cancelled echo expected for the last packet (dBFS).
    expected_db: f32,
    /// Packets held back (silenced) and openings, for the log.
    pub held: u64,
    pub openings: u64,
}

impl Default for EchoGate {
    fn default() -> EchoGate {
        EchoGate::new()
    }
}

impl EchoGate {
    pub fn new() -> EchoGate {
        EchoGate {
            voice: VecDeque::with_capacity(ECHO_SPAN),
            playing: VecDeque::with_capacity(ECHO_SPAN),
            coupling: Coupling::new(),
            raw_coupling: Coupling::new(),
            burst: Vec::with_capacity(OPEN_PACKETS as usize),
            talking: 0,
            hangover: 0,
            open: false,
            expected_db: -100.0,
            held: 0,
            openings: 0,
        }
    }

    /// How loud the echo is taken to be relative to what plays (dB): in
    /// the cancelled microphone and in the recorded one.
    pub fn coupling_db(&self) -> (f32, f32) {
        (self.coupling.estimate(), self.raw_coupling.estimate())
    }

    /// The loudest echo expected in the cancelled microphone for the last
    /// packet (dBFS).
    pub fn expected_db(&self) -> f32 {
        self.expected_db
    }

    /// The agent's voice may be echoing right now.
    pub fn echoing(&self) -> bool {
        self.voice.iter().any(|&v| v > PLAYING_FLOOR_DB)
    }

    /// Letting the microphone through because the user talks.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Takes the levels of 20 ms of echo-cancelled microphone audio, of the
    /// same 20 ms as recorded (before cancellation; the cancelled level
    /// again if unknown), of everything that was playing at the time and of
    /// the agent's voice in it.
    pub fn packet(&mut self, mic_db: f32, raw_db: f32, playing_db: f32, voice_db: f32) -> Verdict {
        for (window, db) in [(&mut self.voice, voice_db), (&mut self.playing, playing_db)] {
            if window.len() == ECHO_SPAN {
                window.pop_front();
            }
            window.push_back(db);
        }
        let loudest = self.playing.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if !self.echoing() || loudest <= PLAYING_FLOOR_DB {
            // The voice quiet for the whole echo span: none of it can be in
            // the microphone.
            self.open = false;
            self.burst.clear();
            self.talking = 0;
            self.expected_db = -100.0;
            return Verdict::Pass;
        }
        let expected = loudest + self.coupling.estimate();
        let expected_raw = loudest + self.raw_coupling.estimate();
        self.expected_db = expected;
        // Someone talking adds sound to the room; a canceller that lost
        // track of the echo does not.
        let loud = raw_db > expected_raw + RAW_MARGIN_DB;
        let speech = mic_db > SPEECH_FLOOR_DB && mic_db > expected + MARGIN_DB && loud;
        if self.open {
            if speech || mic_db > expected + MARGIN_DB / 2.0 {
                self.hangover = HANGOVER_PACKETS;
            } else if self.hangover > 0 {
                self.hangover -= 1;
            }
            if self.hangover > 0 {
                return Verdict::Through;
            }
            self.open = false;
        }
        self.talking = if speech { self.talking + 1 } else { 0 };
        if self.talking >= OPEN_PACKETS {
            self.open = true;
            self.hangover = HANGOVER_PACKETS;
            self.openings += 1;
            self.talking = 0;
            self.burst.clear();
            return Verdict::Open;
        }
        if loud {
            // Someone talking, or a burst of echo louder than any before:
            // which one shows when it ends.
            if self.burst.len() < OPEN_PACKETS as usize {
                self.burst.push((mic_db - loudest, raw_db - loudest));
            }
        } else {
            // Holding: what the microphone hears now is (at most) echo,
            // which teaches the gate how loud the echo gets. So was a burst
            // too short to be speech: the echo can be that loud.
            if self.burst.len() < OPEN_PACKETS as usize {
                for &(c, r) in &self.burst {
                    self.coupling.learn(c);
                    self.raw_coupling.learn(r);
                }
            }
            self.burst.clear();
            self.coupling.learn(mic_db - loudest);
            self.raw_coupling.learn(raw_db - loudest);
        }
        self.held += 1;
        Verdict::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Sound at -20 dBFS (with dips between syllables); its echo `coupling`
    /// dB louder in the cancelled microphone and `raw` dB louder in the
    /// recorded one, `delay` packets later; and the user's speech level,
    /// packet by packet.
    fn run(
        gate: &mut EchoGate,
        packets: usize,
        delay: usize,
        coupling: impl Fn(usize) -> f32,
        raw: impl Fn(usize) -> f32,
        user: impl Fn(usize) -> f32,
    ) -> Vec<Verdict> {
        run_held_back(gate, packets, delay, coupling, raw, user, |_| 0.0)
    }

    /// The same, with the canceller taking `held_back` dB off the user's
    /// voice, packet by packet.
    fn run_held_back(
        gate: &mut EchoGate,
        packets: usize,
        delay: usize,
        coupling: impl Fn(usize) -> f32,
        raw: impl Fn(usize) -> f32,
        user: impl Fn(usize) -> f32,
        held_back: impl Fn(usize) -> f32,
    ) -> Vec<Verdict> {
        let playing = |i: usize| {
            if i >= packets.saturating_sub(10) {
                -100.0
            } else if i % 9 < 3 {
                -35.0
            } else {
                -20.0
            }
        };
        let power = |db: f32| 10f32.powf(db / 10.0);
        let sum = |a: f32, b: f32| 10.0 * (power(a) + power(b) + 1e-10).log10();
        (0..packets)
            .map(|i| {
                let source = if i >= delay { playing(i - delay) } else { -100.0 };
                let mic = sum(source + coupling(i), user(i) - held_back(i));
                let recorded = sum(source + raw(i), user(i));
                gate.packet(mic, recorded, playing(i), playing(i))
            })
            .collect()
    }

    #[test]
    fn passes_when_nothing_plays() {
        let mut g = EchoGate::new();
        for _ in 0..50 {
            assert_eq!(g.packet(-30.0, -30.0, -100.0, -100.0), Verdict::Pass);
        }
        assert!(!g.echoing());
    }

    #[test]
    fn music_on_its_own_goes_through() {
        let mut g = EchoGate::new();
        for i in 0..200 {
            let music = if i % 9 < 3 { -30.0 } else { -18.0 };
            assert_eq!(g.packet(-35.0, -30.0, music, -100.0), Verdict::Pass);
        }
        assert!(!g.echoing());
    }

    #[test]
    fn the_voice_over_music_expects_the_echo_of_both() {
        // Music louder than the voice: its echo (6 dB under it) is far
        // above the voice's own, and holding on to the voice alone would
        // take it for the user.
        let mut g = EchoGate::new();
        let mut opened = 0;
        for _ in 0..300 {
            if g.packet(-18.0, -18.0, -12.0, -24.0) == Verdict::Open {
                opened += 1;
            }
        }
        assert_eq!(opened, 0);
    }

    #[test]
    fn holds_back_loud_echo_from_the_first_word() {
        // Strong echo (only 3 dB quieter than the sound), late (400 ms): the
        // cautious guess covers the first second, the learnt echo the rest.
        let mut g = EchoGate::new();
        let v = run(&mut g, 300, 20, |_| -3.0, |_| -3.0, |_| -100.0);
        assert!(v[..290].iter().all(|&x| x == Verdict::Hold), "the echo leaked: {v:?}");
        assert_eq!(g.openings, 0);
        // After the sound and its echo are over, the microphone passes again.
        for _ in 0..ECHO_SPAN {
            g.packet(-70.0, -70.0, -100.0, -100.0);
        }
        assert_eq!(g.packet(-70.0, -70.0, -100.0, -100.0), Verdict::Pass);
    }

    #[test]
    fn the_user_can_talk_over_the_agent() {
        // A moderately cancelled echo (-18 dB, -4 dB as recorded); the user
        // starts talking at -12 dBFS after three seconds.
        let mut g = EchoGate::new();
        let v = run(&mut g, 400, 10, |_| -18.0, |_| -4.0, |i| if i >= 150 { -12.0 } else { -100.0 });
        assert!(v[..150].iter().all(|&x| x == Verdict::Hold));
        let opened = v.iter().position(|&x| x == Verdict::Open).expect("the gate never opened");
        assert!((150..160).contains(&opened), "opened at packet {opened}");
        assert!(v[opened + 1..380].iter().all(|&x| x == Verdict::Through), "{:?}", &v[opened..]);
    }

    #[test]
    fn the_user_held_back_at_first_is_not_learnt_as_echo() {
        // The user talks in syllables (400 ms of every second) at -10 dBFS
        // from packet 150. The canceller suppresses every fourth packet of
        // the first syllable (by 30 dB, echo and voice), so that it never
        // sounds like speech for long enough; that is no echo to learn, and
        // the next syllable opens the gate.
        let mut g = EchoGate::new();
        let talking = |i: usize| i >= 150 && (i - 150) % 50 < 20;
        let suppressed = |i: usize| talking(i) && i < 200 && i.is_multiple_of(4);
        let v = run_held_back(
            &mut g,
            400,
            10,
            |i| if suppressed(i) { -48.0 } else { -18.0 },
            |_| -4.0,
            |i| if talking(i) { -10.0 } else { -100.0 },
            |i| if suppressed(i) { 30.0 } else { 0.0 },
        );
        let opened = v.iter().position(|&x| x == Verdict::Open).expect("the gate never opened");
        assert!((200..210).contains(&opened), "opened at packet {opened}: {v:?}");
        assert!(g.coupling_db().0 < -12.0, "learnt {:?}", g.coupling_db());
    }

    #[test]
    fn quiet_talk_does_not_open_it_over_loud_echo() {
        let mut g = EchoGate::new();
        let v = run(&mut g, 300, 10, |_| -6.0, |_| -6.0, |i| if i >= 100 { -24.0 } else { -100.0 });
        assert_eq!(g.openings, 0, "{v:?}");
    }

    #[test]
    fn a_canceller_losing_track_does_not_open_it() {
        // The canceller removes 16 dB of a -2 dB echo, then for a second
        // removes nothing: the cancelled microphone jumps far above what
        // the gate learnt, the recorded one does not change.
        let mut g = EchoGate::new();
        let lost = |i: usize| if (200..250).contains(&i) { -2.0 } else { -18.0 };
        let v = run(&mut g, 400, 15, lost, |_| -2.0, |_| -100.0);
        assert_eq!(g.openings, 0, "{v:?}");
        assert!(v[..390].iter().all(|&x| x == Verdict::Hold));
    }

    #[test]
    fn echo_bursting_at_each_sentence_does_not_open_it() {
        // What a laptop that cancels the echo itself lets through: a burst
        // 400 ms into each sentence, nearly nothing for the rest. The
        // loudest burst comes first; later, quieter ones are still far
        // above the echo in between.
        let mut g = EchoGate::new();
        let sentences = [(0, -3.0), (250, -26.0), (500, -26.0), (900, -24.0)];
        let mut opened = Vec::new();
        for i in 0..1100 {
            let sentence = sentences.iter().find(|&&(start, _)| (start..start + 150).contains(&i));
            let playing = match sentence {
                Some(_) if i % 9 < 3 => -36.0,
                Some(_) => -16.0,
                None => -100.0,
            };
            let echo = match sentences.iter().find(|&&(start, _)| (start + 20..start + 170).contains(&i)) {
                Some(&(start, burst)) if i < start + 32 => -16.0 + burst,
                Some(_) => -65.0,
                None => -85.0,
            };
            if g.packet(echo, echo, playing, playing) == Verdict::Open {
                opened.push(i);
            }
        }
        assert!(opened.is_empty(), "opened at {opened:?}");
    }

    #[test]
    fn short_bursts_of_echo_are_learnt() {
        // Echo at -20 dB (0 dB as recorded), with bursts 14 dB louder
        // lasting 60 ms: too short to open the gate, and learnt (so longer
        // ones would not open it either).
        let mut g = EchoGate::new();
        let burst = |i: usize| i >= 150 && i % 20 < 3;
        let v = run(
            &mut g,
            300,
            5,
            |i| if burst(i) { -6.0 } else { -20.0 },
            |i| if burst(i) { 14.0 } else { 0.0 },
            |_| -100.0,
        );
        assert_eq!(g.openings, 0, "{v:?}");
        assert!(g.coupling_db().0 > -8.0, "learnt {:?}", g.coupling_db());
    }
}
