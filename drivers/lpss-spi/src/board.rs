//! The board around the amplifiers as this driver reaches it: the SPI
//! controller's registers, mapped from its BAR, the GPIO pins devmgr
//! drives on its behalf, and the amplifiers' firmware files.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use vabi::map_flags;
use vcs35l41::group::{Board, BoardError, SpiMode};
use vproto::pci::{self, DeviceInfo, pcidev};
use vproto::vfs;
use vrt::println;
use vrt::time::{Duration, now_ns};
use vrt::vm::Mapping;

/// Where the firmware files are installed (from `assets/firmware`), by
/// their names in the Linux firmware collection.
const FIRMWARE_DIR: &str = "/system/firmware/";

/// The controller's registers.
pub struct Mmio {
    _map: Mapping,
    base: *mut u8,
    len: usize,
}

impl vspi::Hardware for Mmio {
    fn read(&self, offset: usize) -> u32 {
        if offset.is_multiple_of(4) && offset + 4 <= self.len {
            // SAFETY: an aligned register inside the mapped BAR.
            unsafe { read_volatile(self.base.add(offset) as *const u32) }
        } else {
            u32::MAX
        }
    }

    fn write(&self, offset: usize, value: u32) {
        if offset.is_multiple_of(4) && offset + 4 <= self.len {
            // SAFETY: as above.
            unsafe { write_volatile(self.base.add(offset) as *mut u32, value) }
        }
    }

    fn delay_ns(&self, ns: u64) {
        let end = now_ns() + ns;
        while now_ns() < end {
            core::hint::spin_loop();
        }
    }
}

pub struct LpssBoard<'a> {
    pub spi: vspi::Controller<Mmio>,
    pci: &'a pcidev::Client,
    /// The amplifiers' device: its index among those devmgr described.
    device: u32,
    /// The file service, once a firmware file is wanted.
    vfs: Option<vfs::Client>,
}

impl<'a> LpssBoard<'a> {
    /// Powers the controller up (D0, if the firmware left it asleep),
    /// enables it and maps its registers, then sets it up (`vspi`).
    pub fn start(
        pci: &'a pcidev::Client,
        info: &DeviceInfo,
        input_hz: u32,
        device: u32,
    ) -> Result<LpssBoard<'a>, &'static str> {
        let bar = info.bars.iter().find(|b| b.index == 0 && !b.io).ok_or("no register BAR")?;
        if bar.size < 0x300 {
            return Err("the register BAR is too small");
        }
        if let Some(pm) = pci::find_capability(pci, 0x01) {
            let control = pci.config_read(pm + 4, 2).ok().and_then(|r| r.ok()).unwrap_or(0);
            if control & 3 != 0 {
                println!("the controller was in power state D{}; waking it", control & 3);
                let _ = pci.config_write(pm + 4, 2, control & !0x8003);
                vrt::time::sleep(Duration::from_millis(10));
                let address = pci.config_read(0x10, 4).ok().and_then(|r| r.ok()).unwrap_or(0);
                if address as u64 & !0xF != bar.address & 0xFFFF_FFF0 {
                    return Err("the controller lost its address when it woke");
                }
            }
        }
        if !matches!(pci.enable(false), Ok(Ok(()))) {
            return Err("cannot enable the device");
        }
        let Ok(Ok(vmo)) = pci.map_bar(0) else { return Err("cannot map the registers") };
        let len = vmo.size().map_err(|_| "cannot map the registers")?;
        let map = Mapping::new(vmo, len, map_flags::READ | map_flags::WRITE).map_err(|_| "cannot map the registers")?;
        let mmio = Mmio { base: map.as_ptr(), _map: map, len };
        let spi = vspi::Controller::start(mmio, input_hz, bar.address)?;
        Ok(LpssBoard { spi, pci, device, vfs: None })
    }
}

impl Board for LpssBoard<'_> {
    fn transfer(&mut self, native_cs: Option<u8>, mode: SpiMode, data: &mut [u8]) -> Result<(), BoardError> {
        let mode = vspi::Mode { cpol: mode.cpol, cpha: mode.cpha };
        self.spi.transfer(native_cs, mode, data).map_err(|_| BoardError)
    }

    fn gpio_write(&mut self, index: u32, high: bool) -> Result<(), BoardError> {
        match self.pci.gpio_write(self.device, index, high) {
            Ok(Ok(())) => Ok(()),
            _ => Err(BoardError),
        }
    }

    fn gpio_read(&mut self, index: u32) -> Result<bool, BoardError> {
        match self.pci.gpio_read(self.device, index) {
            Ok(Ok(level)) => Ok(level),
            _ => Err(BoardError),
        }
    }

    fn sleep_us(&mut self, us: u64) {
        vrt::time::sleep(Duration::from_micros(us));
    }

    fn log(&mut self, line: &str) {
        println!("{}", line);
    }

    fn native_chip_selects(&self) -> u32 {
        self.spi.chip_selects
    }

    fn firmware(&mut self, name: &str) -> Option<Vec<u8>> {
        if self.vfs.is_none() {
            self.vfs = vproto::connect(vfs::NAME).ok().map(vfs::Client::new);
        }
        let (vmo, len) = self.vfs.as_ref()?.read_file(format!("{}{}", FIRMWARE_DIR, name)).ok()?.ok()?;
        let mut data = vec![0u8; len as usize];
        vmo.read(0, &mut data).ok()?;
        Some(data)
    }
}
