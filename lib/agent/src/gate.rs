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
//! agent's voice is audible (and for the echo's length after), whether it
//! is sent or replaced with silence. It is sent only when it is clearly
//! louder than the agent's echo could be — the user talking over the
//! agent — which is how a person barges in anyway. The echo's loudness is
//! learnt as the agent talks: how much louder or quieter the microphone is
//! than the voice being played, at the loudest, over the last seconds.
//! Before anything is learnt the gate assumes the echo may be a little
//! louder than the voice itself (a microphone's gain can be high).
//!
//! Levels are in dBFS, computed by the caller.

use alloc::collections::VecDeque;

/// Packets of 20 ms.
const PACKET_MS: u32 = 20;
/// How long the agent's voice is taken to echo: playback and capture
/// latencies plus the room (a hypervisor adds hundreds of milliseconds).
const ECHO_SPAN: usize = (900 / PACKET_MS) as usize;
/// How long the echo's loudness is remembered.
const LEARN_SPAN: usize = (3000 / PACKET_MS) as usize;
/// Below this the agent's voice counts as silent.
const VOICE_FLOOR_DB: f32 = -60.0;
/// The microphone must be this much louder than the expected echo ...
const MARGIN_DB: f32 = 6.0;
/// ... for this many packets in a row (speech, not a click) ...
const OPEN_PACKETS: u32 = 5;
/// ... and louder than this at all.
const SPEECH_FLOOR_DB: f32 = -50.0;
/// Once open, the gate stays open this long after the speech was last
/// loud enough.
const HANGOVER_PACKETS: u32 = 600 / PACKET_MS;
/// The echo is assumed this much louder than the voice until learnt (and
/// never taken to be louder).
const UNKNOWN_COUPLING_DB: f32 = 6.0;
/// Audio kept from before the gate opens, so the user's first words are
/// not lost.
pub const PREROLL_PACKETS: usize = (300 / PACKET_MS) as usize;

/// What the gate did with one packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The agent is not speaking: send the microphone.
    Pass,
    /// Echo (or too little to tell): send silence.
    Hold,
    /// The user talks over the agent: send what was held back from just
    /// before (the pre-roll), then the microphone.
    Open,
    /// Still open (the user is talking): send the microphone.
    Through,
}

#[derive(Debug, Clone)]
pub struct EchoGate {
    /// The voice's level, packet by packet, over the echo span.
    voice: VecDeque<f32>,
    /// Microphone minus voice level while holding, over the learning span.
    coupling: VecDeque<f32>,
    loud: u32,
    hangover: u32,
    open: bool,
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
            coupling: VecDeque::with_capacity(LEARN_SPAN),
            loud: 0,
            hangover: 0,
            open: false,
            held: 0,
            openings: 0,
        }
    }

    /// How loud the echo is taken to be relative to the voice (dB).
    pub fn coupling_db(&self) -> f32 {
        self.learned_coupling()
    }

    /// The agent's voice may be echoing right now.
    pub fn echoing(&self) -> bool {
        self.voice.iter().any(|&v| v > VOICE_FLOOR_DB)
    }

    /// Takes the level of 20 ms of (echo-cancelled) microphone audio and of
    /// the agent's voice playing at the same time.
    pub fn packet(&mut self, mic_db: f32, voice_db: f32) -> Verdict {
        if self.voice.len() == ECHO_SPAN {
            self.voice.pop_front();
        }
        self.voice.push_back(voice_db);
        let loudest_voice = self.voice.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if loudest_voice <= VOICE_FLOOR_DB {
            // Quiet for the whole echo span: nothing of the agent can be in
            // the microphone.
            self.open = false;
            self.loud = 0;
            return Verdict::Pass;
        }
        let expected_echo = loudest_voice + self.learned_coupling();
        let speech = mic_db > SPEECH_FLOOR_DB && mic_db > expected_echo + MARGIN_DB;
        if self.open {
            if speech || mic_db > expected_echo + MARGIN_DB / 2.0 {
                self.hangover = HANGOVER_PACKETS;
            } else if self.hangover > 0 {
                self.hangover -= 1;
            }
            if self.hangover > 0 {
                return Verdict::Through;
            }
            self.open = false;
        }
        self.loud = if speech { self.loud + 1 } else { 0 };
        if self.loud >= OPEN_PACKETS {
            self.open = true;
            self.hangover = HANGOVER_PACKETS;
            self.openings += 1;
            return Verdict::Open;
        }
        // Holding: what the microphone hears now is (at most) echo, which
        // teaches the gate how loud the echo gets.
        if !speech {
            if self.coupling.len() == LEARN_SPAN {
                self.coupling.pop_front();
            }
            self.coupling.push_back(mic_db - loudest_voice);
        }
        self.held += 1;
        Verdict::Hold
    }

    /// The echo's loudness relative to the voice: the loudest seen lately,
    /// or the cautious guess until a second of it has been heard.
    fn learned_coupling(&self) -> f32 {
        if self.coupling.len() < LEARN_SPAN / 3 {
            return UNKNOWN_COUPLING_DB;
        }
        self.coupling.iter().copied().fold(f32::NEG_INFINITY, f32::max).clamp(-80.0, UNKNOWN_COUPLING_DB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// The voice at -20 dBFS (with dips between syllables), its echo
    /// `coupling` dB louder in the microphone `delay` packets later, and
    /// the user's speech level packet by packet.
    fn run(
        gate: &mut EchoGate,
        packets: usize,
        delay: usize,
        coupling: f32,
        user: impl Fn(usize) -> f32,
    ) -> Vec<Verdict> {
        let voice = |i: usize| {
            if i >= packets.saturating_sub(10) {
                -100.0
            } else if i % 9 < 3 {
                -35.0
            } else {
                -20.0
            }
        };
        (0..packets)
            .map(|i| {
                let echo = if i >= delay { voice(i - delay) + coupling } else { -100.0 };
                let power = |db: f32| 10f32.powf(db / 10.0);
                let mic = 10.0 * (power(echo) + power(user(i)) + 1e-10).log10();
                gate.packet(mic, voice(i))
            })
            .collect()
    }

    #[test]
    fn passes_when_the_agent_is_silent() {
        let mut g = EchoGate::new();
        for _ in 0..50 {
            assert_eq!(g.packet(-30.0, -100.0), Verdict::Pass);
        }
        assert!(!g.echoing());
    }

    #[test]
    fn holds_back_loud_echo_from_the_first_word() {
        // Strong echo (only 3 dB quieter than the voice), late (400 ms): the
        // cautious guess covers the first second, the learnt echo the rest.
        let mut g = EchoGate::new();
        let v = run(&mut g, 300, 20, -3.0, |_| -100.0);
        assert!(v[..290].iter().all(|&x| x == Verdict::Hold), "the echo leaked: {v:?}");
        assert_eq!(g.openings, 0);
        // After the voice and its echo are over, the microphone passes again.
        for _ in 0..ECHO_SPAN {
            g.packet(-70.0, -100.0);
        }
        assert_eq!(g.packet(-70.0, -100.0), Verdict::Pass);
    }

    #[test]
    fn the_user_can_talk_over_the_agent() {
        // A moderately cancelled echo (-18 dB); the user starts talking at
        // -12 dBFS after three seconds.
        let mut g = EchoGate::new();
        let v = run(&mut g, 400, 10, -18.0, |i| if i >= 150 { -12.0 } else { -100.0 });
        assert!(v[..150].iter().all(|&x| x == Verdict::Hold));
        let opened = v.iter().position(|&x| x == Verdict::Open).expect("the gate never opened");
        assert!((150..160).contains(&opened), "opened at packet {opened}");
        assert!(v[opened + 1..380].iter().all(|&x| x == Verdict::Through), "{:?}", &v[opened..]);
    }

    #[test]
    fn quiet_talk_does_not_open_it_over_loud_echo() {
        let mut g = EchoGate::new();
        let v = run(&mut g, 300, 10, -6.0, |i| if i >= 100 { -24.0 } else { -100.0 });
        assert_eq!(g.openings, 0, "{v:?}");
    }
}
