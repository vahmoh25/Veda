//! GUID Partition Table disk image writer.

use super::fat32::SECTOR;

/// Type GUID of an EFI System Partition.
const ESP_TYPE_GUID: [u8; 16] = guid(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);

const ENTRY_COUNT: usize = 128;
const ENTRY_SIZE: usize = 128;
const ENTRY_SECTORS: u64 = (ENTRY_COUNT * ENTRY_SIZE / SECTOR) as u64; // 32

/// Encodes a GUID in its on-disk mixed-endian form.
const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

/// A partition to place on the disk.
pub struct Partition<'a> {
    pub name: &'a str,
    pub type_guid: [u8; 16],
    /// Volume contents; the partition is exactly this many bytes.
    pub data: &'a [u8],
}

impl<'a> Partition<'a> {
    pub fn esp(data: &'a [u8]) -> Self {
        Partition { name: "EFI System", type_guid: ESP_TYPE_GUID, data }
    }
}

/// First LBA of the first partition (1 MiB alignment).
pub const FIRST_PARTITION_LBA: u64 = 2048;

/// Builds a complete GPT disk containing `parts`, laid out back to back on
/// 1 MiB boundaries.
pub fn build_disk(parts: &[Partition]) -> Vec<u8> {
    let align = |lba: u64| lba.div_ceil(2048) * 2048;
    let mut lba = FIRST_PARTITION_LBA;
    let mut ranges = Vec::new();
    for p in parts {
        assert_eq!(p.data.len() % SECTOR, 0, "partition data must be whole sectors");
        let sectors = (p.data.len() / SECTOR) as u64;
        ranges.push((lba, lba + sectors - 1));
        lba = align(lba + sectors);
    }
    let total_sectors = lba + 2048; // room for the backup GPT, 1 MiB aligned
    let mut disk = vec![0u8; total_sectors as usize * SECTOR];

    for (p, &(start, _)) in parts.iter().zip(&ranges) {
        let off = start as usize * SECTOR;
        disk[off..off + p.data.len()].copy_from_slice(p.data);
    }

    // Protective MBR.
    let mbr = &mut disk[0..SECTOR];
    let pe = &mut mbr[446..462];
    pe[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    pe[4] = 0xEE;
    pe[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    pe[8..12].copy_from_slice(&1u32.to_le_bytes());
    pe[12..16].copy_from_slice(&((total_sectors - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    // Partition entry array.
    let mut entries = vec![0u8; ENTRY_COUNT * ENTRY_SIZE];
    for (i, (p, &(start, end))) in parts.iter().zip(&ranges).enumerate() {
        let e = &mut entries[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE];
        e[0..16].copy_from_slice(&p.type_guid);
        e[16..32].copy_from_slice(&pseudo_guid(0x7061_7274 + i as u64));
        e[32..40].copy_from_slice(&start.to_le_bytes());
        e[40..48].copy_from_slice(&end.to_le_bytes());
        for (j, ch) in p.name.encode_utf16().take(36).enumerate() {
            e[56 + j * 2..58 + j * 2].copy_from_slice(&ch.to_le_bytes());
        }
    }
    let entries_crc = crc32(&entries);
    let last_lba = total_sectors - 1;
    let backup_entries_lba = last_lba - ENTRY_SECTORS;

    let header = |my_lba: u64, alt_lba: u64, entries_lba: u64| -> [u8; SECTOR] {
        let mut h = [0u8; SECTOR];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&(2 + ENTRY_SECTORS).to_le_bytes());
        h[48..56].copy_from_slice(&(backup_entries_lba - 1).to_le_bytes());
        h[56..72].copy_from_slice(&pseudo_guid(0x6469_736b));
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(ENTRY_COUNT as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&h[0..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };

    let primary = header(1, last_lba, 2);
    let backup = header(last_lba, 1, backup_entries_lba);
    disk[SECTOR..2 * SECTOR].copy_from_slice(&primary);
    disk[2 * SECTOR..2 * SECTOR + entries.len()].copy_from_slice(&entries);
    let be = backup_entries_lba as usize * SECTOR;
    disk[be..be + entries.len()].copy_from_slice(&entries);
    let bh = last_lba as usize * SECTOR;
    disk[bh..bh + SECTOR].copy_from_slice(&backup);
    disk
}

/// A deterministic (reproducible) RFC 4122 version-4-shaped GUID.
fn pseudo_guid(seed: u64) -> [u8; 16] {
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = [0u8; 16];
    for chunk in out.chunks_mut(8) {
        x ^= x >> 33;
        x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        x ^= x >> 33;
        chunk.copy_from_slice(&x.to_le_bytes());
    }
    out[7] = (out[7] & 0x0F) | 0x40;
    out[8] = (out[8] & 0x3F) | 0x80;
    out
}

/// CRC-32 (IEEE 802.3), as required by the GPT headers.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn disk_layout() {
        let part = vec![0x11u8; 4 * 1024 * 1024];
        let disk = build_disk(&[Partition::esp(&part)]);
        assert_eq!(&disk[SECTOR..SECTOR + 8], b"EFI PART");
        assert_eq!(disk[2048 * SECTOR], 0x11);
        let last = disk.len() - SECTOR;
        assert_eq!(&disk[last..last + 8], b"EFI PART");
        // Header CRC must verify.
        let mut h = disk[SECTOR..SECTOR + 92].to_vec();
        let stored = u32::from_le_bytes(h[16..20].try_into().unwrap());
        h[16..20].fill(0);
        assert_eq!(crc32(&h), stored);
    }
}
