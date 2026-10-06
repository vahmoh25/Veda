//! Speaker amplifiers that a sound card feeds but does not control: the
//! "smart" amplifiers of many laptops, which take the codec's audio over
//! I2S and are set up over SPI or I2C. Their driver registers this service;
//! the sound card's driver tells it when the stream to the speakers starts
//! and stops, since the amplifiers may only power up while the codec
//! clocks them.

use vipc::{enumeration, protocol};

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SpeakerError {
        /// No amplifier could be set up.
        NoAmplifiers = 1,
        /// An amplifier did not do what it was told (see the driver's log).
        Failed = 2,
    }
}

protocol! {
    /// The speaker amplifiers.
    pub mod speakers = "speakers" {
        /// The codec's stream to the speakers has started (`true`): the
        /// amplifiers power up after the reply. Or it is about to stop
        /// (`false`): they power down before the reply, while the codec
        /// still clocks them.
        1 => fn playing(on: bool) -> Result<(), SpeakerError>;
        /// Mutes the speakers (headphones plugged in), or unmutes them.
        2 => fn mute(muted: bool) -> Result<(), SpeakerError>;
    }
}
