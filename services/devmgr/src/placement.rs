//! Placing the BARs the firmware leaves unplaced. A firmware may assign no
//! address to a function's memory BAR (a laptop's leaves its serial bus
//! controllers so, asleep, for the operating system to set up), and an OS
//! places it then, as Linux does on a PC. devmgr does it before any driver
//! or the driver VM gets the function: in a memory window of the PCI root
//! bridge's (its ACPI `_CRS`), where nothing else is (no other function's
//! BAR, no bridge's window, nothing the firmware reserves for the
//! motherboard), naturally aligned; a 64-bit BAR above 4 GiB where it can
//! be, so as to leave the scarce space below to those that must be there.
//! From the top of the window down: the firmware fills windows from the
//! bottom, and what devmgr cannot see is there too (the BARs of a GPU's
//! virtual functions, in extended configuration space, which devmgr does
//! not reach). Functions on a root bus only: one behind a bridge would need
//! the bridge's window to grow. A BAR that stays unplaced is given to no
//! driver.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ops::Range;

use vrt::println;

use crate::acpi::Acpi;
use crate::pci::{Address, ConfigSpace};

const FOUR_GIB: u64 = 1 << 32;

/// Places the unplaced memory BARs of `functions`, in the windows of their
/// root bridges as `acpi` describes them.
pub fn place(config: &ConfigSpace, functions: &[Address], acpi: Option<&Acpi>) {
    let mut taken: Vec<Range<u64>> = Vec::new();
    let mut unplaced = Vec::new();
    for &a in functions {
        if config.is_bridge(a) {
            taken.extend(config.bridge_windows(a));
            continue;
        }
        for bar in config.memory_bars(a) {
            if bar.address == 0 {
                unplaced.push((a, bar));
            } else {
                taken.push(bar.address..bar.address + bar.size);
            }
        }
    }
    if unplaced.is_empty() {
        return;
    }
    let Some(acpi) = acpi else {
        println!("pci: {} BAR(s) the firmware left unplaced, and no ACPI tables to place them by", unplaced.len());
        return;
    };
    taken.extend(acpi.motherboard_memory());
    // The largest first: their alignment is the hardest to find.
    unplaced.sort_by_key(|(_, bar)| core::cmp::Reverse(bar.size));
    let mut buses: BTreeMap<u8, Vec<Range<u64>>> = BTreeMap::new();
    for (a, bar) in unplaced {
        let windows = buses.entry(a.bus).or_insert_with(|| acpi.pci_root_windows(a.bus));
        if windows.is_empty() {
            println!(
                "pci {:02x}:{:02x}.{}: BAR {} unplaced by the firmware, and no root bridge's window for it: left out",
                a.bus, a.slot, a.function, bar.index
            );
            continue;
        }
        let high = windows.iter().filter(|w| w.start >= FOUR_GIB);
        let low = windows.iter().filter(|w| w.end <= FOUR_GIB);
        let place = if bar.is64 {
            high.chain(low).find_map(|w| free(w, bar.size, &taken))
        } else {
            low.clone().find_map(|w| free(w, bar.size, &taken))
        };
        match place {
            Some(at) => {
                config.set_bar(a, bar.index, at, bar.is64);
                taken.push(at..at + bar.size);
                println!(
                    "pci {:02x}:{:02x}.{}: BAR {} ({} KiB) unplaced by the firmware: placed at {:#x}",
                    a.bus,
                    a.slot,
                    a.function,
                    bar.index,
                    bar.size / 1024,
                    at
                );
            }
            None => println!(
                "pci {:02x}:{:02x}.{}: BAR {} ({} KiB) unplaced by the firmware, and no room for it: left out",
                a.bus,
                a.slot,
                a.function,
                bar.index,
                bar.size / 1024
            ),
        }
    }
}

/// The highest address in `window` where `size` bytes (a power of two),
/// aligned to `size`, meet nothing `taken`.
fn free(window: &Range<u64>, size: u64, taken: &[Range<u64>]) -> Option<u64> {
    let mut end = window.end;
    loop {
        let at = end.checked_sub(size)? & !(size - 1);
        if at < window.start {
            return None;
        }
        match taken.iter().filter(|t| t.start < at + size && at < t.end).map(|t| t.start).min() {
            Some(below) => end = below,
            None => return Some(at),
        }
    }
}
