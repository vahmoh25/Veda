//! The ASUS Zenbook Pro 16X (UX7602ZM, Alder Lake-P): its speakers behind
//! two CS35L41 amplifiers on the chipset's second SPI controller (00:1e.3),
//! their reset line and the second one's chip select on GPIO pins, as its
//! firmware describes them. The AML here has the shape of the laptop's own
//! (its DSDT's `\_SB.PC00`, `SPI1` and `GPI0`, and the `SPKRAMPS` SSDT):
//! `SPI1` exists at its PCI address only as the firmware's settings say,
//! and the GPIO controller's registers are where a firmware variable puts
//! them.
//!
//! The wiring: the controller's chip select 0 selects the left amplifier,
//! GPIO pin 23 (pulled up) the right one; pin 305 (pulled down) resets both;
//! pin 302 is the speakers' id.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::format;
use std::path::Path;
use std::rc::Rc;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use vacpi::Memory;
use vacpi::aml::Namespace;
use vacpi::asm::*;
use vacpi::device::{self, Described};
use vacpi::name::Path as AcpiPath;
use vacpi::resource::Resource;
use vcs35l41::group::{Board, BoardError, SpiMode};
use vgpio::{Layout, PADCFG0_RX_DISABLE, PADCFG0_TX_DISABLE, PADCFG0_TX_STATE, Pads, Registers};
use vspi::regs::{CS_CONTROL, RESETS, SSDR};

use crate::cs35l41::Cs35l41;
use crate::lpss::SspModel;
use crate::pads::PadsModel;

/// The firmware's variables (its NVS memory), where the AML reads them.
const GNVS: u64 = 0x59C7_3000;
/// Where the chipset's sideband registers (and so the GPIO communities)
/// are.
pub const SBREG: u32 = 0xFD00_0000;
/// The SPI controller's register BAR.
pub const SPI_BAR: u64 = 0x6001_1000;
/// The amplifiers' bus speed, as the firmware gives it.
pub const SPI_HZ: u32 = 4_000_000;

pub const PIN_CHIP_SELECT: u16 = 23;
pub const PIN_RESET: u16 = 305;
pub const PIN_SPEAKER_ID: u16 = 302;
pub const PIN_INTERRUPT: u16 = 303;

/// The firmware's tables: a DSDT's worth (the PCI root bridge, the SPI
/// controller, the GPIO controller) and the amplifiers' SSDT.
pub fn firmware() -> (Vec<u8>, Vec<u8>) {
    let gpio_crs = method(
        "_CRS",
        0,
        &cat(&[
            &name(
                "RBFL",
                &buffer(&cat(&[
                    &interrupt(14),
                    &memory32_fixed(0, 0x1_0000),
                    &memory32_fixed(0, 0x1_0000),
                    &memory32_fixed(0, 0x1_0000),
                    &memory32_fixed(0, 0x1_0000),
                    &END_TAG,
                ])),
            ),
            &create_dword_field("RBFL", 0x05, "INTL"),
            &store(&nm("SGIR"), &nm("INTL")),
            &create_dword_field("RBFL", 0x0D, "CML0"),
            &store(&add(&nm("SBRG"), &int(0x006E_0000)), &nm("CML0")),
            &create_dword_field("RBFL", 0x19, "CML1"),
            &store(&add(&nm("SBRG"), &int(0x006D_0000)), &nm("CML1")),
            &create_dword_field("RBFL", 0x25, "CML4"),
            &store(&add(&nm("SBRG"), &int(0x006A_0000)), &nm("CML4")),
            &create_dword_field("RBFL", 0x31, "CML5"),
            &store(&add(&nm("SBRG"), &int(0x0069_0000)), &nm("CML5")),
            &ret(&nm("RBFL")),
        ]),
    );
    let gpio = device(
        "GPI0",
        &cat(&[
            &method(
                "_HID",
                0,
                &cat(&[&if_(&lequal(&nm("GPHD"), &int(1)), &ret(&string("PNP0C02"))), &ret(&string("INTC1055"))]),
            ),
            &name("LINK", &string("\\_SB.GPI0")),
            &gpio_crs,
            &method("_STA", 0, &cat(&[&if_(&lequal(&nm("GPHD"), &int(1)), &ret(&int(8))), &ret(&int(0x0F))])),
        ]),
    );
    let spi = device(
        "SPI1",
        &cat(&[
            &if_(
                &lequal(&nm("SM01"), &int(2)),
                &cat(&[
                    &method("_CRS", 0, &ret(&buffer(&cat(&[&memory32_fixed(0xFE03_0000, 0x1000), &END_TAG])))),
                    &name("_STA", &int(8)),
                ]),
            ),
            &if_(
                &lor(&lequal(&nm("SM01"), &int(1)), &lequal(&nm("SM01"), &int(0))),
                &method("_ADR", 0, &ret(&int(0x001E_0003))),
            ),
        ]),
    );
    let root = device(
        "PC00",
        &cat(&[
            &name("_HID", &eisa("PNP0A08")),
            &name("_CID", &eisa("PNP0A03")),
            &name("_ADR", &int(0)),
            &method("BN00", 0, &ret(&int(0))),
            &method("_BBN", 0, &ret(&nm("BN00"))),
            &name("_UID", &int(0)),
            &spi,
            &device("HDAS", &name("_ADR", &int(0x001F_0003))),
        ]),
    );
    let dsdt = cat(&[
        &region("GNVS", 0, &int(GNVS), &int(0x10)),
        &field("GNVS", 0x10, &[("SM01", 8), ("GPHD", 8), ("SGIR", 8), ("", 8), ("SBRG", 32)]),
        &scope("\\_SB", &cat(&[&root, &gpio])),
    ]);
    let speakers = method(
        "_CRS",
        0,
        &cat(&[
            &name(
                "SBUF",
                &buffer(&cat(&[
                    &spi_descriptor(0, SPI_HZ, "\\_SB.PC00.SPI1"),
                    &spi_descriptor(1, SPI_HZ, "\\_SB.PC00.SPI1"),
                    &gpio_descriptor(false, 2, 1, 0, PIN_CHIP_SELECT, "\\_SB.GPI0"),
                    &gpio_descriptor(false, 2, 2, 0, PIN_RESET, "\\_SB.GPI0"),
                    &gpio_descriptor(false, 1, 1, 0, PIN_SPEAKER_ID, "\\_SB.GPI0"),
                    &gpio_descriptor(false, 9, 1, 100, PIN_INTERRUPT, "\\_SB.GPI0"),
                    &gpio_descriptor(true, 0x0D, 1, 100, PIN_INTERRUPT, "\\_SB.GPI0"),
                    &END_TAG,
                ])),
            ),
            &ret(&nm("SBUF")),
        ]),
    );
    let ssdt = cat(&[
        &if_(&[0x00], &cat(&[&external("\\_SB.GPI0", 6, 0), &external("\\_SB.PC00.SPI1", 6, 0)])),
        &scope(
            "\\_SB.PC00.SPI1",
            &device(
                "SPK1",
                &cat(&[
                    &name("_HID", &string("CSC3551")),
                    &name("_SUB", &string("10431F62")),
                    &name("_UID", &int(1)),
                    &speakers,
                    &method("_STA", 0, &ret(&int(0x0F))),
                    &method("_DIS", 0, &[]),
                ]),
            ),
        ),
    ]);
    (dsdt, ssdt)
}

/// The firmware's settings: the second SPI controller on PCI (`SM01`
/// 1), the GPIO controller shown (`GPHD` 0), its interrupt, and where the
/// sideband registers are (`SBRG`).
pub struct Settings(pub [u8; 16]);

impl Default for Settings {
    fn default() -> Settings {
        let mut s = [0u8; 16];
        s[0] = 1;
        s[2] = 14;
        s[4..8].copy_from_slice(&SBREG.to_le_bytes());
        Settings(s)
    }
}

impl Memory for Settings {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        let Some(at) = address.checked_sub(GNVS).map(|a| a as usize) else { return false };
        match self.0.get(at..at + buf.len()) {
            Some(b) => {
                buf.copy_from_slice(b);
                true
            }
            None => false,
        }
    }
}

/// The chips and their wires.
pub struct World {
    /// The board's clock, in nanoseconds.
    pub now_ns: u64,
    /// The codec clocks the amplifiers' audio port (its stream runs).
    pub i2s_clock: bool,
    /// Left (chip select 0), right (the GPIO).
    pub amps: [Cs35l41; 2],
    pub pads: PadsModel,
    pub ssp: SspModel,
    /// What the wiring saw go wrong: two devices selected at once, bytes
    /// to nobody, a bus driven faster than the firmware allows.
    pub problems: Vec<String>,
}

impl World {
    /// Notes a problem (each once).
    fn note(&mut self, problem: String) {
        if !self.problems.contains(&problem) {
            self.problems.push(problem);
        }
    }

    fn now_us(&self) -> u64 {
        self.now_ns / 1000
    }

    /// The lines from the pads and the controller to the amplifiers.
    fn rewire(&mut self) {
        let now = self.now_us();
        let released = self.pads.output(PIN_RESET).unwrap_or(false);
        let right = !self.pads.output(PIN_CHIP_SELECT).unwrap_or(true);
        let left = self.ssp.asserted() == Some(0);
        for (a, selected) in self.amps.iter_mut().zip([left, right]) {
            a.set_reset(released, now);
            a.select(selected);
        }
    }

    fn ssp_write(&mut self, offset: usize, value: u32) {
        if let Some(s) = self.ssp.write(offset, value) {
            if s.rate_hz > SPI_HZ {
                self.note(format!("a byte at {} Hz, faster than the firmware's {} Hz", s.rate_hz, SPI_HZ));
            }
            if s.bits != 8 || s.cpol || s.cpha {
                self.note(format!("a {}-bit word in mode {}", s.bits, (s.cpol as u8) << 1 | s.cpha as u8));
            }
            self.now_ns += 8 * 1_000_000_000 / s.rate_hz.max(1) as u64;
            let (now, clock) = (self.now_us(), self.i2s_clock);
            let selected: Vec<usize> = (0..2).filter(|&i| self.amps[i].is_selected()).collect();
            let out = match selected.as_slice() {
                [] => {
                    self.note("a byte with no device selected".into());
                    0xFF
                }
                [i] => self.amps[*i].shift(s.byte, now, clock),
                _ => {
                    self.note("both amplifiers selected at once".into());
                    self.amps[0].shift(s.byte, now, clock) & self.amps[1].shift(s.byte, now, clock)
                }
            };
            self.ssp.receive(out);
        } else if offset == SSDR {
            self.note("a byte written with the port off".into());
        }
        if offset == CS_CONTROL || offset == RESETS {
            self.rewire();
        }
    }

    /// Everything that went wrong, on the board and in the amplifiers.
    pub fn all_problems(&self) -> Vec<String> {
        let mut p = self.problems.clone();
        for a in &self.amps {
            p.extend(a.problems.iter().cloned());
            p.extend(a.dsp.problems.iter().cloned());
        }
        p
    }
}

pub type Shared = Rc<RefCell<World>>;

/// The SPI controller's registers, as `vspi` reaches them.
pub struct SspPort(pub Shared);

impl vspi::Hardware for SspPort {
    fn read(&self, offset: usize) -> u32 {
        self.0.borrow_mut().ssp.read(offset)
    }

    fn write(&self, offset: usize, value: u32) {
        self.0.borrow_mut().ssp_write(offset, value);
    }

    fn delay_ns(&self, ns: u64) {
        self.0.borrow_mut().now_ns += ns;
    }
}

/// A GPIO community's registers, as `vgpio` reaches them.
pub struct Community(pub Shared, pub usize);

impl Registers for Community {
    fn read(&self, offset: usize) -> u32 {
        self.0.borrow().pads.read(self.1, offset)
    }

    fn write(&self, offset: usize, value: u32) {
        let mut w = self.0.borrow_mut();
        w.pads.write(self.1, offset, value);
        w.rewire();
    }

    fn len(&self) -> usize {
        0x1_0000
    }
}

/// The laptop: its firmware, settings and chips.
pub struct Zenbook {
    pub world: Shared,
    pub ns: Namespace,
    pub settings: Settings,
}

impl Zenbook {
    /// The laptop as its firmware leaves it (after Windows, say): the
    /// amplifiers out of reset and booted, the chip select pin a GPIO
    /// driven high, the speaker id pin an input.
    pub fn new() -> Zenbook {
        Zenbook::with(Settings::default(), |_| {})
    }

    /// The laptop with other settings, and changes to its chips before it
    /// starts (a locked pin, a missing amplifier).
    pub fn with(settings: Settings, change: impl FnOnce(&mut World)) -> Zenbook {
        let layout = vgpio::layout("INTC1055").expect("Tiger Lake-LP's pads");
        let mut pads = PadsModel::new(layout);
        pads.set_config(PIN_CHIP_SELECT, PADCFG0_RX_DISABLE | PADCFG0_TX_STATE);
        pads.set_config(PIN_RESET, PADCFG0_RX_DISABLE | PADCFG0_TX_STATE);
        pads.set_config(PIN_SPEAKER_ID, PADCFG0_TX_DISABLE);
        pads.set_config(PIN_INTERRUPT, PADCFG0_TX_DISABLE);
        pads.hold(PIN_SPEAKER_ID, false);
        pads.hold(PIN_INTERRUPT, true);
        let mut world = World {
            now_ns: 0,
            i2s_clock: false,
            amps: [Cs35l41::new("left"), Cs35l41::new("right")],
            pads,
            ssp: SspModel::new(1, 100_000_000),
            problems: Vec::new(),
        };
        change(&mut world);
        world.rewire();
        // Long enough for the amplifiers to have booted.
        world.now_ns = 10_000_000;
        let (dsdt, ssdt) = firmware();
        let mut ns = Namespace::new();
        ns.load(&dsdt, &settings).expect("the DSDT loads");
        ns.load(&ssdt, &settings).expect("the SSDT loads");
        Zenbook { world: Rc::new(RefCell::new(world)), ns, settings }
    }
}

impl Default for Zenbook {
    fn default() -> Zenbook {
        Zenbook::new()
    }
}

/// The firmware files Veda ships for the amplifiers (`assets/firmware`,
/// installed at `/system/firmware`), by their names in the Linux firmware
/// collection.
pub fn shipped_firmware() -> BTreeMap<String, Vec<u8>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/firmware");
    let mut files = BTreeMap::new();
    let cirrus = std::fs::read_dir(dir.join("cirrus")).expect("assets/firmware/cirrus");
    for entry in cirrus.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".wmfw") || name.ends_with(".bin") {
            files.insert(format!("cirrus/{}", name), std::fs::read(entry.path()).expect("a firmware file"));
        }
    }
    files
}

/// The amplifiers' board as Veda's drivers reach it: devmgr's part (the
/// firmware's description of the SPI controller and what is below it, the
/// GPIO controller and its pads) and `lpss-spi`'s (the SPI controller, the
/// firmware files).
pub struct Drivers<'a> {
    pub machine: &'a Zenbook,
    /// The amplifiers' device, as devmgr describes it to the driver.
    pub device: Described,
    pub spi: vspi::Controller<SspPort>,
    gpio: (AcpiPath, &'static Layout, Vec<Community>),
    /// The firmware files the system has (those Veda ships, unless a test
    /// changes them).
    pub files: BTreeMap<String, Vec<u8>>,
    pub log: Vec<String>,
}

impl<'a> Drivers<'a> {
    /// What devmgr and `lpss-spi` do before bringing the amplifiers up: find
    /// the SPI controller's ACPI device (PCI 00:1e.3) and the devices below
    /// it, open the GPIO controller they use (its id and registers from
    /// its `_CRS`), and start the SPI controller.
    pub fn start(machine: &'a Zenbook) -> Result<Drivers<'a>, String> {
        let (ns, settings) = (&machine.ns, &machine.settings);
        let roots = device::pci_roots(ns, settings);
        let companion =
            device::pci_companion(ns, &roots, (0, 0x1E, 3), settings).ok_or("no ACPI device for 00:1e.3")?;
        let below = device::describe_children(ns, &companion, settings);
        let device = below.into_iter().find(|d| d.identity.is("CSC3551")).ok_or("no amplifiers below 00:1e.3")?;
        // The GPIO controller of its first GPIO connection.
        let (controller, _) = device.gpio(ns, 0).ok_or("no GPIO connection")?;
        let hid = device::identify(ns, &controller, settings).hid.unwrap_or_default();
        let layout = vgpio::layout(&hid).ok_or_else(|| format!("a GPIO controller {}", hid))?;
        let windows: Vec<u64> = device::resources(ns, &controller, settings)
            .map_err(|e| e.to_string())?
            .iter()
            .filter_map(|r| match r {
                Resource::Memory { base, .. } => Some(*base),
                _ => None,
            })
            .collect();
        // The communities, by the addresses the firmware gave.
        let expected: Vec<u64> = [0x6E, 0x6D, 0x6A, 0x69].iter().map(|p| SBREG as u64 + (p << 16)).collect();
        if windows != expected {
            return Err(format!("GPIO windows at {:x?}, not {:x?}", windows, expected));
        }
        let communities = (0..windows.len()).map(|i| Community(machine.world.clone(), i)).collect();
        let spi = vspi::Controller::start(SspPort(machine.world.clone()), 100_000_000, SPI_BAR)?;
        let gpio = (controller, layout, communities);
        Ok(Drivers { machine, device, spi, gpio, files: shipped_firmware(), log: Vec::new() })
    }

    fn pads(&self) -> Pads<'_, Community> {
        Pads::new(self.gpio.1, &self.gpio.2)
    }

    /// The pin of GPIO connection `index`, as devmgr finds it.
    fn pin(&self, index: u32) -> Result<u16, BoardError> {
        let (controller, pin) = self.device.gpio(&self.machine.ns, index as usize).ok_or(BoardError)?;
        if controller != self.gpio.0 {
            return Err(BoardError);
        }
        Ok(pin)
    }
}

impl Board for Drivers<'_> {
    fn transfer(&mut self, native_cs: Option<u8>, mode: SpiMode, data: &mut [u8]) -> Result<(), BoardError> {
        let mode = vspi::Mode { cpol: mode.cpol, cpha: mode.cpha };
        self.spi.transfer(native_cs, mode, data).map_err(|_| BoardError)
    }

    fn gpio_write(&mut self, index: u32, high: bool) -> Result<(), BoardError> {
        let pin = self.pin(index)?;
        self.pads().write(pin, high).map(|_| ()).map_err(|_| BoardError)
    }

    fn gpio_read(&mut self, index: u32) -> Result<bool, BoardError> {
        let pin = self.pin(index)?;
        self.pads().read(pin).map(|(level, _)| level).map_err(|_| BoardError)
    }

    fn sleep_us(&mut self, us: u64) {
        self.machine.world.borrow_mut().now_ns += us * 1000;
    }

    fn log(&mut self, line: &str) {
        self.log.push(line.to_string());
    }

    fn native_chip_selects(&self) -> u32 {
        self.spi.chip_selects
    }

    fn firmware(&mut self, name: &str) -> Option<Vec<u8>> {
        self.files.get(name).cloned()
    }
}

/// The amplifiers' SPI connections, as `lpss-spi` takes them from the
/// device's resources.
pub fn spi_links(d: &Described) -> Vec<vcs35l41::group::SpiLink> {
    d.resources
        .as_ref()
        .map(|r| {
            r.iter()
                .filter_map(|r| match r {
                    Resource::Spi(s) if !s.cs_active_high => Some(vcs35l41::group::SpiLink {
                        chip_select: s.chip_select,
                        speed_hz: s.speed_hz,
                        mode: SpiMode { cpol: s.cpol, cpha: s.cpha },
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_else(|_| vec![])
}
