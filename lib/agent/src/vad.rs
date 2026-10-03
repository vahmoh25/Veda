//! Telling speech from quiet in microphone audio, on the computer, so that
//! nothing is sent anywhere while nobody speaks.
//!
//! [`Detector`] compares each 20 ms frame with the room's noise floor,
//! which it learns as it goes: speech is sound well above the floor for a
//! tenth of a second, and lasts until half a second of quiet. A steady
//! noise that starts (a fan) soon becomes the floor.

/// RMS level of 16-bit samples in dBFS (-100 for silence).
pub fn level_db(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return -100.0;
    }
    let sum: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = vmath::f32::sqrt((sum / frame.len() as f64) as f32) / 32768.0;
    if rms < 1e-5 { -100.0 } else { 20.0 * vmath::f32::log10(rms) }
}

/// Speech is this much louder than the noise floor ...
const ABOVE_FLOOR_DB: f32 = 12.0;
/// ... and at least this loud.
const MIN_DB: f32 = -50.0;
/// Frames of sound that start speech (100 ms of 20 ms frames).
const START_FRAMES: u32 = 5;
/// Frames of quiet that end it (500 ms).
const END_FRAMES: u32 = 25;

#[derive(Debug, Clone)]
pub struct Detector {
    floor_db: f32,
    loud: u32,
    quiet: u32,
    speaking: bool,
}

impl Default for Detector {
    fn default() -> Detector {
        Detector::new()
    }
}

impl Detector {
    pub fn new() -> Detector {
        Detector { floor_db: -70.0, loud: 0, quiet: 0, speaking: false }
    }

    /// Takes one 20 ms frame; returns whether someone is speaking.
    pub fn frame(&mut self, frame: &[i16]) -> bool {
        let db = level_db(frame);
        // The floor follows quiet quickly and sound slowly, so speech
        // hardly raises it.
        let rate = if db < self.floor_db { 0.2 } else { 0.003 };
        self.floor_db += (db - self.floor_db) * rate;
        if db > self.floor_db + ABOVE_FLOOR_DB && db > MIN_DB {
            self.loud += 1;
            self.quiet = 0;
        } else {
            self.quiet += 1;
            // A syllable's short gaps do not reset the count.
            if self.quiet > 3 {
                self.loud = 0;
            }
        }
        if !self.speaking && self.loud >= START_FRAMES {
            self.speaking = true;
        } else if self.speaking && self.quiet >= END_FRAMES {
            self.speaking = false;
        }
        self.speaking
    }

    /// The learned noise floor (dBFS).
    pub fn floor_db(&self) -> f32 {
        self.floor_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const FRAME: usize = 320;

    fn tone(amp: f32) -> Vec<i16> {
        (0..FRAME).map(|i| (vmath::f32::sin(i as f32 * 0.09) * amp) as i16).collect()
    }

    fn noise(amp: i32) -> Vec<i16> {
        (0..FRAME).map(|i| (((i * 7919) % 61) as i32 - 30) * amp / 30).map(|x| x as i16).collect()
    }

    #[test]
    fn levels() {
        assert_eq!(level_db(&[]), -100.0);
        assert_eq!(level_db(&[0; 320]), -100.0);
        let full = level_db(&tone(32767.0));
        assert!((full + 3.0).abs() < 0.5, "a full-scale sine is about -3 dBFS: {full}");
    }

    #[test]
    fn speech_starts_and_ends() {
        let mut d = Detector::new();
        let quiet = noise(30);
        for _ in 0..100 {
            assert!(!d.frame(&quiet));
        }
        let loud = tone(6000.0);
        let speaking: Vec<bool> = (0..10).map(|_| d.frame(&loud)).collect();
        assert!(!speaking[0] && speaking[9], "speech after a tenth of a second: {speaking:?}");
        // A pause between words keeps it going; half a second ends it.
        assert!((0..10).all(|_| d.frame(&quiet)));
        let after: Vec<bool> = (0..20).map(|_| d.frame(&quiet)).collect();
        assert!(!after[19]);
    }

    #[test]
    fn a_click_is_not_speech() {
        let mut d = Detector::new();
        let quiet = noise(30);
        for _ in 0..50 {
            d.frame(&quiet);
        }
        let loud = tone(8000.0);
        for _ in 0..3 {
            assert!(!d.frame(&loud));
        }
        assert!(!d.frame(&quiet));
    }

    #[test]
    fn a_steady_noise_becomes_the_floor() {
        let mut d = Detector::new();
        let fan = noise(3000);
        let mut last = true;
        for _ in 0..3000 {
            last = d.frame(&fan);
        }
        assert!(!last, "a minute of fan noise is not speech (floor {} dB)", d.floor_db());
        // Speech over the fan is still heard.
        let voice = tone(20000.0);
        assert!((0..10).map(|_| d.frame(&voice)).last().unwrap());
    }
}
