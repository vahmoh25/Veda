//! A codec's audio function group and its widgets (HDA 1.0a section 7.2),
//! read through the controller.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::verb::{self, get_parameter, id, param};

/// The controller's side of talking to one codec.
pub trait Bus {
    /// Sends `verb` ([`verb::verb`], [`verb::verb4`]) to node `nid`;
    /// returns the response (`None`: none came).
    fn command(&mut self, nid: u8, verb: u32) -> Option<u32>;
}

/// What a widget is (section 7.3.4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetType {
    /// A converter from the link to analog: a DAC.
    Output,
    /// A converter from analog to the link: an ADC.
    Input,
    /// Sums its inputs.
    Mixer,
    /// Passes one of its inputs.
    Selector,
    /// A jack, a speaker or a microphone, and its amplifiers.
    Pin,
    Power,
    VolumeKnob,
    Beep,
    Vendor,
    Reserved(u8),
}

impl WidgetType {
    fn from_caps(caps: u32) -> WidgetType {
        match (caps >> 20) & 0xF {
            0 => WidgetType::Output,
            1 => WidgetType::Input,
            2 => WidgetType::Mixer,
            3 => WidgetType::Selector,
            4 => WidgetType::Pin,
            5 => WidgetType::Power,
            6 => WidgetType::VolumeKnob,
            7 => WidgetType::Beep,
            0xF => WidgetType::Vendor,
            n => WidgetType::Reserved(n as u8),
        }
    }
}

/// Audio widget capability bits.
pub mod caps {
    pub const STEREO: u32 = 1 << 0;
    pub const IN_AMP: u32 = 1 << 1;
    pub const OUT_AMP: u32 = 1 << 2;
    /// The widget has its own amplifier parameters (otherwise the function
    /// group's apply).
    pub const AMP_OVERRIDE: u32 = 1 << 3;
    /// The widget has its own format parameters.
    pub const FORMAT_OVERRIDE: u32 = 1 << 4;
    pub const UNSOLICITED: u32 = 1 << 7;
    pub const CONNECTION_LIST: u32 = 1 << 8;
    pub const DIGITAL: u32 = 1 << 9;
    pub const POWER_CONTROL: u32 = 1 << 10;
}

/// Pin capability bits (section 7.3.4.9).
pub mod pin_caps {
    pub const TRIGGER_REQUIRED: u32 = 1 << 1;
    pub const PRESENCE_DETECT: u32 = 1 << 2;
    pub const HEADPHONE_DRIVE: u32 = 1 << 3;
    pub const OUTPUT: u32 = 1 << 4;
    pub const INPUT: u32 = 1 << 5;
    pub const HDMI: u32 = 1 << 7;
    pub const VREF_50: u32 = 1 << 9;
    pub const VREF_80: u32 = 1 << 12;
    pub const VREF_100: u32 = 1 << 13;
    pub const EAPD: u32 = 1 << 16;
    pub const DISPLAY_PORT: u32 = 1 << 24;
}

/// Amplifier capabilities (section 7.3.4.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AmpCaps {
    pub mute: bool,
    /// The size of a step in quarter dB, minus one.
    pub step_size: u8,
    /// The largest gain value (gains go from 0 to this).
    pub steps: u8,
    /// The gain value of 0 dB.
    pub offset: u8,
}

impl AmpCaps {
    pub fn parse(v: u32) -> AmpCaps {
        AmpCaps {
            mute: v & 1 << 31 != 0,
            step_size: ((v >> 16) & 0x7F) as u8,
            steps: ((v >> 8) & 0x7F) as u8,
            offset: (v & 0x7F) as u8,
        }
    }

    /// The gain of `value` in quarter dB.
    pub fn quarter_db(&self, value: u8) -> i32 {
        (value as i32 - self.offset as i32) * (self.step_size as i32 + 1)
    }

    /// The largest gain that does not exceed `db` (the smallest one if
    /// none is that low).
    pub fn gain_for_db(&self, db: i32) -> u8 {
        let value = self.offset as i32 + (db * 4).div_euclid(self.step_size as i32 + 1);
        value.clamp(0, self.steps as i32) as u8
    }

    /// The gain of 0 dB: no amplification and no attenuation.
    pub fn unity(&self) -> u8 {
        self.gain_for_db(0)
    }
}

/// Where a pin's jack or device is (bits 30-31 of its configuration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    Jack,
    /// Nothing is connected to the pin: it is not used.
    None,
    /// A device built in, such as a laptop's speakers or microphone.
    Internal,
    /// Both a jack and a built-in device.
    Both,
}

/// What a pin is meant for (bits 20-23 of its configuration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    LineOut,
    Speaker,
    Headphone,
    Cd,
    SpdifOut,
    DigitalOut,
    ModemLine,
    ModemHandset,
    LineIn,
    Aux,
    Mic,
    Telephony,
    SpdifIn,
    DigitalIn,
    Other,
}

/// A pin's configuration default (section 7.3.3.31), set by the firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PinConfig(pub u32);

impl PinConfig {
    pub fn connectivity(&self) -> Connectivity {
        match self.0 >> 30 {
            0 => Connectivity::Jack,
            1 => Connectivity::None,
            2 => Connectivity::Internal,
            _ => Connectivity::Both,
        }
    }

    /// Inside the computer (the gross location of bits 28-29).
    pub fn inside(&self) -> bool {
        (self.0 >> 28) & 3 == 1
    }

    /// On the front of the computer.
    pub fn front(&self) -> bool {
        (self.0 >> 24) & 0xF == 2
    }

    pub fn device(&self) -> Device {
        match (self.0 >> 20) & 0xF {
            0 => Device::LineOut,
            1 => Device::Speaker,
            2 => Device::Headphone,
            3 => Device::Cd,
            4 => Device::SpdifOut,
            5 => Device::DigitalOut,
            6 => Device::ModemLine,
            7 => Device::ModemHandset,
            8 => Device::LineIn,
            9 => Device::Aux,
            0xA => Device::Mic,
            0xB => Device::Telephony,
            0xC => Device::SpdifIn,
            0xD => Device::DigitalIn,
            _ => Device::Other,
        }
    }

    /// The connection type (bits 16-19); 6 is "other digital", such as a
    /// laptop's digital microphone.
    pub fn connection_type(&self) -> u8 {
        ((self.0 >> 16) & 0xF) as u8
    }

    /// The firmware says presence detection does not work for this pin.
    pub fn no_presence_detect(&self) -> bool {
        self.0 & 1 << 8 != 0
    }

    /// The default association: pins that belong together (bits 4-7).
    pub fn association(&self) -> u8 {
        ((self.0 >> 4) & 0xF) as u8
    }

    /// The pin's place in its association (bits 0-3): 0 is the front pair
    /// of a multichannel output.
    pub fn sequence(&self) -> u8 {
        (self.0 & 0xF) as u8
    }
}

/// One widget of the audio function group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widget {
    pub nid: u8,
    pub kind: WidgetType,
    /// Audio widget capabilities ([`caps`]).
    pub caps: u32,
    /// Pins: capabilities ([`pin_caps`]) and configuration.
    pub pin_caps: u32,
    pub config: PinConfig,
    pub amp_in: AmpCaps,
    pub amp_out: AmpCaps,
    /// Converters: the supported rates and sample sizes.
    pub pcm: u32,
    /// The widgets this one takes its input from, in connection index
    /// order.
    pub connections: Vec<u8>,
}

impl Widget {
    pub fn has(&self, cap: u32) -> bool {
        self.caps & cap != 0
    }

    pub fn pin_has(&self, cap: u32) -> bool {
        self.pin_caps & cap != 0
    }

    /// A digital widget: an S/PDIF converter or an HDMI or DisplayPort pin.
    pub fn digital(&self) -> bool {
        self.has(caps::DIGITAL) || self.pin_has(pin_caps::HDMI | pin_caps::DISPLAY_PORT)
    }

    /// The connection index of `nid` among this widget's inputs.
    pub fn connection_index(&self, nid: u8) -> Option<u8> {
        self.connections.iter().position(|&c| c == nid).map(|i| i as u8)
    }

    /// Whether the pin can tell when something is plugged in.
    pub fn jack_detect(&self) -> bool {
        self.kind == WidgetType::Pin
            && self.pin_has(pin_caps::PRESENCE_DETECT)
            && !self.config.no_presence_detect()
            && matches!(self.config.connectivity(), Connectivity::Jack | Connectivity::Both)
    }
}

/// Why a codec could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The codec stopped answering.
    NoResponse,
    /// The codec has no audio function group (a modem, for instance).
    NoAudioFunction,
}

impl core::fmt::Display for CodecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            CodecError::NoResponse => "the codec does not answer",
            CodecError::NoAudioFunction => "the codec has no audio function",
        })
    }
}

/// Most connections read for one widget.
const MAX_CONNECTIONS: usize = 64;

/// A codec: its identity and the widgets of its audio function group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codec {
    /// The codec's address on the link (0 to 14).
    pub address: u8,
    /// Vendor (bits 16-31) and device.
    pub vendor_id: u32,
    pub revision: u32,
    /// The board's subsystem ID, as the firmware set it.
    pub subsystem: u32,
    /// The audio function group's node.
    pub afg: u8,
    pub widgets: Vec<Widget>,
}

impl Codec {
    /// Reads the codec at `address` through `bus`.
    pub fn read(address: u8, bus: &mut impl Bus) -> Result<Codec, CodecError> {
        let vendor_id = parameter(bus, 0, param::VENDOR_ID)?;
        let revision = parameter(bus, 0, param::REVISION_ID).unwrap_or(0);
        let (first, count) = node_range(parameter(bus, 0, param::NODE_COUNT)?);
        let mut afg = None;
        for nid in first..first.saturating_add(count) {
            if parameter(bus, nid, param::FUNCTION_TYPE)? & 0xFF == verb::AUDIO_FUNCTION_GROUP {
                afg = Some(nid);
                break;
            }
        }
        let afg = afg.ok_or(CodecError::NoAudioFunction)?;
        // Widgets without amplifier or format parameters of their own use
        // the function group's.
        let afg_amp_in = AmpCaps::parse(parameter(bus, afg, param::AMP_IN_CAPS).unwrap_or(0));
        let afg_amp_out = AmpCaps::parse(parameter(bus, afg, param::AMP_OUT_CAPS).unwrap_or(0));
        let afg_pcm = parameter(bus, afg, param::PCM).unwrap_or(0);
        let subsystem = bus.command(afg, verb::verb(id::GET_SUBSYSTEM_ID, 0)).unwrap_or(0);
        let (first, count) = node_range(parameter(bus, afg, param::NODE_COUNT)?);
        let mut widgets = Vec::new();
        for nid in first..first.saturating_add(count).min(0x80) {
            let caps = parameter(bus, nid, param::WIDGET_CAPS)?;
            let kind = WidgetType::from_caps(caps);
            let mut amp =
                |present: u32, p: u8, default: AmpCaps| match (caps & present != 0, caps & caps::AMP_OVERRIDE != 0) {
                    (false, _) => AmpCaps::default(),
                    (true, true) => AmpCaps::parse(parameter(bus, nid, p).unwrap_or(0)),
                    (true, false) => default,
                };
            let amp_in = amp(caps::IN_AMP, param::AMP_IN_CAPS, afg_amp_in);
            let amp_out = amp(caps::OUT_AMP, param::AMP_OUT_CAPS, afg_amp_out);
            let pcm = match kind {
                WidgetType::Output | WidgetType::Input if caps & caps::FORMAT_OVERRIDE != 0 => {
                    parameter(bus, nid, param::PCM)?
                }
                WidgetType::Output | WidgetType::Input => afg_pcm,
                _ => 0,
            };
            let (pin_caps, config) = if kind == WidgetType::Pin {
                let pin_caps = parameter(bus, nid, param::PIN_CAPS)?;
                let config = bus.command(nid, verb::verb(id::GET_CONFIG_DEFAULT, 0)).ok_or(CodecError::NoResponse)?;
                (pin_caps, PinConfig(config))
            } else {
                (0, PinConfig::default())
            };
            let connections = if caps & caps::CONNECTION_LIST != 0 { read_connections(bus, nid)? } else { Vec::new() };
            widgets.push(Widget { nid, kind, caps, pin_caps, config, amp_in, amp_out, pcm, connections });
        }
        Ok(Codec { address, vendor_id, revision, subsystem, afg, widgets })
    }

    pub fn widget(&self, nid: u8) -> Option<&Widget> {
        self.widgets.iter().find(|w| w.nid == nid)
    }

    /// The codec's maker and model, for logs and the audio service.
    pub fn name(&self) -> String {
        let device = self.vendor_id & 0xFFFF;
        match self.vendor_id >> 16 {
            0x10EC => format!("Realtek ALC{:x}", device),
            0x1013 => format!("Cirrus Logic CS{:x}", device),
            0x11D4 => format!("Analog Devices AD{:x}", device),
            0x14F1 => format!("Conexant {:04x}", device),
            0x111D => format!("IDT {:04x}", device),
            0x8384 => format!("SigmaTel {:04x}", device),
            0x1106 => format!("VIA {:04x}", device),
            0x434D => format!("C-Media {:04x}", device),
            0x1AF4 => String::from("QEMU HDA codec"),
            0x8086 => format!("Intel {:04x}", device),
            0x1002 => format!("AMD {:04x}", device),
            0x10DE => format!("NVIDIA {:04x}", device),
            vendor => format!("{:04x}:{:04x}", vendor, device),
        }
    }
}

/// Reads a parameter of node `nid`.
fn parameter(bus: &mut impl Bus, nid: u8, p: u8) -> Result<u32, CodecError> {
    bus.command(nid, get_parameter(p)).ok_or(CodecError::NoResponse)
}

/// The first node and the number of nodes of a Subordinate Node Count.
fn node_range(v: u32) -> (u8, u8) {
    (((v >> 16) & 0x7F) as u8, (v & 0x7F) as u8)
}

/// Reads a widget's connection list (section 7.3.3.3): short entries
/// (four per response) or long ones (two), where an entry with its top bit
/// set ends a range that starts after the entry before it.
fn read_connections(bus: &mut impl Bus, nid: u8) -> Result<Vec<u8>, CodecError> {
    let length = parameter(bus, nid, param::CONNECTION_LIST_LENGTH)?;
    let long = length & 0x80 != 0;
    let len = (length & 0x7F) as usize;
    let (per, bits): (usize, u32) = if long { (2, 16) } else { (4, 8) };
    let mut list: Vec<u8> = Vec::new();
    let mut previous: Option<u32> = None;
    for start in (0..len).step_by(per) {
        let response =
            bus.command(nid, verb::verb(id::GET_CONNECTION_LIST, start as u8)).ok_or(CodecError::NoResponse)?;
        for k in 0..per.min(len - start) {
            let entry = (response >> (bits * k as u32)) & ((1 << bits) - 1);
            let range = entry & 1 << (bits - 1) != 0;
            let node = entry & !(1 << (bits - 1));
            match previous {
                Some(from) if range && node > from => {
                    list.extend((from + 1..=node).filter(|&n| n < 0x80).map(|n| n as u8));
                }
                _ if node < 0x80 => list.push(node as u8),
                _ => {}
            }
            previous = Some(node);
            if list.len() >= MAX_CONNECTIONS {
                list.truncate(MAX_CONNECTIONS);
                return Ok(list);
            }
        }
    }
    Ok(list)
}
