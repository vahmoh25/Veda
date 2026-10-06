//! The amplifiers of one firmware device (`CSC3551`, one SPI connection per
//! amplifier): the board's settings for them, and bringing them up,
//! starting their DSP firmware, starting and stopping them together, as
//! Linux's `cs35l41_hda` instances do. The board around them (the SPI
//! controller, the GPIO pins they share, the firmware files) is reached
//! through [`Board`], so that this runs in Veda's driver and in host tests
//! against a simulated board alike.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::amp::{self, Amp, DSP_GAIN_PCM, Error};
use crate::config::{self, Boost, Config};
use crate::dsp::version_name;
use crate::wmfw;
use crate::{Bus, Channel};

/// A device's clock polarity and phase (SPI mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiMode {
    pub cpol: bool,
    pub cpha: bool,
}

/// One of the device's SPI connections (`SpiSerialBusV2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiLink {
    pub chip_select: u16,
    pub speed_hz: u32,
    pub mode: SpiMode,
}

/// The board failed to do something (its log says what).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardError;

/// The board around the amplifiers.
pub trait Board {
    /// A full-duplex transfer on the amplifiers' bus: `data` goes out and is
    /// replaced by what comes in. With the controller's chip select
    /// `native_cs`, or (`None`) with a GPIO selecting the device, already
    /// driven.
    fn transfer(&mut self, native_cs: Option<u8>, mode: SpiMode, data: &mut [u8]) -> Result<(), BoardError>;
    /// Drives GPIO connection `index` of the firmware device (counting its
    /// `GpioIo` and `GpioInt` resources in order).
    fn gpio_write(&mut self, index: u32, high: bool) -> Result<(), BoardError>;
    fn gpio_read(&mut self, index: u32) -> Result<bool, BoardError>;
    fn sleep_us(&mut self, us: u64);
    /// A line for the log.
    fn log(&mut self, line: &str);
    /// The chip selects the SPI controller drives itself.
    fn native_chip_selects(&self) -> u32;
    /// A firmware file by its name in the Linux firmware collection
    /// (`cirrus/...`), if the system has it.
    fn firmware(&mut self, _name: &str) -> Option<Vec<u8>> {
        None
    }
}

/// The bus speed below which Linux does not load the DSP firmware.
const MIN_FIRMWARE_HZ: u32 = 1_000_000;

/// A firmware file's name as Linux looks it up for board `ssid`, speaker
/// id `speaker` and amplifier `amp` (`cs35l41_request_firmware_file`): in
/// `cirrus/`, lower case, anything but letters, digits and dots a hyphen.
fn firmware_name(ssid: Option<&str>, speaker: Option<u8>, amp: Option<&str>, ext: &str) -> String {
    let base = "cs35l41-dsp1-spk-prot";
    let name = match (ssid, speaker, amp) {
        (Some(s), Some(id), Some(a)) => format!("{}-{}-spkid{}-{}.{}", base, s, id, a, ext),
        (Some(s), Some(id), None) => format!("{}-{}-spkid{}.{}", base, s, id, ext),
        (Some(s), None, Some(a)) => format!("{}-{}-{}.{}", base, s, a, ext),
        (Some(s), None, None) => format!("{}-{}.{}", base, s, ext),
        (None, ..) => format!("{}.{}", base, ext),
    };
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else if c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("cirrus/{}", clean)
}

/// The firmware and tuning files Linux would load into amplifier `amp`
/// (`L0`, `R0`...) of board `ssid`, in its order of preference
/// (`cs35l41_request_firmware_files`): each firmware name with the tuning
/// names that go with it. The first firmware found is the one; without a
/// tuning for it, Linux falls back to the last pair, the generic files.
pub fn firmware_files(ssid: &str, speaker: Option<u8>, amp: &str) -> Vec<(String, Vec<String>)> {
    let n = |id: Option<u8>, a: Option<&str>, ext: &str| firmware_name(Some(ssid), id, a, ext);
    let mut files = match speaker {
        Some(id) => vec![
            (n(Some(id), Some(amp), "wmfw"), vec![n(Some(id), Some(amp), "bin")]),
            (n(None, Some(amp), "wmfw"), vec![n(Some(id), Some(amp), "bin")]),
            (n(Some(id), None, "wmfw"), vec![n(Some(id), Some(amp), "bin"), n(Some(id), None, "bin")]),
            (n(None, None, "wmfw"), vec![n(Some(id), Some(amp), "bin"), n(Some(id), None, "bin")]),
        ],
        None => vec![
            (n(None, Some(amp), "wmfw"), vec![n(None, Some(amp), "bin")]),
            (n(None, None, "wmfw"), vec![n(None, Some(amp), "bin"), n(None, None, "bin")]),
        ],
    };
    files.push((firmware_name(None, None, None, "wmfw"), vec![firmware_name(None, None, None, "bin")]));
    files
}

/// Files read so far, by name (`None`: the system has no such file).
type Files = Vec<(String, Option<Vec<u8>>)>;

fn file<'f>(board: &mut impl Board, files: &'f mut Files, name: &str) -> Option<&'f [u8]> {
    let i = match files.iter().position(|(n, _)| n == name) {
        Some(i) => i,
        None => {
            files.push((name.into(), board.firmware(name)));
            files.len() - 1
        }
    };
    files[i].1.as_deref()
}

/// The firmware and tuning to load, by Linux's rules (see
/// [`firmware_files`]); or what is missing, for the log.
fn pick(
    board: &mut impl Board,
    files: &mut Files,
    candidates: &[(String, Vec<String>)],
) -> Result<(String, String), String> {
    let Some(((generic, generic_tunings), tried)) = candidates.split_last() else { return Err("no names".into()) };
    let mut missing = format!("none of its firmware files, {} and the rest, is here", candidates[0].0);
    for (fw, tunings) in tried {
        if file(board, files, fw).is_some() {
            if let Some(t) = tunings.iter().find(|t| file(board, files, t).is_some()) {
                return Ok((fw.clone(), t.clone()));
            }
            missing = format!("{} is here, but no tuning for it ({})", fw, tunings.join(" or "));
            break;
        }
    }
    match generic_tunings.first() {
        Some(t) if file(board, files, generic).is_some() && file(board, files, t).is_some() => {
            Ok((generic.clone(), t.clone()))
        }
        _ => Err(missing),
    }
}

/// The amplifier's letter in the firmware names.
fn letter(channel: Channel) -> char {
    match channel {
        Channel::Left => 'L',
        Channel::Right => 'R',
        Channel::Center => 'C',
    }
}

/// The last part of a tuning's name (the tuning tool's file).
fn short_name(name: &[u8]) -> String {
    let text = String::from_utf8_lossy(name);
    let text = text.trim_end_matches('\0').trim();
    text.rsplit(['\\', '/']).next().unwrap_or(text).into()
}

/// How an amplifier is selected: by one of the controller's chip selects,
/// or by a GPIO (connection `index` of the device).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Select {
    Native(u8),
    Gpio(u32),
}

/// The bus to one amplifier: 32-bit big-endian register addresses (the top
/// bit set to read), 16 bits of padding, 32-bit big-endian values.
struct AmpBus<'b, B> {
    board: &'b mut B,
    select: Select,
    mode: SpiMode,
}

impl<B: Board> AmpBus<'_, B> {
    fn transfer(&mut self, frame: &mut [u8]) -> Result<(), Error> {
        match self.select {
            Select::Native(cs) => self.board.transfer(Some(cs), self.mode, frame).map_err(|_| Error::Bus),
            Select::Gpio(index) => {
                self.board.gpio_write(index, false).map_err(|_| Error::Bus)?;
                let r = self.board.transfer(None, self.mode, frame).map_err(|_| Error::Bus);
                self.board.gpio_write(index, true).map_err(|_| Error::Bus)?;
                r
            }
        }
    }
}

impl<B: Board> Bus for AmpBus<'_, B> {
    fn read(&mut self, register: u32) -> Result<u32, Error> {
        let mut frame = [0u8; 10];
        frame[..4].copy_from_slice(&(register | 1 << 31).to_be_bytes());
        self.transfer(&mut frame)?;
        Ok(u32::from_be_bytes([frame[6], frame[7], frame[8], frame[9]]))
    }

    fn write(&mut self, register: u32, value: u32) -> Result<(), Error> {
        let mut frame = [0u8; 10];
        frame[..4].copy_from_slice(&register.to_be_bytes());
        frame[6..].copy_from_slice(&value.to_be_bytes());
        self.transfer(&mut frame)
    }

    /// One frame: the address, the padding, then the registers' values
    /// (the amplifier steps to the next register every four bytes).
    fn write_block(&mut self, register: u32, data: &[u8]) -> Result<(), Error> {
        let mut frame = Vec::with_capacity(6 + data.len());
        frame.extend_from_slice(&register.to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(data);
        self.transfer(&mut frame)
    }

    fn sleep_us(&mut self, us: u64) {
        self.board.sleep_us(us);
    }
}

/// One amplifier: how it is reached and, once brought up, its state.
struct Unit {
    select: Select,
    chip_select: u16,
    amp: Option<Amp>,
}

/// The amplifiers of one firmware device.
pub struct Amps {
    pub path: String,
    /// The board's subsystem id, as the firmware gives it (`_SUB`).
    ssid: String,
    mode: SpiMode,
    /// The slowest speed the firmware gives its connections.
    pub speed_hz: u32,
    config: &'static Config,
    units: Vec<Unit>,
    /// Which speakers are fitted, as the board's pin says.
    speaker_id: Option<u8>,
    muted: bool,
}

impl Amps {
    /// The amplifiers of firmware device `path`, with board id `sub` and
    /// SPI connections `links`, or why they cannot be driven. Nothing is
    /// touched yet.
    pub fn find(path: &str, sub: &str, links: &[SpiLink]) -> Result<Amps, String> {
        let unsupported = |why: String| Err(format!("{}: {}", path, why));
        if links.is_empty() {
            return unsupported("no SPI connection".into());
        }
        if sub.is_empty() {
            return unsupported("the firmware gives no board id (_SUB)".into());
        }
        let Some(config) = config::find(sub) else {
            return unsupported(format!("no settings known for board {}", sub));
        };
        if config.boost == Boost::Internal {
            return unsupported(format!(
                "board {}'s amplifiers boost their own supply, which Veda does not drive yet",
                sub
            ));
        }
        let mut units = Vec::new();
        for (i, link) in links.iter().enumerate().take(config.amps) {
            let cs = link.chip_select;
            // The second of two amplifiers may be selected by a GPIO (the
            // controller has one chip select pin on these boards).
            let select = match config.cs_gpio {
                Some(g) if config.amps == 2 && i == 1 => Select::Gpio(g as u32),
                _ => Select::Native(cs.min(0xFF) as u8),
            };
            units.push(Unit { select, chip_select: cs, amp: None });
        }
        Ok(Amps {
            path: path.into(),
            ssid: sub.into(),
            mode: links[0].mode,
            speed_hz: links.iter().map(|l| l.speed_hz).min().unwrap_or(1_000_000),
            config,
            units,
            speaker_id: None,
            muted: false,
        })
    }

    /// Resets the amplifiers and brings each up, as Linux's probe does.
    /// Logs what happened; false if none came up.
    pub fn bring_up(&mut self, board: &mut impl Board) -> bool {
        let config = self.config;
        let native = board.native_chip_selects();
        if let Some(u) = self.units.iter().find(|u| matches!(u.select, Select::Native(cs) if cs as u32 >= native)) {
            board.log(&format!(
                "{}: an amplifier at chip select {}, but the controller has {}",
                self.path, u.chip_select, native
            ));
            return false;
        }
        // A GPIO chip select deasserted before talking to the others.
        for u in &self.units {
            if let Select::Gpio(g) = u.select
                && board.gpio_write(g, true).is_err()
            {
                board.log(&format!(
                    "{}: cannot drive the second amplifier's chip select (GPIO connection {})",
                    self.path, g
                ));
                return false;
            }
        }
        // The shared reset line: low for 2 ms, then released. A line the
        // firmware locked stays as it is (as on Linux, whose writes then do
        // nothing): the software reset below still resets them.
        if let Some(r) = config.reset_gpio {
            if board.gpio_write(r as u32, false).is_ok() {
                board.sleep_us(2000);
                let _ = board.gpio_write(r as u32, true);
            } else {
                board.log(&format!("{}: cannot drive the amplifiers' reset line (GPIO connection {})", self.path, r));
            }
        }
        board.sleep_us(2000);
        let speaker_id = config.speaker_id_gpio.and_then(|g| board.gpio_read(g as u32).ok().map(|l| l as u8));
        self.speaker_id = speaker_id;

        let mut up = Vec::new();
        for i in 0..self.units.len() {
            let (select, cs) = (self.units[i].select, self.units[i].chip_select);
            let channel = config.channels[i];
            let mut bus = AmpBus { board: &mut *board, select, mode: self.mode };
            match Amp::probe(&mut bus, channel) {
                Ok(amp) => {
                    up.push(format!(
                        "{} at chip select {}{} (revision {:X}, OTP id {}, {} trims)",
                        channel.name(),
                        cs,
                        if matches!(select, Select::Gpio(_)) { " by GPIO" } else { "" },
                        amp.revision,
                        amp.otp_id,
                        amp.trims
                    ));
                    self.units[i].amp = Some(amp);
                }
                Err(e) => {
                    let _ = amp::safe_reset(&mut bus);
                    board.log(&format!("{}: the {} amplifier (chip select {}): {}", self.path, channel.name(), cs, e));
                }
            }
        }
        if up.is_empty() {
            if let Some(r) = config.reset_gpio {
                let _ = board.gpio_write(r as u32, false);
            }
            board.log(&format!("{}: no amplifier came up; they are held in reset", self.path));
            return false;
        }
        board.log(&format!(
            "{}: board {}, external boost{}: {}",
            self.path,
            config.ssid,
            speaker_id.map(|s| format!(", speaker id {}", s)).unwrap_or_default(),
            up.join(", ")
        ));
        true
    }

    /// Loads into each amplifier brought up the DSP firmware and tuning
    /// Linux would (by board, speaker id and amplifier, from the files the
    /// [`Board`] has), and starts it, paused until playback starts. An
    /// amplifier whose firmware is missing or fails plays without it, as
    /// on Linux. Logs what happened; true if any runs its firmware.
    pub fn start_firmware(&mut self, board: &mut impl Board) -> bool {
        if self.units.iter().all(|u| u.amp.is_none()) {
            return false;
        }
        if self.speed_hz < MIN_FIRMWARE_HZ {
            board.log(&format!(
                "{}: the bus runs at {} kHz, too slow to load the amplifiers' firmware; they play without it",
                self.path,
                self.speed_hz / 1000
            ));
            return false;
        }
        let mode = self.mode;
        let mut files = Files::new();
        let mut running = false;
        for i in 0..self.units.len() {
            let channel = self.config.channels[i];
            let index = (0..i).filter(|&j| self.config.channels[j] == channel).count();
            let candidates = firmware_files(&self.ssid, self.speaker_id, &format!("{}{}", letter(channel), index));
            let (path, select) = (&self.path, self.units[i].select);
            let Some(amp) = &mut self.units[i].amp else { continue };
            let what = format!("{}: the {} amplifier", path, channel.name());
            let (fw_name, tuning_name) = match pick(board, &mut files, &candidates) {
                Ok(names) => names,
                Err(missing) => {
                    board.log(&format!("{}: {}; it plays without its firmware", what, missing));
                    continue;
                }
            };
            // The tuning's parameters for the driver (its gain), if it has
            // any; Linux keeps its default if they are missing or bad.
            let mut gain = DSP_GAIN_PCM;
            let config_name = format!("{}cfg", tuning_name);
            if let Some(data) = file(board, &mut files, &config_name) {
                match wmfw::tuning_gain(data) {
                    Ok(Some(g)) => gain = g,
                    Ok(None) => {}
                    Err(e) => board.log(&format!("{}: {}: {}; at the default gain", what, config_name, e)),
                }
            }
            let data =
                |name: &str| files.iter().find(|(n, _)| n == name).and_then(|(_, d)| d.as_deref()).unwrap_or(&[]);
            let (fw_data, tuning_data) = (data(&fw_name), data(&tuning_name));
            let parsed = wmfw::firmware(fw_data).map_err(|e| format!("{}: {}", fw_name, e)).and_then(|fw| {
                wmfw::coefficients(tuning_data).map(|t| (fw, t)).map_err(|e| format!("{}: {}", tuning_name, e))
            });
            let (fw, tuning) = match parsed {
                Ok(p) => p,
                Err(why) => {
                    board.log(&format!("{}: {}; it plays without its firmware", what, why));
                    continue;
                }
            };
            let mut bus = AmpBus { board: &mut *board, select, mode };
            match amp.start_firmware(&mut bus, &fw, Some(&tuning), gain) {
                Ok(loaded) => {
                    for w in &loaded.warnings {
                        board.log(&format!("{}: {}", what, w));
                    }
                    board.log(&format!(
                        "{}: firmware {} (v{}), tuning {}{}, at {}.5 dB",
                        what,
                        fw_name,
                        version_name(loaded.version),
                        tuning_name,
                        tuning.name.map(|n| format!(" ({})", short_name(n))).unwrap_or_default(),
                        gain & 0x1F
                    ));
                    running = true;
                }
                Err(e) => board.log(&format!("{}: {}; it plays without its firmware", what, e)),
            }
        }
        running
    }

    /// The codec's stream has started (power up), or is about to stop
    /// (power down while it still runs): each step on every amplifier
    /// before the next, as Linux's playback hooks go. Returns what failed.
    pub fn playing(&mut self, board: &mut impl Board, on: bool) -> Vec<String> {
        let (mode, muted) = (self.mode, self.muted);
        let mut problems = Vec::new();
        for step in 0..2 {
            for u in &mut self.units {
                let Some(amp) = &mut u.amp else { continue };
                let mut bus = AmpBus { board: &mut *board, select: u.select, mode };
                let r = match (on, step) {
                    (true, 0) => amp.mute(&mut bus, muted).and_then(|_| amp.play_start(&mut bus)),
                    (true, _) => amp.play_done(&mut bus),
                    (false, 0) => amp.pause_start(&mut bus),
                    (false, _) => amp.pause_done(&mut bus),
                };
                if let Err(e) = r {
                    problems.push(format!(
                        "the {} amplifier (chip select {}): {}",
                        amp.channel.name(),
                        u.chip_select,
                        e
                    ));
                }
            }
        }
        problems
    }

    /// Mutes the speakers (headphones in) or unmutes them.
    pub fn mute(&mut self, board: &mut impl Board, muted: bool) -> bool {
        self.muted = muted;
        let mode = self.mode;
        let mut ok = true;
        for u in &mut self.units {
            if let Some(amp) = &mut u.amp {
                ok &= amp.mute(&mut AmpBus { board: &mut *board, select: u.select, mode }, muted).is_ok();
            }
        }
        ok
    }

    pub fn is_playing(&self) -> bool {
        self.units.iter().any(|u| u.amp.as_ref().is_some_and(|a| a.playing()))
    }

    /// The amplifiers brought up, and the channel each plays.
    pub fn channels(&self) -> Vec<Channel> {
        self.units.iter().filter_map(|u| u.amp.as_ref().map(|a| a.channel)).collect()
    }

    /// Errors the amplifiers latched (each shuts itself down on one), for
    /// the log.
    pub fn errors(&mut self, board: &mut impl Board) -> Vec<String> {
        let mode = self.mode;
        let mut out = Vec::new();
        for u in &mut self.units {
            let Some(amp) = &mut u.amp else { continue };
            if let Ok(errors) = amp.take_errors(&mut AmpBus { board: &mut *board, select: u.select, mode })
                && errors != 0
            {
                out.push(format!("the {} amplifier: {}", amp.channel.name(), amp::error_names(errors).join(", ")));
            }
        }
        out
    }
}
