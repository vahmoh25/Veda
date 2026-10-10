//! Intel's integrated GPUs as the guest has them: what their driver reads
//! of the PC's firmware besides the GPU itself.
//!
//! The OpRegion: memory the firmware fills for the GPU's driver, which the
//! GPU's configuration space names (ASLS): the display's description (its
//! VBT: the panel, the ports) and mailboxes through which the firmware's
//! ACPI and the driver talk. The guest has a copy of it at
//! [`OPREGION_AT`], with its raw VBT right after it if the firmware keeps
//! that outside (from version 2.0, mailbox 3's RVDA and RVDS); without the
//! mailbox through which the driver would call the PC's firmware (SWSCI,
//! which raises an SMI there).

use alloc::vec::Vec;

/// Where the guest's copy of the OpRegion is: in the legacy area below
/// 1 MiB, which the guest's memory map reserves, before its ACPI tables.
pub const OPREGION_AT: u64 = 0xC_0000;
/// The room it has there.
pub const OPREGION_ROOM: usize = 0x2_0000;
/// An OpRegion's size, its mailboxes included.
pub const OPREGION_SIZE: usize = 0x2000;

// The copy ends where the ACPI tables begin.
const _: () = assert!(OPREGION_AT + OPREGION_ROOM as u64 <= crate::acpi::AT);

const SIGNATURE: &[u8; 16] = b"IntelGraphicsMem";
/// The header's version (its minor and major numbers), and its mailboxes.
const MINOR: usize = 0x16;
const MAJOR: usize = 0x17;
const MBOXES: usize = 0x58;
const MBOX_SWSCI: u32 = 1 << 1;
const MBOX_ASLE: u32 = 1 << 2;
/// Mailbox 3 (ASLE)'s raw VBT: its address (RVDA) and size (RVDS).
const RVDA: usize = 0x3BA;
const RVDS: usize = 0x3C2;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u32_at(b, at) as u64 | (u32_at(b, at + 4) as u64) << 32
}

/// Whether `bytes` start as an OpRegion does.
pub fn is_opregion(bytes: &[u8]) -> bool {
    bytes.len() >= OPREGION_SIZE && bytes[..SIGNATURE.len()] == *SIGNATURE
}

/// Whether its raw VBT's address is relative to it (version 2.1 on), not
/// physical (2.0).
fn relative(opregion: &[u8]) -> bool {
    opregion[MAJOR] > 2 || opregion[MINOR] >= 1
}

/// Where the raw VBT of `opregion` (an OpRegion the PC has at `at`) is, if
/// it keeps it outside itself: its address and size.
pub fn raw_vbt(opregion: &[u8], at: u64) -> Option<(u64, usize)> {
    if !is_opregion(opregion) || opregion[MAJOR] < 2 || u32_at(opregion, MBOXES) & MBOX_ASLE == 0 {
        return None;
    }
    let (rvda, rvds) = (u64_at(opregion, RVDA), u32_at(opregion, RVDS));
    if rvda == 0 || rvds == 0 {
        return None;
    }
    let address = if relative(opregion) { at.checked_add(rvda)? } else { rvda };
    Some((address, rvds as usize))
}

/// The guest's copy of `opregion` (an OpRegion), at `at` in the guest: its
/// raw VBT `vbt` right after it (none: the VBT in its mailbox 4, as the
/// firmware gave no other or it could not be read), and no SWSCI. `None`
/// if `opregion` is no OpRegion, or the copy needs more than
/// [`OPREGION_ROOM`].
pub fn for_guest(opregion: &[u8], vbt: Option<&[u8]>, at: u64) -> Option<Vec<u8>> {
    if !is_opregion(opregion) {
        return None;
    }
    let mut copy = opregion[..OPREGION_SIZE].to_vec();
    let mboxes = u32_at(&copy, MBOXES) & !MBOX_SWSCI;
    copy[MBOXES..MBOXES + 4].copy_from_slice(&mboxes.to_le_bytes());
    if mboxes & MBOX_ASLE != 0 && copy[MAJOR] >= 2 {
        let vbt = vbt.filter(|v| !v.is_empty());
        let rvda = match vbt {
            Some(_) if relative(&copy) => OPREGION_SIZE as u64,
            Some(_) => at + OPREGION_SIZE as u64,
            None => 0,
        };
        let rvds = u32::try_from(vbt.map_or(0, <[u8]>::len)).ok()?;
        copy[RVDA..RVDA + 8].copy_from_slice(&rvda.to_le_bytes());
        copy[RVDS..RVDS + 4].copy_from_slice(&rvds.to_le_bytes());
        copy.extend_from_slice(vbt.unwrap_or_default());
    }
    (copy.len() <= OPREGION_ROOM).then_some(copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An OpRegion of version `major.minor` with the ACPI, SWSCI and ASLE
    /// mailboxes, its raw VBT at `rvda` (`rvds` bytes).
    fn opregion(major: u8, minor: u8, rvda: u64, rvds: u32) -> Vec<u8> {
        let mut o = alloc::vec![0u8; OPREGION_SIZE];
        o[..16].copy_from_slice(SIGNATURE);
        o[MINOR] = minor;
        o[MAJOR] = major;
        o[MBOXES..MBOXES + 4].copy_from_slice(&(1 | MBOX_SWSCI | MBOX_ASLE).to_le_bytes());
        o[RVDA..RVDA + 8].copy_from_slice(&rvda.to_le_bytes());
        o[RVDS..RVDS + 4].copy_from_slice(&rvds.to_le_bytes());
        o
    }

    #[test]
    fn the_raw_vbt_is_where_the_version_says() {
        // 2.1 on: relative to the OpRegion; 2.0: physical.
        assert_eq!(raw_vbt(&opregion(2, 1, 0x2000, 0x1800), 0x5000_0000), Some((0x5000_2000, 0x1800)));
        assert_eq!(raw_vbt(&opregion(3, 0, 0x2000, 0x1800), 0x5000_0000), Some((0x5000_2000, 0x1800)));
        assert_eq!(raw_vbt(&opregion(2, 0, 0x6000_0000, 0x1800), 0x5000_0000), Some((0x6000_0000, 0x1800)));
        // None outside: in mailbox 4.
        assert_eq!(raw_vbt(&opregion(2, 1, 0, 0), 0x5000_0000), None);
        assert_eq!(raw_vbt(&opregion(1, 0, 0x2000, 0x1800), 0x5000_0000), None);
        let mut not = opregion(2, 1, 0x2000, 0x1800);
        not[0] = b'X';
        assert_eq!(raw_vbt(&not, 0x5000_0000), None);
    }

    #[test]
    fn the_guests_copy_has_its_vbt_after_it_and_no_swsci() {
        let vbt = [0x24u8; 0x1800];
        let copy = for_guest(&opregion(2, 1, 0x2000, 0x1800), Some(&vbt), OPREGION_AT).unwrap();
        assert_eq!(copy.len(), OPREGION_SIZE + vbt.len());
        assert_eq!(u32_at(&copy, MBOXES) & MBOX_SWSCI, 0);
        assert_eq!(raw_vbt(&copy, OPREGION_AT), Some((OPREGION_AT + 0x2000, 0x1800)));
        assert_eq!(&copy[OPREGION_SIZE..], &vbt[..]);
        // 2.0's address is the guest's.
        let copy = for_guest(&opregion(2, 0, 0x6000_0000, 0x1800), Some(&vbt), OPREGION_AT).unwrap();
        assert_eq!(raw_vbt(&copy, OPREGION_AT), Some((OPREGION_AT + 0x2000, 0x1800)));
        // Without it, the driver reads mailbox 4's.
        let copy = for_guest(&opregion(2, 1, 0x2000, 0x1800), None, OPREGION_AT).unwrap();
        assert_eq!((copy.len(), raw_vbt(&copy, OPREGION_AT)), (OPREGION_SIZE, None));
        // Too big, or no OpRegion.
        assert!(for_guest(&opregion(2, 1, 0x2000, 0), Some(&[0; OPREGION_ROOM]), OPREGION_AT).is_none());
        assert!(for_guest(&[0; OPREGION_SIZE], None, OPREGION_AT).is_none());
    }
}
