//! The echo canceller used by capture streams that ask for it.
//!
//! Until `vaudio` grows its canceller this is a pass-through with the same
//! interface, so the capture path is complete and testable.

/// Removes the playback reference from microphone frames.
pub struct EchoCanceller;

impl EchoCanceller {
    /// Samples per frame given to [`EchoCanceller::process`] (10 ms at 16 kHz).
    pub const FRAME: usize = 160;

    pub fn new(_rate: u32, _tail_ms: u32) -> EchoCanceller {
        EchoCanceller
    }

    /// Cleans one frame of `capture` given what was playing (`reference`).
    pub fn process(&mut self, capture: &[i16], _reference: &[i16], out: &mut [i16]) {
        out.copy_from_slice(capture);
    }

    /// Echo return loss enhancement achieved, in dB.
    pub fn erle_db(&self) -> f32 {
        0.0
    }

    /// The delay between playback and its echo, once found.
    pub fn echo_delay_ms(&self) -> Option<f32> {
        None
    }
}
