//! Codec commands (HDA 1.0a section 7.3): verbs, parameters and their
//! payloads, and the stream format word that the controller's stream
//! descriptors and the codec's converters share.

/// A command as the controller sends it (a CORB entry): the codec's
/// address, the node, and a verb with its payload ([`verb`], [`verb4`]).
pub const fn command(codec: u8, nid: u8, verb: u32) -> u32 {
    (codec as u32 & 0xF) << 28 | (nid as u32 & 0x7F) << 20 | (verb & 0xF_FFFF)
}

/// A verb with a 12-bit ID and an 8-bit payload.
pub const fn verb(id: u16, payload: u8) -> u32 {
    (id as u32 & 0xFFF) << 8 | payload as u32
}

/// A verb with a 4-bit ID and a 16-bit payload (converter format,
/// amplifier gain).
pub const fn verb4(id: u8, payload: u16) -> u32 {
    (id as u32 & 0xF) << 16 | payload as u32
}

/// Verb IDs with an 8-bit payload.
pub mod id {
    pub const GET_PARAMETER: u16 = 0xF00;
    pub const GET_CONNECTION_SELECT: u16 = 0xF01;
    pub const SET_CONNECTION_SELECT: u16 = 0x701;
    pub const GET_CONNECTION_LIST: u16 = 0xF02;
    pub const GET_POWER_STATE: u16 = 0xF05;
    pub const SET_POWER_STATE: u16 = 0x705;
    pub const GET_STREAM_CHANNEL: u16 = 0xF06;
    pub const SET_STREAM_CHANNEL: u16 = 0x706;
    pub const GET_PIN_CONTROL: u16 = 0xF07;
    pub const SET_PIN_CONTROL: u16 = 0x707;
    pub const SET_UNSOLICITED: u16 = 0x708;
    pub const GET_PIN_SENSE: u16 = 0xF09;
    pub const EXECUTE_PIN_SENSE: u16 = 0x709;
    pub const GET_EAPD: u16 = 0xF0C;
    pub const SET_EAPD: u16 = 0x70C;
    /// The function group's general-purpose pins: their levels, which are
    /// in use, which are outputs.
    pub const SET_GPIO_DATA: u16 = 0x715;
    pub const SET_GPIO_ENABLE: u16 = 0x716;
    pub const SET_GPIO_DIRECTION: u16 = 0x717;
    pub const GET_CONFIG_DEFAULT: u16 = 0xF1C;
    pub const GET_SUBSYSTEM_ID: u16 = 0xF20;
}

/// Verb IDs with a 16-bit payload.
pub mod id4 {
    pub const SET_FORMAT: u8 = 0x2;
    pub const SET_AMP: u8 = 0x3;
    /// The vendor's processing coefficients: a value, and which one.
    pub const SET_PROC_COEF: u8 = 0x4;
    pub const SET_COEF_INDEX: u8 = 0x5;
    pub const GET_FORMAT: u8 = 0xA;
    pub const GET_AMP: u8 = 0xB;
    pub const GET_PROC_COEF: u8 = 0xC;
}

/// Parameters, read with [`get_parameter`] (section 7.3.4).
pub mod param {
    pub const VENDOR_ID: u8 = 0x00;
    pub const REVISION_ID: u8 = 0x02;
    /// The first subordinate node (bits 16-23) and how many (bits 0-7).
    pub const NODE_COUNT: u8 = 0x04;
    /// 1 in bits 0-7: an audio function group.
    pub const FUNCTION_TYPE: u8 = 0x05;
    pub const WIDGET_CAPS: u8 = 0x09;
    /// Supported sample rates (bits 0-11) and sizes (bits 16-20).
    pub const PCM: u8 = 0x0A;
    pub const PIN_CAPS: u8 = 0x0C;
    pub const AMP_IN_CAPS: u8 = 0x0D;
    pub const CONNECTION_LIST_LENGTH: u8 = 0x0E;
    pub const AMP_OUT_CAPS: u8 = 0x12;
}

/// The function group type of an audio function group.
pub const AUDIO_FUNCTION_GROUP: u32 = 1;

/// Pin widget control bits (section 7.3.3.13).
pub mod pin_ctl {
    pub const HEADPHONE: u8 = 0x80;
    pub const OUT: u8 = 0x40;
    pub const IN: u8 = 0x20;
    /// Microphone bias voltages (bits 0-2).
    pub const VREF_HIZ: u8 = 0;
    pub const VREF_50: u8 = 1;
    pub const VREF_80: u8 = 4;
    pub const VREF_100: u8 = 5;
}

/// EAPD/BTL enable: EAPD powers the external amplifier of a pin.
pub const EAPD: u8 = 0x02;

pub fn get_parameter(p: u8) -> u32 {
    verb(id::GET_PARAMETER, p)
}

/// Which amplifier of a widget a gain/mute command addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Amp {
    Output,
    /// The input amplifier of connection `n` (mixers and selectors have
    /// one per input; most other widgets only input 0).
    Input(u8),
}

/// Set Amplifier Gain/Mute for both channels.
pub fn set_amp(amp: Amp, mute: bool, gain: u8) -> u32 {
    let (direction, index) = match amp {
        Amp::Output => (1 << 15, 0),
        Amp::Input(n) => (1 << 14, n & 0xF),
    };
    verb4(id4::SET_AMP, direction | 3 << 12 | (index as u16) << 8 | (mute as u16) << 7 | (gain as u16 & 0x7F))
}

/// Get Amplifier Gain/Mute of the left channel.
pub fn get_amp(amp: Amp) -> u32 {
    let payload = match amp {
        Amp::Output => 1 << 15 | 1 << 13,
        Amp::Input(n) => 1 << 13 | (n as u16 & 0xF),
    };
    verb4(id4::GET_AMP, payload)
}

/// Set Power State: D0 is fully on.
pub fn set_power_state(d: u8) -> u32 {
    verb(id::SET_POWER_STATE, d & 0xF)
}

/// Set Converter Stream, Channel: the stream tag a converter takes its
/// samples from (or gives them to), and its first channel.
pub fn set_stream_channel(stream: u8, channel: u8) -> u32 {
    verb(id::SET_STREAM_CHANNEL, (stream & 0xF) << 4 | (channel & 0xF))
}

/// Set Converter Format ([`StreamFormat::encode`]).
pub fn set_format(format: u16) -> u32 {
    verb4(id4::SET_FORMAT, format)
}

pub fn set_pin_control(bits: u8) -> u32 {
    verb(id::SET_PIN_CONTROL, bits)
}

pub fn set_connection_select(index: u8) -> u32 {
    verb(id::SET_CONNECTION_SELECT, index)
}

pub fn set_eapd(bits: u8) -> u32 {
    verb(id::SET_EAPD, bits)
}

/// Enables unsolicited responses with `tag` (bits 26-31 of the response),
/// or disables them.
pub fn set_unsolicited(tag: Option<u8>) -> u32 {
    verb(id::SET_UNSOLICITED, tag.map_or(0, |t| 0x80 | (t & 0x3F)))
}

/// Get Pin Sense: bit 31 of the response is set while something is plugged
/// in.
pub fn get_pin_sense() -> u32 {
    verb(id::GET_PIN_SENSE, 0)
}

/// Presence detected, from a Get Pin Sense response.
pub fn presence(sense: u32) -> bool {
    sense & 1 << 31 != 0
}

/// The tag of an unsolicited response.
pub fn unsolicited_tag(response: u32) -> u8 {
    (response >> 26) as u8
}

/// The format of a stream: the word in the controller's stream descriptor
/// and in the codec's converters (section 3.7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub rate: u32,
    /// Bits per sample: 8, 16, 20, 24 or 32.
    pub bits: u8,
    pub channels: u8,
}

/// Sample rates and the supported-rate bit of each (section 7.3.4.7).
const RATES: [(u32, u32); 12] = [
    (8_000, 1 << 0),
    (11_025, 1 << 1),
    (16_000, 1 << 2),
    (22_050, 1 << 3),
    (32_000, 1 << 4),
    (44_100, 1 << 5),
    (48_000, 1 << 6),
    (88_200, 1 << 7),
    (96_000, 1 << 8),
    (176_400, 1 << 9),
    (192_000, 1 << 10),
    (384_000, 1 << 11),
];

impl StreamFormat {
    pub const fn new(rate: u32, bits: u8, channels: u8) -> StreamFormat {
        StreamFormat { rate, bits, channels }
    }

    /// The 16-bit encoding (`None`: a rate, size or channel count HDA cannot
    /// express).
    pub fn encode(&self) -> Option<u16> {
        // Base rate (44.1 kHz or 48 kHz), multiplier and divisor.
        let (base_44k1, mult, div): (u16, u16, u16) = match self.rate {
            8_000 => (0, 1, 6),
            11_025 => (1, 1, 4),
            16_000 => (0, 1, 3),
            22_050 => (1, 1, 2),
            32_000 => (0, 2, 3),
            44_100 => (1, 1, 1),
            48_000 => (0, 1, 1),
            88_200 => (1, 2, 1),
            96_000 => (0, 2, 1),
            176_400 => (1, 4, 1),
            192_000 => (0, 4, 1),
            _ => return None,
        };
        let bits = match self.bits {
            8 => 0,
            16 => 1,
            20 => 2,
            24 => 3,
            32 => 4,
            _ => return None,
        };
        if !(1..=16).contains(&self.channels) {
            return None;
        }
        Some(base_44k1 << 14 | (mult - 1) << 11 | (div - 1) << 8 | bits << 4 | (self.channels as u16 - 1))
    }

    /// Bytes per frame in memory (20- and 24-bit samples take 32 bits).
    pub fn frame_bytes(&self) -> usize {
        let sample = match self.bits {
            8 => 1,
            16 => 2,
            _ => 4,
        };
        sample * self.channels as usize
    }

    /// Whether a converter with these supported rates and sizes (the PCM
    /// parameter) can run at this format.
    pub fn supported_by(&self, pcm: u32) -> bool {
        let rate = RATES.iter().find(|r| r.0 == self.rate).map_or(0, |r| r.1);
        let size = match self.bits {
            8 => 1 << 16,
            16 => 1 << 17,
            20 => 1 << 18,
            24 => 1 << 19,
            32 => 1 << 20,
            _ => 0,
        };
        rate != 0 && size != 0 && pcm & rate != 0 && pcm & size != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        // Get Parameter (vendor ID) of node 0 of codec 2.
        assert_eq!(command(2, 0, get_parameter(param::VENDOR_ID)), 0x200F_0000);
        // Set Pin Widget Control (out + headphone) on node 0x15 of codec 0.
        assert_eq!(command(0, 0x15, set_pin_control(pin_ctl::OUT | pin_ctl::HEADPHONE)), 0x0157_07C0);
        assert_eq!(command(0, 0x02, set_format(0x0011)), 0x0022_0011);
        assert_eq!(set_stream_channel(1, 0), 0x70610);
        assert_eq!(set_unsolicited(Some(5)), 0x70885);
        assert_eq!(set_unsolicited(None), 0x70800);
        assert_eq!(unsolicited_tag(5 << 26 | 0x123), 5);
        assert!(presence(0x8000_0000) && !presence(0x7FFF_FFFF));
    }

    #[test]
    fn amplifiers() {
        // Output amplifier, both channels, unmuted, gain 0x57.
        assert_eq!(set_amp(Amp::Output, false, 0x57), 0x3B057);
        // Input amplifier 2, muted.
        assert_eq!(set_amp(Amp::Input(2), true, 0), 0x37280);
        assert_eq!(get_amp(Amp::Output), 0xBA000);
        assert_eq!(get_amp(Amp::Input(1)), 0xB2001);
    }

    #[test]
    fn stream_formats() {
        assert_eq!(StreamFormat::new(48_000, 16, 2).encode(), Some(0x0011));
        assert_eq!(StreamFormat::new(44_100, 16, 2).encode(), Some(0x4011));
        assert_eq!(StreamFormat::new(96_000, 24, 2).encode(), Some(0x0831));
        assert_eq!(StreamFormat::new(192_000, 32, 8).encode(), Some(0x1847));
        assert_eq!(StreamFormat::new(16_000, 16, 1).encode(), Some(0x0210));
        assert_eq!(StreamFormat::new(12_000, 16, 2).encode(), None);
        assert_eq!(StreamFormat::new(48_000, 12, 2).encode(), None);
        assert_eq!(StreamFormat::new(48_000, 16, 0).encode(), None);
        assert_eq!(StreamFormat::new(48_000, 16, 2).frame_bytes(), 4);
        assert_eq!(StreamFormat::new(48_000, 24, 2).frame_bytes(), 8);
    }

    #[test]
    fn supported_formats() {
        // 44.1, 48 and 96 kHz; 16 and 24 bits.
        let pcm = 1 << 5 | 1 << 6 | 1 << 8 | 1 << 17 | 1 << 19;
        assert!(StreamFormat::new(48_000, 16, 2).supported_by(pcm));
        assert!(StreamFormat::new(96_000, 24, 2).supported_by(pcm));
        assert!(!StreamFormat::new(192_000, 16, 2).supported_by(pcm));
        assert!(!StreamFormat::new(48_000, 32, 2).supported_by(pcm));
    }
}
