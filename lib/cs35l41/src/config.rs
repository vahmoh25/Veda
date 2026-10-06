//! The laptops whose firmware describes the amplifiers (`CSC3551`) without
//! their settings (no `_DSD`), by subsystem id: Linux's
//! `cs35l41_config_table` (`cs35l41_hda_property.c`), entry for entry.

use crate::Channel::{self, Left as L, Right as R};

/// What supplies the amplifiers' speaker voltage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boost {
    /// The amplifiers' own boost converters (their inductor, capacitor and
    /// peak current are in the entry).
    Internal,
    /// A boost circuit on the board, switched on by the amplifiers' GPIO1.
    External,
}

/// One laptop's amplifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The subsystem id (`_SUB` of the amplifiers' ACPI device, or the
    /// codec's): vendor and board, as 8 hexadecimal digits.
    pub ssid: &'static str,
    pub amps: usize,
    pub boost: Boost,
    /// The channel each amplifier plays, by chip select (SPI) or address
    /// order (I2C).
    pub channels: [Channel; 4],
    /// Which of the device's GPIO connections is the shared reset line,
    /// the speaker id and the second amplifier's chip select.
    pub reset_gpio: Option<u8>,
    pub speaker_id_gpio: Option<u8>,
    pub cs_gpio: Option<u8>,
    /// Internal boost: inductor (nH), peak current (mA), capacitor (µF).
    pub boost_inductor: u16,
    pub boost_peak: u16,
    pub boost_capacitor: u16,
}

const fn gpio(index: i8) -> Option<u8> {
    if index < 0 { None } else { Some(index as u8) }
}

#[allow(clippy::too_many_arguments)]
const fn entry(
    ssid: &'static str,
    amps: usize,
    boost: Boost,
    channels: [Channel; 4],
    reset: i8,
    speaker_id: i8,
    cs: i8,
    inductor: u16,
    peak: u16,
    capacitor: u16,
) -> Config {
    Config {
        ssid,
        amps,
        boost,
        channels,
        reset_gpio: gpio(reset),
        speaker_id_gpio: gpio(speaker_id),
        cs_gpio: gpio(cs),
        boost_inductor: inductor,
        boost_peak: peak,
        boost_capacitor: capacitor,
    }
}

use Boost::{External as EXT, Internal as INT};

const LR: [Channel; 4] = [L, R, L, R];
const RL: [Channel; 4] = [R, L, R, L];
const LLRR: [Channel; 4] = [L, L, R, R];

pub const CONFIGS: &[Config] = &[
    entry("10251826", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("1025182C", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("10251844", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("10280B27", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10280B28", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10280BEB", 2, EXT, LR, 1, -1, 0, 0, 0, 0),
    entry("10280C4D", 4, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("103C89C6", 2, INT, RL, -1, -1, -1, 1000, 4500, 24),
    entry("103C8A28", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A29", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A2A", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A2B", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A2C", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A2D", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A2E", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A30", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A31", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8A6E", 4, EXT, LLRR, 0, -1, -1, 0, 0, 0),
    entry("103C8B63", 4, EXT, RL, -1, -1, -1, 0, 0, 0),
    entry("103C8BB3", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BB4", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BDD", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BDE", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BDF", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE0", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE1", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE2", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE3", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE5", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE6", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE7", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE8", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8BE9", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8B3A", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8C15", 2, INT, LR, 0, 1, -1, 1000, 4000, 24),
    entry("103C8C16", 2, INT, LR, 0, 1, -1, 1000, 4000, 24),
    entry("103C8C17", 2, INT, LR, 0, 1, -1, 1000, 4000, 24),
    entry("103C8C4D", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8C4E", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8C4F", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8C50", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8C51", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8CDD", 2, INT, LR, 0, 1, -1, 1000, 4100, 24),
    entry("103C8CDE", 2, INT, LR, 0, 1, -1, 1000, 3900, 24),
    entry("104312AF", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10431433", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431463", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431473", 2, INT, LR, 1, -1, 0, 1000, 4500, 24),
    entry("10431483", 2, INT, LR, 1, -1, 0, 1000, 4500, 24),
    entry("10431493", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("104314D3", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("104314E3", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431503", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431533", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431573", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10431663", 2, INT, LR, 1, -1, 0, 1000, 4500, 24),
    entry("10431683", 2, EXT, LR, 0, 1, -1, 0, 0, 0),
    entry("104316A3", 2, EXT, LR, 1, 2, 0, 0, 0, 0),
    entry("104316D3", 2, EXT, LR, 1, 2, 0, 0, 0, 0),
    entry("104316F3", 2, EXT, LR, 1, 2, 0, 0, 0, 0),
    entry("104317F3", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431863", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("104318D3", 2, EXT, LR, 0, 1, -1, 0, 0, 0),
    entry("10431A83", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431B93", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10431C9F", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10431CAF", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10431CCF", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10431CDF", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10431CEF", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10431D1F", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431DA2", 2, EXT, LR, 1, 2, 0, 0, 0, 0),
    entry("10431E02", 2, EXT, LR, 1, 2, 0, 0, 0, 0),
    entry("10431E12", 2, EXT, LR, 0, 1, -1, 0, 0, 0),
    entry("10431EE2", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("10431F12", 2, INT, LR, 0, 1, -1, 1000, 4500, 24),
    entry("10431F1F", 2, EXT, LR, 1, -1, 0, 0, 0, 0),
    entry("10431F62", 2, EXT, LR, 1, 2, 0, 0, 0, 0),
    entry("10433A20", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10433A30", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10433A40", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10433A50", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("10433A60", 2, INT, LR, 1, 2, 0, 1000, 4500, 24),
    entry("17AA3865", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("17AA3866", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("17AA386E", 2, EXT, LR, 0, 2, -1, 0, 0, 0),
    entry("17AA386F", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("17AA3874", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("17AA3877", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("17AA3878", 2, EXT, LR, 0, -1, -1, 0, 0, 0),
    entry("17AA38A9", 2, EXT, LR, 0, 2, -1, 0, 0, 0),
    entry("17AA38AB", 2, EXT, LR, 0, 2, -1, 0, 0, 0),
    entry("17AA38B4", 2, EXT, LR, 0, 1, -1, 0, 0, 0),
    entry("17AA38B5", 2, EXT, LR, 0, 1, -1, 0, 0, 0),
    entry("17AA38B6", 2, EXT, LR, 0, 1, -1, 0, 0, 0),
    entry("17AA38B7", 2, EXT, LR, 0, 1, -1, 0, 0, 0),
    entry("17AA38C7", 4, INT, RL, 0, 2, -1, 1000, 4500, 24),
    entry("17AA38C8", 4, INT, RL, 0, 2, -1, 1000, 4500, 24),
    entry("17AA38F9", 2, EXT, RL, 0, 2, -1, 0, 0, 0),
    entry("17AA38FA", 2, EXT, RL, 0, 2, -1, 0, 0, 0),
    entry("17AA3929", 4, INT, RL, 0, 2, -1, 1000, 4500, 24),
    entry("17AA392B", 4, INT, RL, 0, 2, -1, 1000, 4500, 24),
];

/// The settings of the laptop with subsystem id `ssid` (in any case).
pub fn find(ssid: &str) -> Option<&'static Config> {
    CONFIGS.iter().find(|c| c.ssid.eq_ignore_ascii_case(ssid))
}
