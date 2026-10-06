//! Codecs modelled on real ones, answering verbs as the hardware does,
//! and what the routing makes of them.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::codec::{AmpCaps, Bus, Codec, CodecError, Connectivity, Device, WidgetType};
use crate::route::{InputKind, OutputKind, Routing};
use crate::vendor::{self, Prepared};
use crate::verb::{self, Amp, StreamFormat, id4, pin_ctl};

const FORMAT: StreamFormat = StreamFormat::new(48_000, 16, 2);

// Widget types and capability bits.
const DAC: u32 = 0;
const ADC: u32 = 1 << 20;
const MIX: u32 = 2 << 20;
const SEL: u32 = 3 << 20;
const PIN: u32 = 4 << 20;
const VENDOR: u32 = 0xF << 20;
const STEREO: u32 = 1;
const IN_AMP: u32 = 1 << 1;
const OUT_AMP: u32 = 1 << 2;
const AMP_OVERRIDE: u32 = 1 << 3;
const FORMAT_OVERRIDE: u32 = 1 << 4;
const UNSOLICITED: u32 = 1 << 7;
const CONNECTIONS: u32 = 1 << 8;
const DIGITAL: u32 = 1 << 9;
const POWER: u32 = 1 << 10;

// Pin capabilities.
const PRESENCE: u32 = 1 << 2;
const HP_DRIVE: u32 = 1 << 3;
const OUT: u32 = 1 << 4;
const IN: u32 = 1 << 5;
const HDMI: u32 = 1 << 7;
/// Hi-Z, 50%, ground, 80% and 100%.
const VREFS: u32 = 0x3700;
const EAPD: u32 = 1 << 16;

/// Realtek's converters: 44.1, 48, 96 and 192 kHz; 16, 20 and 24 bits.
const REALTEK_PCM: u32 = 0x000E_0560;
/// Mute only.
const MUTE_ONLY: u32 = 0x8000_0000;
/// A microphone boost: 0 to +30 dB in 10 dB steps.
const BOOST: u32 = 0x0027_0300;

#[derive(Clone)]
struct Node {
    nid: u8,
    caps: u32,
    pin_caps: u32,
    config: u32,
    amp_in: u32,
    amp_out: u32,
    pcm: u32,
    connections: Vec<u8>,
}

fn node(nid: u8, caps: u32) -> Node {
    Node { nid, caps, pin_caps: 0, config: 0, amp_in: 0, amp_out: 0, pcm: 0, connections: Vec::new() }
}

impl Node {
    fn from(mut self, connections: &[u8]) -> Node {
        self.connections = connections.to_vec();
        self.caps |= CONNECTIONS;
        self
    }

    fn pin(mut self, pin_caps: u32, config: u32) -> Node {
        self.pin_caps = pin_caps;
        self.config = config;
        self
    }

    fn amps(mut self, amp_in: u32, amp_out: u32) -> Node {
        self.amp_in = amp_in;
        self.amp_out = amp_out;
        self
    }

    fn pcm(mut self, pcm: u32) -> Node {
        self.pcm = pcm;
        self
    }
}

/// A codec at address 0: root node 0, the audio function group at node 1,
/// and widgets from the lowest given node to the highest (the gaps are
/// vendor widgets).
struct FakeCodec {
    vendor: u32,
    afg_pcm: u32,
    afg_amp_in: u32,
    afg_amp_out: u32,
    function_type: u32,
    nodes: Vec<Node>,
    long_lists: bool,
    subsystem: u32,
    /// Realtek's processing coefficients: (node, index) to value, and the
    /// index each node is at.
    coefs: BTreeMap<(u8, u16), u16>,
    coef_index: BTreeMap<u8, u16>,
    /// Every command that is not a query.
    sent: Vec<(u8, u32)>,
}

impl FakeCodec {
    fn new(vendor: u32, nodes: Vec<Node>) -> FakeCodec {
        FakeCodec {
            vendor,
            afg_pcm: REALTEK_PCM,
            afg_amp_in: MUTE_ONLY,
            afg_amp_out: 0,
            function_type: 1,
            nodes,
            long_lists: false,
            subsystem: 0x1043_1C13,
            coefs: BTreeMap::new(),
            coef_index: BTreeMap::new(),
            sent: Vec::new(),
        }
    }

    fn node(&self, nid: u8) -> Option<&Node> {
        self.nodes.iter().find(|n| n.nid == nid)
    }

    fn widget_range(&self) -> (u32, u32) {
        let first = self.nodes.iter().map(|n| n.nid).min().unwrap_or(2) as u32;
        let last = self.nodes.iter().map(|n| n.nid).max().unwrap_or(2) as u32;
        (first, last - first + 1)
    }

    fn parameter(&self, nid: u8, p: u8) -> Option<u32> {
        let (first, count) = self.widget_range();
        match (nid, p) {
            (0, verb::param::VENDOR_ID) => Some(self.vendor),
            (0, verb::param::REVISION_ID) => Some(0x0010_0100),
            (0, verb::param::NODE_COUNT) => Some(1 << 16 | 1),
            (0, _) => Some(0),
            (1, verb::param::FUNCTION_TYPE) => Some(self.function_type),
            (1, verb::param::NODE_COUNT) => Some(first << 16 | count),
            (1, verb::param::PCM) => Some(self.afg_pcm),
            (1, verb::param::AMP_IN_CAPS) => Some(self.afg_amp_in),
            (1, verb::param::AMP_OUT_CAPS) => Some(self.afg_amp_out),
            (1, _) => Some(0),
            _ if (nid as u32) < first || (nid as u32) >= first + count => None,
            _ => {
                let Some(n) = self.node(nid) else {
                    return Some(if p == verb::param::WIDGET_CAPS { VENDOR } else { 0 });
                };
                Some(match p {
                    verb::param::WIDGET_CAPS => n.caps,
                    verb::param::PCM => n.pcm,
                    verb::param::PIN_CAPS => n.pin_caps,
                    verb::param::AMP_IN_CAPS => n.amp_in,
                    verb::param::AMP_OUT_CAPS => n.amp_out,
                    verb::param::CONNECTION_LIST_LENGTH => {
                        n.connections.len() as u32 | if self.long_lists { 0x80 } else { 0 }
                    }
                    _ => 0,
                })
            }
        }
    }

    fn connection_entries(&self, nid: u8, offset: usize) -> Option<u32> {
        let n = self.node(nid)?;
        let (per, bits) = if self.long_lists { (2, 16) } else { (4, 8) };
        let mut v = 0u32;
        for k in 0..per {
            if let Some(&c) = n.connections.get(offset + k) {
                v |= (c as u32) << (bits * k);
            }
        }
        Some(v)
    }
}

impl Bus for FakeCodec {
    fn command(&mut self, nid: u8, v: u32) -> Option<u32> {
        let index = self.coef_index.get(&nid).copied().unwrap_or(0);
        match (v >> 16) as u8 {
            id4::GET_PROC_COEF => return Some(self.coefs.get(&(nid, index)).copied().unwrap_or(0) as u32),
            id4::SET_COEF_INDEX => {
                self.coef_index.insert(nid, v as u16);
                return Some(0);
            }
            id4::SET_PROC_COEF => {
                self.coefs.insert((nid, index), v as u16);
            }
            _ => {}
        }
        let (id, payload) = ((v >> 8) as u16, (v & 0xFF) as u8);
        match id {
            verb::id::GET_PARAMETER => self.parameter(nid, payload),
            verb::id::GET_CONNECTION_LIST => self.connection_entries(nid, payload as usize),
            verb::id::GET_CONFIG_DEFAULT => self.node(nid).map(|n| n.config),
            verb::id::GET_SUBSYSTEM_ID => Some(self.subsystem),
            _ => {
                self.sent.push((nid, v));
                Some(0)
            }
        }
    }
}

/// A Realtek ALC269 as laptops have it: speakers, a headphone jack, a
/// digital microphone and a microphone jack.
fn alc269() -> FakeCodec {
    let dac = DAC | STEREO | OUT_AMP | AMP_OVERRIDE | FORMAT_OVERRIDE | POWER;
    let adc = ADC | STEREO | IN_AMP | AMP_OVERRIDE | FORMAT_OVERRIDE | POWER;
    let out_pin = PIN | STEREO | OUT_AMP | AMP_OVERRIDE | UNSOLICITED | POWER;
    let io_pin = out_pin | IN_AMP;
    let nc = 0x4111_11F0;
    FakeCodec::new(
        0x10EC_0269,
        vec![
            node(0x02, dac).amps(0, 0x0002_5757).pcm(REALTEK_PCM),
            node(0x03, dac).amps(0, 0x0002_5757).pcm(REALTEK_PCM),
            node(0x06, DAC | STEREO | FORMAT_OVERRIDE | DIGITAL).pcm(REALTEK_PCM),
            node(0x08, adc).amps(0x8002_3F17, 0).pcm(REALTEK_PCM).from(&[0x23]),
            node(0x09, adc).amps(0x8002_3F17, 0).pcm(REALTEK_PCM).from(&[0x22]),
            node(0x0B, MIX | STEREO | IN_AMP | AMP_OVERRIDE).amps(0x8005_1F17, 0).from(&[0x18, 0x19, 0x1A, 0x1B, 0x1D]),
            node(0x0C, MIX | STEREO | IN_AMP).from(&[0x02, 0x0B]),
            node(0x0D, MIX | STEREO | IN_AMP).from(&[0x03, 0x0B]),
            node(0x0F, MIX | STEREO | IN_AMP).from(&[0x02, 0x0B]),
            node(0x12, PIN | STEREO | IN_AMP | AMP_OVERRIDE | POWER).pin(IN, 0x90A6_0130).amps(BOOST, 0),
            node(0x14, out_pin).pin(PRESENCE | OUT | EAPD, 0x9017_0110).amps(0, MUTE_ONLY).from(&[0x0C, 0x0D]),
            node(0x15, out_pin)
                .pin(PRESENCE | HP_DRIVE | OUT | EAPD, 0x0221_101F)
                .amps(0, MUTE_ONLY)
                .from(&[0x0C, 0x0D]),
            node(0x17, out_pin).pin(OUT, nc).amps(0, MUTE_ONLY).from(&[0x0F]),
            node(0x18, io_pin).pin(VREFS | IN | OUT | PRESENCE, 0x02A1_1030).amps(BOOST, MUTE_ONLY).from(&[0x0C, 0x0D]),
            node(0x19, io_pin).pin(VREFS | IN | OUT | PRESENCE, nc).amps(BOOST, MUTE_ONLY).from(&[0x0C, 0x0D]),
            node(0x1A, io_pin).pin(VREFS | IN | OUT | PRESENCE, nc).amps(BOOST, MUTE_ONLY).from(&[0x0C, 0x0D]),
            node(0x1B, io_pin).pin(VREFS | IN | OUT | PRESENCE, nc).amps(BOOST, MUTE_ONLY).from(&[0x0C, 0x0D]),
            node(0x1D, PIN | STEREO).pin(IN, 0x40E7_E629),
            node(0x1E, PIN | STEREO | DIGITAL).pin(OUT, nc).from(&[0x06]),
            node(0x22, MIX | STEREO | IN_AMP).from(&[0x18, 0x19, 0x1A, 0x1B, 0x1D, 0x0B]),
            node(0x23, MIX | STEREO | IN_AMP).from(&[0x18, 0x19, 0x1A, 0x1B, 0x1D, 0x0B, 0x12]),
        ],
    )
}

/// A Realtek ALC887 as desktops have it: a 7.1 line output on the back
/// (green front pair), headphones and a microphone on the front, a
/// microphone and a line input on the back, S/PDIF.
fn alc887() -> FakeCodec {
    let dac = DAC | STEREO | OUT_AMP | AMP_OVERRIDE | FORMAT_OVERRIDE | POWER;
    let adc = ADC | STEREO | IN_AMP | AMP_OVERRIDE | FORMAT_OVERRIDE | POWER;
    let pin = PIN | STEREO | IN_AMP | OUT_AMP | AMP_OVERRIDE | UNSOLICITED | POWER;
    let retaskable = PRESENCE | HP_DRIVE | OUT | IN | VREFS;
    let mixers = [0x0C, 0x0D, 0x0E, 0x0F];
    let capture = [0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x14, 0x15, 0x16, 0x17, 0x0B];
    let mut nodes = Vec::new();
    for nid in 0x02..=0x05 {
        nodes.push(node(nid, dac).amps(0, 0x0003_4040).pcm(REALTEK_PCM));
    }
    nodes.push(node(0x06, DAC | STEREO | FORMAT_OVERRIDE | DIGITAL).pcm(REALTEK_PCM));
    nodes.push(node(0x08, adc).amps(0x8003_2E10, 0).pcm(REALTEK_PCM).from(&[0x23]));
    nodes.push(node(0x09, adc).amps(0x8003_2E10, 0).pcm(REALTEK_PCM).from(&[0x22]));
    nodes.push(
        node(0x0B, MIX | STEREO | IN_AMP | AMP_OVERRIDE)
            .amps(0x8005_1F17, 0)
            .from(&[0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x14, 0x15, 0x16, 0x17]),
    );
    for (k, nid) in mixers.into_iter().enumerate() {
        nodes.push(node(nid, MIX | STEREO | IN_AMP).from(&[0x02 + k as u8, 0x0B]));
    }
    for (nid, config) in [
        (0x14, 0x0101_4010), // back, green: front left/right
        (0x15, 0x0101_1012), // back, black: surround
        (0x16, 0x0101_6011), // back, orange: centre/LFE
        (0x17, 0x0101_2014), // back, grey: side
        (0x18, 0x01A1_9030), // back, pink: microphone
        (0x19, 0x02A1_9040), // front, pink: microphone
        (0x1A, 0x0181_303F), // back, blue: line in
        (0x1B, 0x0221_401F), // front, green: headphones
    ] {
        nodes.push(node(nid, pin).pin(retaskable, config).amps(BOOST, MUTE_ONLY).from(&mixers));
    }
    nodes.push(node(0x1C, PIN | STEREO).pin(IN, 0x4111_11F0));
    nodes.push(node(0x1D, PIN | STEREO).pin(IN, 0x4024_C601));
    nodes.push(node(0x1E, PIN | STEREO | DIGITAL).pin(OUT, 0x0144_1120).from(&[0x06]));
    nodes.push(node(0x22, MIX | STEREO | IN_AMP).from(&capture));
    nodes.push(node(0x23, MIX | STEREO | IN_AMP).from(&capture));
    FakeCodec::new(0x10EC_0887, nodes)
}

/// QEMU's hda-duplex: one DAC to a line output, one line input to an ADC.
fn qemu_duplex() -> FakeCodec {
    let amp = 0x0003_4A4A;
    let mut codec = FakeCodec::new(
        0x1AF4_0021,
        vec![
            node(0x02, DAC | STEREO | OUT_AMP | AMP_OVERRIDE | FORMAT_OVERRIDE).amps(0, amp).pcm(0x0002_01FC),
            node(0x03, PIN | STEREO).pin(OUT, 0x0000_4010).from(&[0x02]),
            node(0x04, ADC | STEREO | IN_AMP | AMP_OVERRIDE | FORMAT_OVERRIDE)
                .amps(amp, 0)
                .pcm(0x0002_01FC)
                .from(&[0x05]),
            node(0x05, PIN | STEREO).pin(IN, 0x0081_4020),
        ],
    );
    codec.afg_pcm = 0;
    codec.afg_amp_in = 0;
    codec
}

fn read(fake: &mut FakeCodec) -> Codec {
    Codec::read(0, fake).unwrap()
}

#[test]
fn reads_a_codec() {
    let mut fake = alc269();
    let codec = read(&mut fake);
    assert_eq!((codec.vendor_id, codec.afg, codec.subsystem), (0x10EC_0269, 1, 0x1043_1C13));
    assert_eq!(codec.name(), "Realtek ALC269");
    // Every node from 0x02 to 0x23; the gaps are vendor widgets.
    assert_eq!(codec.widgets.len(), 0x23 - 0x02 + 1);
    assert_eq!(codec.widget(0x04).unwrap().kind, WidgetType::Vendor);
    let dac = codec.widget(0x02).unwrap();
    assert_eq!(dac.kind, WidgetType::Output);
    assert_eq!(dac.amp_out, AmpCaps { mute: false, step_size: 2, steps: 0x57, offset: 0x57 });
    assert_eq!(dac.pcm, REALTEK_PCM);
    // Without amplifier parameters of its own, a mixer has the function
    // group's.
    assert_eq!(codec.widget(0x0C).unwrap().amp_in, AmpCaps::parse(MUTE_ONLY));
    let hp = codec.widget(0x15).unwrap();
    assert_eq!(hp.config.device(), Device::Headphone);
    assert_eq!(hp.config.connectivity(), Connectivity::Jack);
    assert!(hp.config.front() && hp.jack_detect());
    assert_eq!(hp.connections, [0x0C, 0x0D]);
    let dmic = codec.widget(0x12).unwrap();
    assert_eq!((dmic.config.connectivity(), dmic.config.connection_type()), (Connectivity::Internal, 6));
    assert!(!dmic.jack_detect());
    assert_eq!(codec.widget(0x23).unwrap().connections, [0x18, 0x19, 0x1A, 0x1B, 0x1D, 0x0B, 0x12]);
    assert!(codec.widget(0x1E).unwrap().digital());
}

#[test]
fn laptop_routing() {
    let mut fake = alc269();
    let codec = read(&mut fake);
    let r = Routing::new(&codec, FORMAT);
    // The speakers and the headphones, each with a DAC of its own.
    assert_eq!(r.outputs.len(), 2);
    assert_eq!(
        (r.outputs[0].kind, r.outputs[0].pin, &r.outputs[0].path[..]),
        (OutputKind::Speaker, 0x14, &[0x14, 0x0C, 0x02][..])
    );
    assert_eq!(
        (r.outputs[1].kind, r.outputs[1].pin, &r.outputs[1].path[..]),
        (OutputKind::Headphone, 0x15, &[0x15, 0x0D, 0x03][..])
    );
    assert_eq!(r.dacs, [0x02, 0x03]);
    assert!(!r.outputs[0].jack && r.outputs[1].jack);
    // The ADC that reaches both microphones.
    assert_eq!(r.adc, Some(0x08));
    assert_eq!(r.inputs.len(), 2);
    let dmic = &r.inputs[0];
    assert_eq!(
        (dmic.kind, dmic.pin, &dmic.path[..], dmic.digital),
        (InputKind::InternalMic, 0x12, &[0x08, 0x23, 0x12][..], true)
    );
    let jack = &r.inputs[1];
    assert_eq!((jack.kind, jack.pin, &jack.path[..], jack.jack), (InputKind::Mic, 0x18, &[0x08, 0x23, 0x18][..], true));
    assert_eq!(r.jack_pins(), [0x15, 0x18]);
    // The built-in microphone, until one is plugged in.
    assert_eq!(r.active_input(|_| false), Some(0));
    assert_eq!(r.active_input(|pin| pin == 0x18), Some(1));
    assert!(!r.headphones_in(|_| false));
    assert!(r.headphones_in(|pin| pin == 0x15));
}

#[test]
fn laptop_setup() {
    let mut fake = alc269();
    let codec = read(&mut fake);
    let r = Routing::new(&codec, FORMAT);
    let format = FORMAT.encode().unwrap();
    let c = r.setup(&codec, Some((1, format)), Some((2, format)));
    let has = |nid: u8, v: u32| c.contains(&(nid, v));
    let position = |nid: u8, v: u32| c.iter().position(|&x| x == (nid, v)).unwrap();
    assert_eq!(c[0], (0x01, verb::set_power_state(0)));
    assert!(has(0x02, verb::set_power_state(0)));
    // The analog loopback stays muted, on every input and where it enters
    // the output mixers.
    for i in 0..5 {
        assert!(has(0x0B, verb::set_amp(Amp::Input(i), true, 0)));
    }
    assert!(has(0x0C, verb::set_amp(Amp::Input(1), true, 0)));
    // The speaker path: the mixer's DAC input opened after everything was
    // muted, the DAC at 0 dB, the pin on with its amplifier powered.
    assert!(
        position(0x0C, verb::set_amp(Amp::Input(0), true, 0)) < position(0x0C, verb::set_amp(Amp::Input(0), false, 0))
    );
    assert!(has(0x14, verb::set_connection_select(0)));
    assert!(has(0x14, verb::set_amp(Amp::Output, false, 0)));
    assert!(has(0x14, verb::set_eapd(verb::EAPD)));
    assert!(has(0x14, verb::set_pin_control(pin_ctl::OUT)));
    assert!(has(0x02, verb::set_amp(Amp::Output, false, 0x57)));
    // The headphones take the second DAC, with the headphone amplifier.
    assert!(has(0x15, verb::set_connection_select(1)));
    assert!(has(0x15, verb::set_pin_control(pin_ctl::OUT | pin_ctl::HEADPHONE)));
    assert!(has(0x03, verb::set_amp(Amp::Output, false, 0x57)));
    // Both DACs on the output stream, the ADC on the input stream: each
    // converter its stream first, then the format (as Linux does it).
    for (converter, tag) in [(0x02, 1), (0x03, 1), (0x08, 2)] {
        assert!(position(converter, verb::set_stream_channel(tag, 0)) < position(converter, verb::set_format(0x0011)));
    }
    // The microphone jack with bias voltage and +20 dB; the digital
    // microphone without either.
    assert!(has(0x18, verb::set_pin_control(pin_ctl::IN | pin_ctl::VREF_80)));
    assert!(has(0x18, verb::set_amp(Amp::Input(0), false, 2)));
    assert!(has(0x12, verb::set_pin_control(pin_ctl::IN)));
    assert!(has(0x12, verb::set_amp(Amp::Input(0), false, 0)));
    // Unused pins off; the S/PDIF converter untouched.
    for pin in [0x17, 0x19, 0x1A, 0x1B, 0x1D, 0x1E] {
        assert!(has(pin, verb::set_pin_control(0)), "pin {:#x}", pin);
    }
    assert!(!c.iter().any(|&(nid, _)| nid == 0x06));
}

#[test]
fn laptop_jacks() {
    let mut fake = alc269();
    let codec = read(&mut fake);
    let r = Routing::new(&codec, FORMAT);
    // Headphones in: the speakers go off.
    assert_eq!(
        r.output_commands(&codec, true),
        [(0x14, verb::set_pin_control(0)), (0x15, verb::set_pin_control(pin_ctl::OUT | pin_ctl::HEADPHONE))]
    );
    assert_eq!(r.output_commands(&codec, false)[0], (0x14, verb::set_pin_control(pin_ctl::OUT)));
    // The digital microphone: input 6 of the capture mixer is the only one
    // open, and the ADC is at 0 dB.
    let c = r.input_commands(&codec, 0);
    assert!(c.contains(&(0x08, verb::set_amp(Amp::Input(0), false, 0x17))));
    assert!(c.contains(&(0x23, verb::set_amp(Amp::Input(6), false, 0))));
    for i in 0..6 {
        assert!(c.contains(&(0x23, verb::set_amp(Amp::Input(i), true, 0))));
    }
    // The microphone jack: input 0.
    let c = r.input_commands(&codec, 1);
    assert!(c.contains(&(0x23, verb::set_amp(Amp::Input(0), false, 0))));
    assert!(c.contains(&(0x23, verb::set_amp(Amp::Input(6), true, 0))));
}

#[test]
fn desktop_routing() {
    let mut fake = alc887();
    let codec = read(&mut fake);
    let r = Routing::new(&codec, FORMAT);
    // The front pair of the 7.1 output, and the front headphones on a DAC
    // of their own; surround, centre and side stay off.
    let outputs: Vec<(OutputKind, u8, Vec<u8>)> = r.outputs.iter().map(|o| (o.kind, o.pin, o.path.clone())).collect();
    assert_eq!(
        outputs,
        [(OutputKind::LineOut, 0x14, vec![0x14, 0x0C, 0x02]), (OutputKind::Headphone, 0x1B, vec![0x1B, 0x0D, 0x03])]
    );
    let c = r.setup(&codec, Some((1, 0x0011)), Some((2, 0x0011)));
    for pin in [0x15, 0x16, 0x17] {
        assert!(c.contains(&(pin, verb::set_pin_control(0))), "pin {:#x}", pin);
    }
    assert!(c.contains(&(0x02, verb::set_amp(Amp::Output, false, 0x40))));
    // Headphones in: the line output goes off too.
    assert!(r.output_commands(&codec, true).contains(&(0x14, verb::set_pin_control(0))));
    // The microphones and the line input, through the first ADC.
    assert_eq!(r.adc, Some(0x08));
    let inputs: Vec<(InputKind, u8)> = r.inputs.iter().map(|i| (i.kind, i.pin)).collect();
    assert_eq!(inputs, [(InputKind::Mic, 0x18), (InputKind::LineIn, 0x1A), (InputKind::Mic, 0x19)]);
    assert!(r.inputs[2].front && !r.inputs[0].front);
    // Nothing plugged in: the back microphone; the front one when plugged
    // in; the line input when it is the only thing plugged in.
    assert_eq!(r.active_input(|_| false), Some(0));
    assert_eq!(r.active_input(|pin| pin == 0x19 || pin == 0x18), Some(2));
    assert_eq!(r.active_input(|pin| pin == 0x1A), Some(1));
}

#[test]
fn qemu_routing() {
    let mut fake = qemu_duplex();
    let codec = read(&mut fake);
    assert_eq!(codec.name(), "QEMU HDA codec");
    let r = Routing::new(&codec, FORMAT);
    assert_eq!(r.outputs.len(), 1);
    let o = &r.outputs[0];
    assert_eq!((o.kind, o.pin, &o.path[..], o.jack), (OutputKind::LineOut, 0x03, &[0x03, 0x02][..], false));
    assert_eq!((r.adc, r.inputs.len()), (Some(0x04), 1));
    assert_eq!((r.inputs[0].kind, &r.inputs[0].path[..]), (InputKind::LineIn, &[0x04, 0x05][..]));
    // Nothing tells whether the line input is plugged in: it records.
    assert_eq!(r.active_input(|_| false), Some(0));
    let c = r.setup(&codec, Some((1, 0x0011)), Some((2, 0x0011)));
    assert!(c.contains(&(0x02, verb::set_amp(Amp::Output, false, 0x4A))));
    assert!(c.contains(&(0x03, verb::set_pin_control(pin_ctl::OUT))));
    assert!(c.contains(&(0x05, verb::set_pin_control(pin_ctl::IN))));
    assert_eq!(r.input_commands(&codec, 0), [(0x04, verb::set_amp(Amp::Input(0), false, 0x4A))]);
    assert!(r.jack_pins().is_empty());
}

#[test]
fn outputs_share_a_dac_when_there_is_one() {
    // One DAC, a mixer, the speakers and a headphone jack.
    let mut fake = FakeCodec::new(
        0x14F1_5051,
        vec![
            node(0x02, DAC | STEREO | OUT_AMP | AMP_OVERRIDE).amps(0, 0x0003_4040),
            node(0x0C, MIX | STEREO | IN_AMP).from(&[0x02]),
            node(0x14, PIN | STEREO).pin(OUT | EAPD, 0x9017_0110).from(&[0x0C]),
            node(0x15, PIN | STEREO).pin(OUT | PRESENCE | HP_DRIVE, 0x0221_101F).from(&[0x0C]),
        ],
    );
    let codec = read(&mut fake);
    let r = Routing::new(&codec, FORMAT);
    assert_eq!(r.outputs.len(), 2);
    assert_eq!(r.outputs[1].path, [0x15, 0x0C, 0x02]);
    assert_eq!(r.dacs, [0x02]);
}

#[test]
fn a_shared_selector_keeps_one_input() {
    // Two pins behind one selector that chooses between two DACs: once the
    // first output has it on DAC 0x02, the second shares that DAC rather
    // than switching the selector away.
    let mut fake = FakeCodec::new(
        0x111D_76D1,
        vec![
            node(0x02, DAC | STEREO),
            node(0x03, DAC | STEREO),
            node(0x0E, SEL | STEREO).from(&[0x02, 0x03]),
            node(0x14, PIN | STEREO).pin(OUT, 0x9017_0110).from(&[0x0E]),
            node(0x15, PIN | STEREO).pin(OUT | PRESENCE, 0x0221_101F).from(&[0x0E]),
        ],
    );
    let codec = read(&mut fake);
    let r = Routing::new(&codec, FORMAT);
    assert_eq!(r.outputs[0].path, [0x14, 0x0E, 0x02]);
    assert_eq!(r.outputs[1].path, [0x15, 0x0E, 0x02]);
    assert_eq!(r.dacs, [0x02]);
    assert!(r.setup(&codec, None, None).contains(&(0x0E, verb::set_connection_select(0))));
}

#[test]
fn long_connection_lists_and_ranges() {
    // Node 0x20 lists, in the long form, 0x0010 and then 0x8014: a range
    // (bit 15) that ends at 0x14 and starts after 0x10.
    struct Ranged(FakeCodec);
    impl Bus for Ranged {
        fn command(&mut self, nid: u8, v: u32) -> Option<u32> {
            if nid == 0x20 && v >> 8 == verb::id::GET_CONNECTION_LIST as u32 {
                return Some(0x8014 << 16 | 0x0010);
            }
            self.0.command(nid, v)
        }
    }
    let mut fake =
        FakeCodec::new(0x10EC_0888, vec![node(0x10, MIX | STEREO), node(0x20, MIX | STEREO).from(&[0x10, 0x14])]);
    fake.long_lists = true;
    let codec = Codec::read(0, &mut Ranged(fake)).unwrap();
    assert_eq!(codec.widget(0x20).unwrap().connections, [0x10, 0x11, 0x12, 0x13, 0x14]);
    // The short form reads four entries per response.
    let mut short = alc269();
    let codec = read(&mut short);
    assert_eq!(codec.widget(0x0B).unwrap().connections, [0x18, 0x19, 0x1A, 0x1B, 0x1D]);
}

#[test]
fn hdmi_and_modem_codecs() {
    // An HDMI codec: a digital converter and an HDMI pin. Nothing to play
    // through for this driver.
    let mut fake = FakeCodec::new(
        0x8086_280B,
        vec![
            node(0x02, DAC | STEREO | DIGITAL),
            node(0x05, PIN | STEREO | DIGITAL).pin(OUT | PRESENCE | HDMI, 0x1856_0010).from(&[0x02]),
        ],
    );
    let codec = read(&mut fake);
    assert_eq!(codec.name(), "Intel 280b");
    let r = Routing::new(&codec, FORMAT);
    assert!(r.outputs.is_empty() && r.inputs.is_empty());
    // A modem codec has no audio function group.
    let mut modem = FakeCodec::new(0x14F1_2C06, vec![node(0x02, DAC)]);
    modem.function_type = 2;
    assert_eq!(Codec::read(0, &mut modem), Err(CodecError::NoAudioFunction));
}

#[test]
fn external_amplifiers_and_built_in_line_outputs() {
    // A Conexant-like codec: the speakers' amplifier is powered through a
    // pin of its own (0x1F, no path), and the firmware calls the built-in
    // speakers a line output.
    let mut fake = FakeCodec::new(
        0x14F1_5069,
        vec![
            node(0x10, DAC | STEREO | OUT_AMP | AMP_OVERRIDE).amps(0, 0x0003_4040),
            node(0x11, DAC | STEREO | OUT_AMP | AMP_OVERRIDE).amps(0, 0x0003_4040),
            node(0x16, PIN | STEREO).pin(OUT | PRESENCE | HP_DRIVE, 0x0221_101F).from(&[0x10, 0x11]),
            node(0x1F, PIN | STEREO).pin(OUT | EAPD, 0x9017_0110).from(&[0x10, 0x11]),
            node(0x1A, PIN | STEREO).pin(OUT | EAPD, 0x4000_00F0),
        ],
    );
    fake.nodes[3].config = 0x9001_0120;
    let codec = read(&mut fake);
    let r = Routing::new(&codec, FORMAT);
    let kinds: Vec<(OutputKind, u8)> = r.outputs.iter().map(|o| (o.kind, o.pin)).collect();
    // In the firmware's order: the headphones' association comes first.
    assert_eq!(kinds, [(OutputKind::Headphone, 0x16), (OutputKind::Speaker, 0x1F)]);
    let c = r.setup(&codec, Some((1, 0x0011)), None);
    // The amplifier of the unused pin is on too.
    assert!(c.contains(&(0x1A, verb::set_eapd(verb::EAPD))));
    assert!(c.contains(&(0x1F, verb::set_eapd(verb::EAPD))));
    // Headphones in: the built-in "line out" goes off like speakers do.
    assert!(r.output_commands(&codec, true).contains(&(0x1F, verb::set_pin_control(0))));
}

#[test]
fn amplifier_gains() {
    // Realtek's DAC: 0.75 dB steps, 0 dB at the top.
    let dac = AmpCaps::parse(0x0002_5757);
    assert_eq!((dac.unity(), dac.quarter_db(0)), (0x57, -0x57 * 3));
    // A boost: 10 dB steps from 0 dB.
    let boost = AmpCaps::parse(BOOST);
    assert_eq!((boost.unity(), boost.gain_for_db(20), boost.gain_for_db(25), boost.gain_for_db(100)), (0, 2, 2, 3));
    // An ADC with 0 dB at 0x17 and room above.
    let adc = AmpCaps::parse(0x8002_3F17);
    assert!(adc.mute);
    assert_eq!((adc.unity(), adc.gain_for_db(-3)), (0x17, 0x13));
    // Mute only.
    assert_eq!(AmpCaps::parse(MUTE_ONLY).unity(), 0);
}

fn set_config(fake: &mut FakeCodec, nid: u8, config: u32) {
    if let Some(n) = fake.nodes.iter_mut().find(|n| n.nid == nid) {
        n.config = config;
    }
}

#[test]
fn realtek_eapd_follows_the_verb() {
    // An ALC269VC (revision 0x30), whose EAPD the codec runs itself.
    let mut fake = alc269();
    fake.coefs.insert((0x20, 0x00), 0x8030);
    fake.coefs.insert((0x20, 0x10), 0x0A20);
    let codec = read(&mut fake);
    // The assembly ID (the configuration of pin 0x1D) says EAPD, no GPIO.
    assert_eq!(vendor::prepare(&codec, &mut fake, Some((0x1043, 0x1C13))), Prepared::default());
    assert_eq!(fake.coefs[&(0x20, 0x10)], 0x0820);
    assert!(!fake.sent.iter().any(|&(_, v)| v >> 8 == verb::id::SET_GPIO_DATA as u32));
    // An ALC269VA (revision 0x10) has the switch elsewhere.
    let mut fake = alc269();
    fake.coefs.insert((0x20, 0x00), 0x0010);
    let codec = read(&mut fake);
    vendor::prepare(&codec, &mut fake, None);
    assert_eq!(fake.coefs[&(0x20, 0x0D)], 0x4000);
    // An ALC256, as recent laptops have.
    let mut fake = alc269();
    fake.vendor = 0x10EC_0256;
    fake.coefs.insert((0x20, 0x10), 0x0220);
    let codec = read(&mut fake);
    vendor::prepare(&codec, &mut fake, None);
    assert_eq!(fake.coefs[&(0x20, 0x10)], 0x0020);
}

#[test]
fn amplifiers_on_a_gpio() {
    // The never-connected pin's assembly ID: the amplifier on GPIO 0.
    let mut fake = alc269();
    set_config(&mut fake, 0x1D, 0x4001_0009);
    let codec = read(&mut fake);
    assert_eq!(vendor::prepare(&codec, &mut fake, Some((0x1043, 0x1C13))), Prepared { amplifier_gpio: 0x01 });
    for id in [verb::id::SET_GPIO_ENABLE, verb::id::SET_GPIO_DIRECTION, verb::id::SET_GPIO_DATA] {
        assert!(fake.sent.contains(&(0x01, verb::verb(id, 0x01))));
    }
    // With a wrong checksum it is no assembly ID.
    let mut fake = alc269();
    set_config(&mut fake, 0x1D, 0x4002_0009);
    let codec = read(&mut fake);
    assert_eq!(vendor::prepare(&codec, &mut fake, None), Prepared::default());
    // The codec's subsystem ID, when it is not the board's: GPIO 1.
    let mut fake = alc269();
    fake.subsystem = 0x1043_0019;
    let codec = read(&mut fake);
    assert_eq!(vendor::prepare(&codec, &mut fake, Some((0x1043, 0x1C13))).amplifier_gpio, 0x02);
    // When it is the board's, the pin's assembly ID counts (EAPD).
    let mut fake = alc269();
    fake.subsystem = 0x1043_0019;
    let codec = read(&mut fake);
    assert_eq!(vendor::prepare(&codec, &mut fake, Some((0x1043, 0x0019))).amplifier_gpio, 0);
    // A board whose assembly ID is known to be wrong.
    let mut fake = alc269();
    fake.subsystem = 0x17AA_0019;
    let codec = read(&mut fake);
    assert_eq!(vendor::prepare(&codec, &mut fake, Some((0x17AA, 0x21B8))).amplifier_gpio, 0);
}

#[test]
fn older_codecs_drive_eapd_through_a_coefficient() {
    // An ALC888 (revision 0x10) without an amplifier on a GPIO.
    let mut fake = alc887();
    fake.vendor = 0x10EC_0888;
    fake.coefs.insert((0x20, 0x00), 0x0010);
    let codec = read(&mut fake);
    vendor::prepare(&codec, &mut fake, None);
    assert_eq!(fake.coefs[&(0x20, 0x07)], 0x2030);
    // The ALC260's is on node 0x1A.
    let mut fake = alc887();
    fake.vendor = 0x10EC_0260;
    let codec = read(&mut fake);
    vendor::prepare(&codec, &mut fake, None);
    assert_eq!(fake.coefs[&(0x1A, 0x07)], 0x2010);
}

#[test]
fn other_codecs_are_left_alone() {
    let mut fake = qemu_duplex();
    let codec = read(&mut fake);
    assert_eq!(vendor::prepare(&codec, &mut fake, Some((0x1AF4, 0x1100))), Prepared::default());
    assert!(fake.sent.is_empty() && fake.coefs.is_empty());
}
