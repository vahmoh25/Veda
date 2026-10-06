//! `hda` — the driver for Intel High Definition Audio: the sound hardware
//! of nearly every PC since 2005 (Intel and AMD chipsets, with codecs from
//! Realtek, Conexant, IDT, Cirrus Logic and others), QEMU's `intel-hda` and
//! VirtualBox's HD Audio.
//!
//! * **The controller** (`controller`): reset, the codecs on the link, the
//!   commands to them, what particular chipsets need, and one output and
//!   one input stream — 48 kHz (44.1 kHz on codecs without it), 16 bits,
//!   stereo, in 10 ms periods.
//! * **The codec** (`vhda`): its widgets are read, and the outputs chosen —
//!   speakers, headphone jacks, the front pair of each line output — each
//!   with a path from a DAC of its own where there are enough. They all
//!   play the same stream at unity gain; the volume is the audio
//!   service's. The input is a microphone, built in or on a jack, or a line
//!   input, through the ADC that reaches most of them. Everything else is
//!   muted, the analog loopback above all. Codecs that need more than the
//!   specification (Realtek's) are prepared first (`vhda::vendor`).
//! * **Jacks**: while headphones are plugged in, the speakers and line
//!   outputs are off; a microphone plugged into a jack records instead of
//!   the built-in one. The codec reports plugging (unsolicited responses),
//!   and the driver also looks every second.
//! * **Playback and recording** (`player`, `recorder`): through the audio
//!   service's rings, as for the other sound drivers. The controller
//!   interrupts after each period (MSI), or the driver polls.
//!
//! HDMI and DisplayPort codecs are left alone: their audio needs the
//! graphics driver's help, and Veda has no graphics driver.

#![no_std]
#![no_main]

extern crate alloc;

mod controller;
mod player;
mod recorder;
mod speakers;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::{WaitItem, signals};
use vhda::codec::{Bus, Codec, WidgetType, caps, pin_caps};
use vhda::route::{InputKind, OutputKind, Routing};
use vhda::vendor;
use vhda::verb::{self, StreamFormat};
use vproto::audio::{DeviceFormat, Ring, Role, audiodev};
use vproto::pci::pcidev;
use vrt::object::{Channel, Event};
use vrt::println;
use vrt::time::{Duration, now_ns};

use controller::Controller;
use player::Player;
use recorder::Recorder;

vrt::entry!(main);

const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
pub const CHANNELS: usize = 2;
pub const FRAME_BYTES: usize = 4;
/// The sample rates the driver runs at, best first: 48 kHz, which HD Audio
/// codecs should all have, and 44.1 kHz, for those that do not.
const RATES: [u32; 2] = [48_000, 44_100];
/// Stream tags (unique across directions, as every controller accepts).
const OUTPUT_TAG: u8 = 1;
const INPUT_TAG: u8 = 2;
/// How often a running stream is looked at without interrupts.
const POLL_NS: u64 = 4_000_000;
/// With interrupts, a safety net in case one goes missing.
const IRQ_SAFETY_NS: u64 = 25_000_000;
/// Playing this long without a single interrupt: they do not arrive, and
/// the driver polls instead.
const NO_INTERRUPTS_NS: u64 = 500_000_000;
/// How often the jacks are looked at.
const JACK_POLL_NS: u64 = 1_000_000_000;
/// How long the codec may take to power up.
const POWER_UP_NS: u64 = 500_000_000;

/// The stream format and periods of the codec in use: 16-bit stereo, and
/// periods of about 10 ms in whole multiples of 128 bytes (where HD Audio
/// buffers must start).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub rate: u32,
    /// The stream format word.
    pub format: u16,
    pub period_frames: usize,
    pub period_bytes: usize,
    pub period_ns: u64,
}

impl Timing {
    fn new(rate: u32) -> Timing {
        let frames = ((rate as usize / 100 + 16) / 32 * 32).max(32);
        Timing {
            rate,
            format: Timing::stream_format(rate).encode().unwrap_or(0x0011),
            period_frames: frames,
            period_bytes: frames * FRAME_BYTES,
            period_ns: frames as u64 * 1_000_000_000 / rate as u64,
        }
    }

    fn stream_format(rate: u32) -> StreamFormat {
        StreamFormat::new(rate, 16, CHANNELS as u8)
    }
}

/// One codec, through the controller.
struct CodecBus<'a> {
    hc: &'a mut Controller,
    address: u8,
}

impl Bus for CodecBus<'_> {
    fn command(&mut self, nid: u8, verb: u32) -> Option<u32> {
        self.hc.command(self.address, nid, verb)
    }
}

/// The codec in use, its routing and the state of its jacks.
struct Audio {
    codec: Codec,
    routing: Routing,
    timing: Timing,
    /// The watched jacks and whether something is plugged into each.
    jacks: Vec<(u8, bool)>,
    headphones: bool,
    input: Option<usize>,
}

fn output_name(kind: OutputKind) -> &'static str {
    match kind {
        OutputKind::Speaker => "speakers",
        OutputKind::Headphone => "headphones",
        OutputKind::LineOut => "line out",
    }
}

fn input_name(kind: InputKind) -> &'static str {
    match kind {
        InputKind::InternalMic => "built-in microphone",
        InputKind::Mic => "microphone jack",
        InputKind::LineIn => "line in",
    }
}

/// Logs every widget of a codec: its type, capabilities and connections,
/// and for pins their capabilities and configuration. With the controller,
/// also what the codec holds now: power states, the converters' stream
/// and format, the amplifiers' gains (`m` before a muted one's), the pins'
/// controls and EAPD, and the selected connection (marked `*`).
fn dump(codec: &Codec, mut hc: Option<&mut Controller>) {
    let address = codec.address;
    let amp = |v: u32| if v & 0x80 != 0 { format!("m{:02x}", v & 0x7F) } else { format!("{:02x}", v & 0x7F) };
    if let Some(hc) = hc.as_deref_mut() {
        let power = hc.command(address, codec.afg, verb::verb(verb::id::GET_POWER_STATE, 0)).unwrap_or(u32::MAX);
        println!(
            "codec {} function group {:#04x}: D{}, subsystem {:#010x}, revision {:#010x}",
            address,
            codec.afg,
            (power >> 4) & 0xF,
            codec.subsystem,
            codec.revision
        );
    }
    for w in &codec.widgets {
        let mut line = format!("codec {} node {:#04x}: {:?} caps {:#08x}", address, w.nid, w.kind, w.caps);
        if w.kind == WidgetType::Pin {
            line += &format!(" pin {:#010x} config {:#010x}", w.pin_caps, w.config.0);
        }
        let converter = matches!(w.kind, WidgetType::Output | WidgetType::Input);
        if converter {
            line += &format!(" pcm {:#010x}", w.pcm);
        }
        let mut selected = None;
        if let Some(hc) = hc.as_deref_mut() {
            let mut get = |v: u32| hc.command(address, w.nid, v);
            if w.has(caps::POWER_CONTROL)
                && let Some(p) = get(verb::verb(verb::id::GET_POWER_STATE, 0))
            {
                line += &format!(" D{}", (p >> 4) & 0xF);
            }
            if converter
                && let (Some(s), Some(f)) =
                    (get(verb::verb(verb::id::GET_STREAM_CHANNEL, 0)), get(verb::verb4(verb::id4::GET_FORMAT, 0)))
            {
                line += &format!(" stream {} format {:#06x}", (s >> 4) & 0xF, f & 0xFFFF);
            }
            if w.has(caps::OUT_AMP)
                && let Some(v) = get(verb::get_amp(verb::Amp::Output))
            {
                line += &format!(" out {}", amp(v));
            }
            if w.has(caps::IN_AMP) {
                let inputs = if w.kind == WidgetType::Mixer { w.connections.len().clamp(1, 16) } else { 1 };
                let gains: Vec<String> =
                    (0..inputs).filter_map(|i| get(verb::get_amp(verb::Amp::Input(i as u8)))).map(amp).collect();
                line += &format!(" in [{}]", gains.join(" "));
            }
            if w.kind == WidgetType::Pin {
                if let Some(c) = get(verb::verb(verb::id::GET_PIN_CONTROL, 0)) {
                    line += &format!(" ctl {:#04x}", c & 0xFF);
                }
                if w.pin_has(pin_caps::EAPD)
                    && let Some(e) = get(verb::verb(verb::id::GET_EAPD, 0))
                {
                    line += &format!(" eapd {:#x}", e & 0xFF);
                }
            }
            if w.connections.len() > 1 && w.kind != WidgetType::Mixer {
                selected = get(verb::verb(verb::id::GET_CONNECTION_SELECT, 0)).map(|s| (s & 0xFF) as usize);
            }
        }
        if !w.connections.is_empty() {
            let list: Vec<String> = w
                .connections
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{:02x}{}", c, if selected == Some(i) { "*" } else { "" }))
                .collect();
            line += &format!(" from [{}]", list.join(" "));
        }
        println!("{}", line);
    }
}

/// "speakers (pins 0x14, 0x17), headphones (pin 0x15)": named pins,
/// grouped by name in order of first appearance.
fn pin_list(pins: impl Iterator<Item = (&'static str, u8)>) -> String {
    let mut groups: Vec<(&str, Vec<String>)> = Vec::new();
    for (name, pin) in pins {
        let pin = format!("{:#04x}", pin);
        match groups.iter_mut().find(|g| g.0 == name) {
            Some(g) => g.1.push(pin),
            None => groups.push((name, alloc::vec![pin])),
        }
    }
    let parts: Vec<String> = groups
        .iter()
        .map(|(name, pins)| format!("{} (pin{} {})", name, if pins.len() > 1 { "s" } else { "" }, pins.join(", ")))
        .collect();
    parts.join(", ")
}

/// What a codec offers, for the log.
fn describe(routing: &Routing) -> String {
    if routing.outputs.is_empty() {
        return String::from("no speaker, headphone or line output (HDMI or DisplayPort?), not used");
    }
    let mut text = pin_list(routing.outputs.iter().map(|o| (output_name(o.kind), o.pin)));
    if !routing.inputs.is_empty() {
        text += "; ";
        text += &pin_list(routing.inputs.iter().map(|i| (input_name(i.kind), i.pin)));
    }
    text
}

impl Audio {
    /// Reads every codec on the link and takes the first one with analog
    /// outputs.
    fn probe(hc: &mut Controller) -> Result<Audio, &'static str> {
        let mut chosen = None;
        for address in 0..15u8 {
            if hc.codecs & 1 << address == 0 {
                continue;
            }
            let codec = match Codec::read(address, &mut CodecBus { hc, address }) {
                Ok(c) => c,
                Err(e) => {
                    println!("codec {}: {}", address, e);
                    continue;
                }
            };
            // The best rate at which the codec has outputs.
            let (routing, rate) = RATES
                .iter()
                .map(|&rate| (Routing::new(&codec, Timing::stream_format(rate)), rate))
                .find(|(routing, _)| !routing.outputs.is_empty())
                .unwrap_or_else(|| (Routing::default(), RATES[0]));
            let rate_note = if rate == RATES[0] { String::new() } else { format!(" at {} Hz", rate) };
            println!(
                "codec {}: {} ({:04x}:{:04x}){}: {}",
                address,
                codec.name(),
                codec.vendor_id >> 16,
                codec.vendor_id & 0xFFFF,
                rate_note,
                describe(&routing)
            );
            if routing.outputs.is_empty() && codec.widgets.iter().any(|w| w.kind == WidgetType::Pin && !w.digital()) {
                // Analog pins but no way to them: what the codec looks
                // like, for whoever finds out why.
                dump(&codec, Some(&mut *hc));
            }
            if chosen.is_none() && !routing.outputs.is_empty() {
                chosen = Some((codec, routing, rate));
            }
        }
        let (codec, routing, rate) = chosen.ok_or("no codec with speakers, headphones or a line output")?;
        Ok(Audio { codec, routing, timing: Timing::new(rate), jacks: Vec::new(), headphones: false, input: None })
    }

    fn send(&self, hc: &mut Controller, commands: &[(u8, u32)]) {
        for &(nid, v) in commands {
            if hc.command(self.codec.address, nid, v).is_none() {
                println!("codec {}: no answer from node {:#04x}", self.codec.address, nid);
                return;
            }
        }
    }

    /// Sets the codec up for playback (and recording), and follows the
    /// jacks from now on. `board` is the PCI subsystem vendor and device.
    fn configure(&mut self, hc: &mut Controller, recording: bool, board: Option<(u16, u16)>) {
        // The function group first, fully on; it may take a moment to get
        // there, and must before its widgets are set up.
        let (address, afg) = (self.codec.address, self.codec.afg);
        hc.command(address, afg, verb::set_power_state(0));
        let end = now_ns() + POWER_UP_NS;
        while hc.command(address, afg, verb::verb(verb::id::GET_POWER_STATE, 0)).is_some_and(|s| (s >> 4) & 0xF != 0)
            && now_ns() < end
        {
            vrt::time::sleep(Duration::from_millis(1));
        }
        let prepared = vendor::prepare(&self.codec, &mut CodecBus { hc, address }, board);
        if prepared.amplifier_gpio != 0 {
            println!("the speaker amplifier is switched by GPIO {}", prepared.amplifier_gpio.trailing_zeros());
        }
        let format = self.timing.format;
        let setup =
            self.routing.setup(&self.codec, Some((OUTPUT_TAG, format)), recording.then_some((INPUT_TAG, format)));
        self.send(hc, &setup);
        self.verify_converters(hc, recording);
        // Unsolicited responses from the jacks, tagged with the pin.
        for pin in self.routing.jack_pins() {
            if self.codec.widget(pin).is_some_and(|w| w.has(caps::UNSOLICITED)) {
                self.send(hc, &[(pin, verb::set_unsolicited(Some(pin & 0x3F)))]);
            }
        }
        self.check_jacks(hc, true);
    }

    /// Reads every converter's stream and format back, and sets them again
    /// (once the converter is powered) where the codec did not take them:
    /// a converter in another format than the stream plays noise. Logs
    /// what the converters hold.
    fn verify_converters(&self, hc: &mut Controller, recording: bool) {
        let (address, format) = (self.codec.address, self.timing.format);
        let read = |hc: &mut Controller, nid: u8| -> Option<(u8, u16)> {
            let stream = hc.command(address, nid, verb::verb(verb::id::GET_STREAM_CHANNEL, 0))?;
            let format = hc.command(address, nid, verb::verb4(verb::id4::GET_FORMAT, 0))?;
            Some((((stream >> 4) & 0xF) as u8, format as u16))
        };
        let converters = self.routing.dacs.iter().map(|&dac| (dac, OUTPUT_TAG));
        let converters = converters.chain(self.routing.adc.filter(|_| recording).map(|adc| (adc, INPUT_TAG)));
        let mut report: Vec<String> = Vec::new();
        for (nid, tag) in converters {
            let mut held = read(hc, nid);
            let mut again = "";
            if held != Some((tag, format)) {
                vrt::time::sleep(Duration::from_millis(10));
                let mut commands = Vec::new();
                if self.codec.widget(nid).is_some_and(|w| w.has(caps::POWER_CONTROL)) {
                    commands.push((nid, verb::set_power_state(0)));
                }
                commands.extend([(nid, verb::set_stream_channel(tag, 0)), (nid, verb::set_format(format))]);
                self.send(hc, &commands);
                held = read(hc, nid);
                again = " (set again)";
            }
            report.push(match held {
                Some((tag, f)) => format!("{:#04x} on stream {} at {:#06x}{}", nid, tag, f, again),
                None => format!("{:#04x} does not answer", nid),
            });
        }
        println!("converters {} ({:#06x} wanted)", report.join(", "), format);
    }

    /// Whether something is plugged into `pin`.
    fn sense(&self, hc: &mut Controller, pin: u8) -> bool {
        let address = self.codec.address;
        if self.codec.widget(pin).is_some_and(|w| w.pin_has(pin_caps::TRIGGER_REQUIRED)) {
            hc.command(address, pin, verb::verb(verb::id::EXECUTE_PIN_SENSE, 0));
            vrt::time::sleep(Duration::from_millis(1));
        }
        hc.command(address, pin, verb::get_pin_sense()).is_some_and(verb::presence)
    }

    /// Reads the jacks and switches the outputs and the input for them.
    fn check_jacks(&mut self, hc: &mut Controller, force: bool) {
        let jacks: Vec<(u8, bool)> = self.routing.jack_pins().into_iter().map(|p| (p, self.sense(hc, p))).collect();
        if !force && jacks == self.jacks {
            return;
        }
        self.jacks = jacks;
        let present = |pin: u8| self.jacks.iter().any(|&(p, on)| p == pin && on);
        let headphones = self.routing.headphones_in(present);
        if force || headphones != self.headphones {
            self.send(hc, &self.routing.output_commands(&self.codec, headphones));
            let mut playing: Vec<&str> = self
                .routing
                .outputs
                .iter()
                .filter(|o| self.routing.output_enabled(o, headphones))
                .map(|o| output_name(o.kind))
                .collect();
            playing.dedup();
            let change = match (force, headphones) {
                (true, _) => "",
                (false, true) => "headphones plugged in: ",
                (false, false) => "headphones unplugged: ",
            };
            println!("{}playing through the {}", change, playing.join(" and "));
            self.headphones = headphones;
        }
        let input = self.routing.active_input(present);
        if force || input != self.input {
            if let Some(k) = input {
                self.send(hc, &self.routing.input_commands(&self.codec, k));
                let i = &self.routing.inputs[k];
                println!("recording from the {} (pin {:#04x})", input_name(i.kind), i.pin);
            }
            self.input = input;
        }
    }
}

/// The controller, set up.
struct Device {
    hc: Controller,
    audio: Audio,
    player: Player,
    recorder: Option<Recorder>,
    next_jack_check: u64,
    /// Interrupts seen; whether the driver polls although it has an
    /// interrupt (which never came).
    interrupts: u64,
    polling: bool,
    /// devmgr's channel for the device: open while the driver runs.
    _pci: pcidev::Client,
}

fn setup() -> Result<Device, String> {
    let h = vrt::env::take_handle(PCIDEV_ROLE).ok_or("no pcidev channel")?;
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let info = pci.info().map_err(|_| "devmgr went away")?;
    let location = format!("{:02x}:{:02x}.{}", info.bus, info.slot, info.function);
    let mut hc = Controller::start(&pci, &info).map_err(|e| format!("controller at {}: {}", location, e))?;
    let mut details = String::new();
    match hc.link_clock {
        Some((before, now)) if before != now => {
            details += &format!(", link clock {} MHz (the firmware's {} MHz)", now, before)
        }
        Some((_, now)) => details += &format!(", link clock {} MHz", now),
        None => {}
    }
    match hc.processing_pipe {
        Some(found) if found & ((1 << 31) - 1) != 0 => {
            details += &format!(", processing pipe was on ({:#010x}), now off", found)
        }
        Some(_) => details += ", processing pipe off",
        None => {}
    }
    if hc.position_buffer {
        details += ", positions from the position buffer";
    }
    println!(
        "controller at {} ({:04x}:{:04x}, class {:02x}.{:02x}.{:02x}): HD Audio {}.{}, {} output, {} input and {} bidirectional streams, {}{}{}",
        location,
        info.vendor,
        info.device,
        info.class,
        info.subclass,
        info.prog_if,
        hc.version.0,
        hc.version.1,
        hc.output_streams,
        hc.input_streams,
        hc.bidirectional_streams,
        if hc.irq.is_some() { "MSI" } else { "polling" },
        if hc.immediate_commands { ", immediate commands" } else { "" },
        details
    );
    let mut audio = Audio::probe(&mut hc)?;
    let timing = audio.timing;
    let output = match (hc.output_streams, hc.bidirectional_streams) {
        (0, 0) => return Err("the controller has no output stream".into()),
        (0, _) => hc.input_streams + hc.output_streams,
        _ => hc.input_streams,
    };
    let player = Player::new(hc.stream(output, OUTPUT_TAG, true, player::PERIODS, timing.period_bytes)?, timing);
    let recorder = if audio.routing.adc.is_some() && hc.input_streams > 0 {
        match hc.stream(0, INPUT_TAG, false, recorder::PERIODS, timing.period_bytes) {
            Ok(stream) => Some(Recorder::new(stream, timing)),
            Err(e) => {
                println!("no recording: {}", e);
                None
            }
        }
    } else {
        None
    };
    // The board's subsystem ID, which Realtek codecs compare theirs with.
    let board = pci
        .config_read(0x2C, 4)
        .ok()
        .and_then(|r| r.ok())
        .filter(|&v| v != 0 && v != u32::MAX)
        .map(|v| (v as u16, (v >> 16) as u16));
    audio.configure(&mut hc, recorder.is_some(), board);
    player.speakers.mute(audio.headphones);
    Ok(Device {
        hc,
        audio,
        player,
        recorder,
        next_jack_check: now_ns() + JACK_POLL_NS,
        interrupts: 0,
        polling: false,
        _pci: pci,
    })
}

/// The input side of an attachment.
struct InputSide {
    ring: Ring,
    data_event: Event,
    wake_event: Event,
}

fn format(name: &str, timing: &Timing, max_periods: u32) -> DeviceFormat {
    DeviceFormat {
        name: name.into(),
        rate: timing.rate,
        channels: CHANNELS as u32,
        period_frames: timing.period_frames as u32,
        max_periods,
    }
}

/// Connects to the audio service and attaches the device (and its input).
fn attach(
    name: &str,
    timing: &Timing,
    with_input: bool,
) -> Result<(Channel, Ring, Event, Event, Option<InputSide>), &'static str> {
    let ch = vproto::connect(audiodev::NAME).map_err(|_| "no registry")?;
    let client = audiodev::Client::new(ch);
    let link = client
        .attach(format(name, timing, player::MAX_DEPTH as u32))
        .map_err(|_| "the audio service went away")?
        .map_err(|_| "refused")?;
    let ring = Ring::map(link.ring, Role::Consumer).map_err(|_| "bad ring")?;
    if ring.rate() != timing.rate || ring.channels() != CHANNELS as u32 {
        return Err("ring format does not match the device");
    }
    let input = if with_input {
        match client.attach_input(format(name, timing, 0)) {
            Ok(Ok(l)) => Ring::map(l.ring, Role::Producer).ok().map(|ring| InputSide {
                ring,
                data_event: l.data_event,
                wake_event: l.wake_event,
            }),
            Ok(Err(e)) => {
                println!("the audio service refused the input: {}", e);
                None
            }
            Err(_) => None,
        }
    } else {
        None
    };
    Ok((client.into_channel(), ring, link.data_event, link.space_event, input))
}

/// Plays (and records) until the audio service goes away.
fn serve(
    dev: &mut Device,
    link: &Channel,
    ring: &Ring,
    data_event: &Event,
    space_event: &Event,
    input: Option<&InputSide>,
) {
    loop {
        let recording = dev.recorder.as_ref().is_some_and(|r| r.running);
        let now = now_ns();
        let streaming = dev.player.running || recording;
        let mut deadline = match (streaming, dev.hc.irq.is_some() && !dev.polling) {
            (true, false) => now + POLL_NS,
            (true, true) => now + IRQ_SAFETY_NS,
            (false, _) => dev.next_jack_check,
        };
        if dev.recorder.as_ref().is_some_and(|r| r.held_back) {
            deadline = deadline.min(now + recorder::HELD_BACK_NS);
        }
        // Unused slots wait on the data event again.
        let fallback = data_event.raw();
        let mut items = [
            WaitItem { handle: link.raw(), signals: signals::PEER_CLOSED, ..Default::default() },
            WaitItem { handle: data_event.raw(), signals: signals::SIGNALED, ..Default::default() },
            WaitItem {
                handle: input.map_or(fallback, |i| i.wake_event.raw()),
                signals: signals::SIGNALED,
                ..Default::default()
            },
            WaitItem {
                handle: dev.hc.irq.as_ref().map_or(fallback, |irq| irq.raw()),
                signals: signals::SIGNALED,
                ..Default::default()
            },
        ];
        let _ = vrt::object::wait_many(&mut items, deadline);
        if items[0].observed & signals::PEER_CLOSED != 0 {
            return;
        }
        if let Some(irq) = &dev.hc.irq {
            if items[3].observed & signals::SIGNALED != 0 {
                let _ = irq.ack();
                dev.interrupts += 1;
            } else if dev.interrupts == 0
                && !dev.polling
                && dev.player.running
                && now_ns().saturating_sub(dev.player.started_ns) > NO_INTERRUPTS_NS
            {
                println!("no interrupts come from the controller; polling it instead");
                dev.polling = true;
            }
        }
        dev.hc.service();
        let _ = data_event.clear();
        let mut changed = dev.player.reap(&dev.hc);
        changed |= dev.player.pump(&dev.hc, ring);
        dev.player.publish(ring);
        if changed {
            let _ = space_event.signal();
        }
        if dev.player.take_checked() {
            // After the output check (and a refill, which the sampling of
            // the positions has time for), the codec as it is while it
            // plays: what someone fixing a codec that plays wrongly needs.
            dev.player.log_positions(&dev.hc);
            dump(&dev.audio.codec, Some(&mut dev.hc));
        }
        if let (Some(rec), Some(input)) = (&mut dev.recorder, input) {
            let _ = input.wake_event.clear();
            rec.follow(&dev.hc, &input.ring);
            rec.reap(&dev.hc, &input.ring, &input.data_event);
        }
        // The jacks: when the codec reports a change, and every second.
        let address = dev.audio.codec.address;
        let reported = dev.hc.unsolicited.drain(..).any(|(codec, _)| codec == address);
        let now = now_ns();
        if reported || now >= dev.next_jack_check {
            dev.audio.check_jacks(&mut dev.hc, false);
            dev.next_jack_check = now + JACK_POLL_NS;
            // Amplifiers beside the codec: silent while headphones play.
            dev.player.speakers.mute(dev.audio.headphones);
        }
        if !dev.hc.healthy() {
            println!("the controller stopped answering");
            return;
        }
    }
}

/// Attaches to the audio service and plays until it goes away, forever.
fn run(mut dev: Device) {
    let name = dev.audio.codec.name();
    loop {
        match attach(&name, &dev.audio.timing, dev.recorder.is_some()) {
            Ok((link, ring, data_event, space_event, input)) => {
                println!("attached to the audio service{}", if input.is_some() { " (with recording)" } else { "" });
                dev.player.resume_at(ring.read_pos());
                serve(&mut dev, &link, &ring, &data_event, &space_event, input.as_ref());
                println!("the audio service went away");
                dev.player.stop(&dev.hc);
                if let Some(r) = &mut dev.recorder {
                    r.stop(&dev.hc);
                }
                if !dev.hc.healthy() {
                    return;
                }
            }
            Err(e) => {
                println!("cannot attach: {}", e);
                vrt::time::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn main() -> i32 {
    // Refilling the device is time-critical: the controller is set up and
    // served on a thread that runs above normal programs.
    let worker = vrt::thread::Builder::new().name("playback").priority(vabi::priority::HIGH).spawn(|| match setup() {
        Ok(dev) => {
            run(dev);
            0
        }
        Err(e) => {
            println!("{}", e);
            1
        }
    });
    match worker {
        Ok(handle) => handle.join().unwrap_or(1),
        Err(e) => {
            println!("cannot start the playback thread: {}", e);
            1
        }
    }
}
