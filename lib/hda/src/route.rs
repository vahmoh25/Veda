//! Choosing what plays and what records, and the commands that set the
//! codec up for it.
//!
//! [`Routing::new`] finds the outputs worth driving — speakers, headphone
//! jacks and the front pair of each line output — and for each a path of
//! widgets from a DAC to the pin, giving every output its own DAC while
//! there are enough. It finds the inputs — built-in microphones, microphone
//! jacks and line inputs — and the ADC that reaches most of them, with a
//! path to each.
//!
//! Every DAC takes the same stream, so all outputs play the same stereo
//! sound at unity gain. While headphones are plugged in, the speakers and
//! line outputs are switched off ([`Routing::output_commands`]); the ADC
//! records a microphone plugged into a jack before a built-in one
//! ([`Routing::active_input`], [`Routing::input_commands`]). Everything
//! the paths do not use — the analog loopback above all, which would feed
//! the microphones to the speakers — stays muted.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec;
use alloc::vec::Vec;

use crate::codec::{Codec, Connectivity, Device, Widget, WidgetType, caps, pin_caps};
use crate::verb::{self, Amp, StreamFormat, pin_ctl};

/// A command for the codec: a node and a verb.
pub type Command = (u8, u32);

/// Microphone preamplification (the "boost" of a microphone pin), in dB.
pub const MIC_BOOST_DB: i32 = 20;
/// Widgets between a pin and its converter at most.
const MAX_DEPTH: usize = 6;
/// Amplifier indexes are four bits wide.
const MAX_AMP_INDEX: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutputKind {
    Speaker,
    Headphone,
    LineOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputKind {
    /// A microphone jack.
    Mic,
    /// A microphone built into the computer.
    InternalMic,
    LineIn,
}

/// An output pin and its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub kind: OutputKind,
    pub pin: u8,
    /// From the pin to the DAC.
    pub path: Vec<u8>,
    /// The pin tells when something is plugged in.
    pub jack: bool,
}

impl Output {
    pub fn dac(&self) -> u8 {
        *self.path.last().unwrap_or(&self.pin)
    }
}

/// An input pin and its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub kind: InputKind,
    pub pin: u8,
    /// From the ADC to the pin.
    pub path: Vec<u8>,
    /// The pin tells when something is plugged in.
    pub jack: bool,
    /// On the front of the computer.
    pub front: bool,
    /// A digital microphone (no bias voltage, no preamplifier needed).
    pub digital: bool,
}

/// The outputs and inputs of a codec, and the converters they use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Routing {
    pub outputs: Vec<Output>,
    /// The DACs of the outputs, which all take the output stream.
    pub dacs: Vec<u8>,
    pub inputs: Vec<Input>,
    /// The ADC every input is routed to.
    pub adc: Option<u8>,
}

/// The order in which pins are considered: by association and sequence,
/// which the firmware sets by priority.
fn pin_order(w: &Widget) -> (u8, u8, u8) {
    (w.config.association(), w.config.sequence(), w.nid)
}

fn output_kind(w: &Widget) -> Option<OutputKind> {
    if w.kind != WidgetType::Pin
        || w.config.connectivity() == Connectivity::None
        || !w.pin_has(pin_caps::OUTPUT)
        || w.digital()
    {
        return None;
    }
    match w.config.device() {
        Device::Speaker => Some(OutputKind::Speaker),
        Device::Headphone => Some(OutputKind::Headphone),
        // A built-in "line out" is a speaker by another name.
        Device::LineOut if w.config.connectivity() == Connectivity::Internal => Some(OutputKind::Speaker),
        Device::LineOut => Some(OutputKind::LineOut),
        _ => None,
    }
}

fn input_kind(w: &Widget) -> Option<InputKind> {
    if w.kind != WidgetType::Pin
        || w.config.connectivity() == Connectivity::None
        || !w.pin_has(pin_caps::INPUT)
        || w.digital()
    {
        return None;
    }
    match w.config.device() {
        Device::Mic if w.config.connectivity() == Connectivity::Internal || w.config.inside() => {
            Some(InputKind::InternalMic)
        }
        Device::Mic => Some(InputKind::Mic),
        Device::LineIn => Some(InputKind::LineIn),
        _ => None,
    }
}

/// Whether a converter can run at `format` (a converter that reports no
/// formats is given the benefit of the doubt).
fn converter_fits(w: &Widget, format: StreamFormat) -> bool {
    !w.digital() && (w.pcm == 0 || format.supported_by(w.pcm))
}

/// The shortest path from `from` through mixers and selectors to a widget
/// that `is_target` accepts. `fixed` holds the input already chosen for
/// widgets that other paths go through; a path must agree with it.
fn find_path(
    codec: &Codec,
    from: u8,
    fixed: &BTreeMap<u8, u8>,
    mut is_target: impl FnMut(&Widget) -> bool,
) -> Option<Vec<u8>> {
    let mut parent: BTreeMap<u8, u8> = BTreeMap::new();
    let mut depth: BTreeMap<u8, usize> = BTreeMap::from([(from, 0)]);
    let mut queue = VecDeque::from([from]);
    while let Some(nid) = queue.pop_front() {
        let Some(w) = codec.widget(nid) else { continue };
        let d = depth[&nid];
        if d >= MAX_DEPTH {
            continue;
        }
        let inputs = match fixed.get(&nid) {
            Some(&chosen) => vec![chosen],
            None => w.connections.clone(),
        };
        for next in inputs {
            if depth.contains_key(&next) {
                continue;
            }
            let Some(n) = codec.widget(next) else { continue };
            if is_target(n) {
                parent.insert(next, nid);
                let mut path = vec![next];
                let mut at = next;
                while let Some(&p) = parent.get(&at) {
                    path.push(p);
                    at = p;
                }
                path.reverse();
                return Some(path);
            }
            if matches!(n.kind, WidgetType::Mixer | WidgetType::Selector) {
                parent.insert(next, nid);
                depth.insert(next, d + 1);
                queue.push_back(next);
            }
        }
    }
    None
}

impl Routing {
    /// Chooses the outputs and inputs of `codec` and their paths, for
    /// streams in `format`.
    pub fn new(codec: &Codec, format: StreamFormat) -> Routing {
        let mut routing = Routing::default();
        // Every pin each output path passes through keeps its input.
        let mut fixed: BTreeMap<u8, u8> = BTreeMap::new();
        let mut candidates: Vec<(OutputKind, &Widget)> =
            codec.widgets.iter().filter_map(|w| output_kind(w).map(|k| (k, w))).collect();
        candidates.sort_by_key(|(_, w)| pin_order(w));
        let mut line_associations: Vec<u8> = Vec::new();
        for (kind, pin) in candidates {
            // Of a multichannel line output, only the front pair (the
            // first of its association) plays.
            let association = pin.config.association();
            if kind == OutputKind::LineOut && !matches!(association, 0 | 0xF) {
                if line_associations.contains(&association) {
                    continue;
                }
                line_associations.push(association);
            }
            let dac_free = |w: &Widget| {
                w.kind == WidgetType::Output && converter_fits(w, format) && !routing.dacs.contains(&w.nid)
            };
            let any_dac = |w: &Widget| w.kind == WidgetType::Output && converter_fits(w, format);
            let Some(path) =
                find_path(codec, pin.nid, &fixed, dac_free).or_else(|| find_path(codec, pin.nid, &fixed, any_dac))
            else {
                continue;
            };
            for pair in path.windows(2) {
                fixed.insert(pair[0], pair[1]);
            }
            let dac = *path.last().unwrap_or(&pin.nid);
            if !routing.dacs.contains(&dac) {
                routing.dacs.push(dac);
            }
            routing.outputs.push(Output { kind, pin: pin.nid, path, jack: pin.jack_detect() });
        }

        let mut candidates: Vec<(InputKind, &Widget)> =
            codec.widgets.iter().filter_map(|w| input_kind(w).map(|k| (k, w))).collect();
        candidates.sort_by_key(|(_, w)| pin_order(w));
        // The ADC that reaches the most inputs.
        let no_fixed = BTreeMap::new();
        let mut best: Option<(u8, Vec<Input>)> = None;
        for adc in codec.widgets.iter().filter(|w| w.kind == WidgetType::Input && converter_fits(w, format)) {
            let inputs: Vec<Input> = candidates
                .iter()
                .filter_map(|&(kind, pin)| {
                    let path = find_path(codec, adc.nid, &no_fixed, |w| w.nid == pin.nid)?;
                    Some(Input {
                        kind,
                        pin: pin.nid,
                        path,
                        jack: pin.jack_detect(),
                        front: pin.config.front(),
                        digital: pin.config.connection_type() == 6,
                    })
                })
                .collect();
            if !inputs.is_empty() && best.as_ref().is_none_or(|(_, b)| inputs.len() > b.len()) {
                best = Some((adc.nid, inputs));
            }
        }
        if let Some((adc, inputs)) = best {
            routing.adc = Some(adc);
            routing.inputs = inputs;
        }
        routing
    }

    fn uses_pin(&self, nid: u8) -> bool {
        self.outputs.iter().any(|o| o.pin == nid) || self.inputs.iter().any(|i| i.pin == nid)
    }

    /// The commands that set the codec up: everything powered (external
    /// amplifiers too: some codecs power the speakers' through a pin of
    /// their own), the unused mixer inputs muted and the unused pins off,
    /// the output paths open at unity gain, the input pins enabled (with
    /// bias voltage and preamplification for microphones), and the
    /// converters on their streams. `output` and `input` are (stream tag,
    /// format).
    pub fn setup(&self, codec: &Codec, output: Option<(u8, u16)>, input: Option<(u8, u16)>) -> Vec<Command> {
        let mut c: Vec<Command> = vec![(codec.afg, verb::set_power_state(0))];
        for w in &codec.widgets {
            if w.has(caps::POWER_CONTROL) {
                c.push((w.nid, verb::set_power_state(0)));
            }
            if w.kind == WidgetType::Pin && w.pin_has(pin_caps::EAPD) {
                c.push((w.nid, verb::set_eapd(verb::EAPD)));
            }
            match w.kind {
                WidgetType::Mixer if w.has(caps::IN_AMP) => {
                    for i in 0..w.connections.len().min(MAX_AMP_INDEX) {
                        c.push((w.nid, verb::set_amp(Amp::Input(i as u8), true, 0)));
                    }
                }
                WidgetType::Pin if !self.uses_pin(w.nid) => c.push((w.nid, verb::set_pin_control(0))),
                _ => {}
            }
        }
        for o in &self.outputs {
            self.output_path(codec, o, &mut c);
        }
        if let Some((tag, format)) = output {
            for &dac in &self.dacs {
                c.push((dac, verb::set_format(format)));
                c.push((dac, verb::set_stream_channel(tag, 0)));
            }
        }
        for i in &self.inputs {
            let Some(pin) = codec.widget(i.pin) else { continue };
            let analog_mic = matches!(i.kind, InputKind::Mic | InputKind::InternalMic) && !i.digital;
            c.push((i.pin, verb::set_pin_control(pin_ctl::IN | if analog_mic { mic_bias(pin) } else { 0 })));
            if pin.has(caps::IN_AMP) {
                let gain = if analog_mic { pin.amp_in.gain_for_db(MIC_BOOST_DB) } else { pin.amp_in.unity() };
                c.push((i.pin, verb::set_amp(Amp::Input(0), false, gain)));
            }
        }
        if let (Some(adc), Some((tag, format))) = (self.adc, input) {
            c.push((adc, verb::set_format(format)));
            c.push((adc, verb::set_stream_channel(tag, 0)));
        }
        c
    }

    /// Opens an output path: each widget's input on it chosen and
    /// unmuted, every amplifier at 0 dB, the pin on.
    fn output_path(&self, codec: &Codec, o: &Output, c: &mut Vec<Command>) {
        for (k, &nid) in o.path.iter().enumerate() {
            let Some(w) = codec.widget(nid) else { continue };
            if let Some(index) = o.path.get(k + 1).and_then(|&next| w.connection_index(next)) {
                if matches!(w.kind, WidgetType::Pin | WidgetType::Selector) && w.connections.len() > 1 {
                    c.push((nid, verb::set_connection_select(index)));
                }
                if matches!(w.kind, WidgetType::Mixer | WidgetType::Selector) && w.has(caps::IN_AMP) {
                    c.push((nid, verb::set_amp(Amp::Input(index), false, w.amp_in.unity())));
                }
            }
            if w.has(caps::OUT_AMP) {
                c.push((nid, verb::set_amp(Amp::Output, false, w.amp_out.unity())));
            }
            if w.kind == WidgetType::Pin {
                c.push((nid, verb::set_pin_control(output_control(w, o.kind))));
            }
        }
    }

    /// Whether headphones are plugged in (`present` tells whether something
    /// is plugged into a pin).
    pub fn headphones_in(&self, present: impl Fn(u8) -> bool) -> bool {
        self.outputs.iter().any(|o| o.kind == OutputKind::Headphone && o.jack && present(o.pin))
    }

    /// Whether an output plays: with headphones in, only headphones do.
    pub fn output_enabled(&self, output: &Output, headphones_in: bool) -> bool {
        !headphones_in || output.kind == OutputKind::Headphone
    }

    /// Switches the outputs' pins on or off for whether headphones are in.
    pub fn output_commands(&self, codec: &Codec, headphones_in: bool) -> Vec<Command> {
        self.outputs
            .iter()
            .filter_map(|o| {
                let w = codec.widget(o.pin)?;
                let control = if self.output_enabled(o, headphones_in) { output_control(w, o.kind) } else { 0 };
                Some((o.pin, verb::set_pin_control(control)))
            })
            .collect()
    }

    /// The input to record: a microphone plugged in (the front one first),
    /// the built-in microphone, a plugged-in line input; then a microphone
    /// jack that cannot tell whether anything is plugged in.
    pub fn active_input(&self, present: impl Fn(u8) -> bool) -> Option<usize> {
        let rank = |i: &Input| -> u8 {
            let plugged = i.jack && present(i.pin);
            match i.kind {
                InputKind::Mic if plugged => {
                    if i.front {
                        0
                    } else {
                        1
                    }
                }
                InputKind::InternalMic => 2,
                InputKind::LineIn if plugged => 3,
                InputKind::Mic if !i.jack => 4,
                InputKind::Mic => 5,
                InputKind::LineIn => 6,
            }
        };
        (0..self.inputs.len()).min_by_key(|&k| (rank(&self.inputs[k]), k))
    }

    /// Routes input `active` to the ADC: each widget on its path takes it
    /// (a mixer has every other input muted), at 0 dB.
    pub fn input_commands(&self, codec: &Codec, active: usize) -> Vec<Command> {
        let mut c = Vec::new();
        let Some(input) = self.inputs.get(active) else { return c };
        for pair in input.path.windows(2) {
            let (nid, next) = (pair[0], pair[1]);
            let Some(w) = codec.widget(nid) else { continue };
            let Some(index) = w.connection_index(next) else { continue };
            match w.kind {
                WidgetType::Mixer => {
                    if w.has(caps::IN_AMP) {
                        for i in 0..w.connections.len().min(MAX_AMP_INDEX) {
                            let on = i == index as usize;
                            c.push((nid, verb::set_amp(Amp::Input(i as u8), !on, w.amp_in.unity())));
                        }
                    }
                }
                _ => {
                    if w.connections.len() > 1 {
                        c.push((nid, verb::set_connection_select(index)));
                    }
                    if w.has(caps::IN_AMP) {
                        c.push((nid, verb::set_amp(Amp::Input(index), false, w.amp_in.unity())));
                    }
                }
            }
            if w.has(caps::OUT_AMP) {
                c.push((nid, verb::set_amp(Amp::Output, false, w.amp_out.unity())));
            }
        }
        c
    }

    /// The pins whose jacks are watched.
    pub fn jack_pins(&self) -> Vec<u8> {
        let mut pins: Vec<u8> = self
            .outputs
            .iter()
            .filter(|o| o.jack)
            .map(|o| o.pin)
            .chain(self.inputs.iter().filter(|i| i.jack).map(|i| i.pin))
            .collect();
        pins.sort_unstable();
        pins.dedup();
        pins
    }
}

/// The pin control of an output pin that plays.
fn output_control(w: &Widget, kind: OutputKind) -> u8 {
    let headphone = kind == OutputKind::Headphone && w.pin_has(pin_caps::HEADPHONE_DRIVE);
    pin_ctl::OUT | if headphone { pin_ctl::HEADPHONE } else { 0 }
}

/// The bias voltage for a microphone on `pin`: 80% where it can, else 50%
/// or 100%.
fn mic_bias(pin: &Widget) -> u8 {
    if pin.pin_has(pin_caps::VREF_80) {
        pin_ctl::VREF_80
    } else if pin.pin_has(pin_caps::VREF_50) {
        pin_ctl::VREF_50
    } else if pin.pin_has(pin_caps::VREF_100) {
        pin_ctl::VREF_100
    } else {
        pin_ctl::VREF_HIZ
    }
}
