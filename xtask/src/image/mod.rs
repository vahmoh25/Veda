//! Assembly of the bootable Veda disk image, and of the live system's ISO
//! image.

pub mod fat32;
pub mod gpt;
pub mod iso9660;

use std::path::Path;

use fat32::{Fat32Builder, SECTOR};

/// Files placed on the EFI system partition.
pub struct EspContents<'a> {
    pub bootloader: &'a [u8],
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    pub symbols: &'a [u8],
    pub boot_cfg: &'a str,
}

impl EspContents<'_> {
    /// The partition's files: where they go, and what they hold.
    fn files(&self) -> Vec<(&'static str, &[u8])> {
        let mut files = vec![
            ("EFI/BOOT/BOOTX64.EFI", self.bootloader),
            ("VEDA/VKERNEL.EXE", self.kernel),
            ("VEDA/INITRD.IMG", self.initrd),
        ];
        if !self.symbols.is_empty() {
            files.push(("VEDA/VKERNEL.SYM", self.symbols));
        }
        files.push(("VEDA/BOOT.CFG", self.boot_cfg.as_bytes()));
        files
    }

    /// A FAT32 file system with the partition's files and directories; and
    /// the files' size.
    fn fat(&self) -> Result<(Fat32Builder, u64), String> {
        let mut fat = Fat32Builder::new("VEDA");
        let mut payload = 0;
        for (path, data) in self.files() {
            fat.add_file(path, data.to_vec())?;
            payload += data.len() as u64;
        }
        fat.add_dir(LOGS)?;
        Ok((fat, payload))
    }
}

/// Where a stick Veda started from keeps the system's logs (the driver
/// VM's `logkeeper`, which keeps them on a disk that has the directory).
pub const LOGS: &str = "VEDA/LOGS";
/// The room the live system's image leaves for them.
const LOGS_ROOM_MIB: u64 = 64;

/// Builds a GPT disk with a single FAT32 EFI system partition and writes it to
/// `out`. The partition is sized to fit its contents with ample headroom.
pub fn write_disk_image(out: &Path, esp: &EspContents) -> Result<u64, String> {
    let (fat, payload) = esp.fat()?;
    // FAT32 needs >= 65525 clusters; 128 MiB is the smallest comfortable size.
    let mib = ((payload * 3 / 2) / (1024 * 1024) + 32).max(128).next_multiple_of(32);
    let sectors = (mib * 1024 * 1024 / SECTOR as u64) as u32;
    let volume = fat.build(sectors, gpt::FIRST_PARTITION_LBA as u32)?;
    let disk = gpt::build_disk(&[gpt::Partition::esp(&volume)]);
    std::fs::write(out, &disk).map_err(|e| format!("writing {}: {e}", out.display()))?;
    Ok(disk.len() as u64)
}

/// Builds the live system's ISO image (see [`iso9660`]) and writes it to
/// `out`: the partition's files in an ISO 9660 file system, and the
/// partition itself as the UEFI boot image and a stick's EFI system
/// partition.
pub fn write_iso_image(out: &Path, esp: &EspContents) -> Result<u64, String> {
    let image = iso_image(esp)?;
    std::fs::write(out, &image).map_err(|e| format!("writing {}: {e}", out.display()))?;
    Ok(image.len() as u64)
}

fn iso_image(esp: &EspContents) -> Result<Vec<u8>, String> {
    let (fat, payload) = esp.fat()?;
    // A little room beyond the files, and the logs' (a FAT32 file system
    // has at least 65525 clusters, about 34 MiB).
    let mib = (payload * 11 / 10 / (1024 * 1024) + 4 + LOGS_ROOM_MIB).max(40);
    let sectors = (mib * 1024 * 1024 / SECTOR as u64) as u32;
    let mut iso = iso9660::IsoBuilder::new("VEDA");
    let files = esp.files();
    for &(path, data) in &files {
        iso.add_file(path, data)?;
    }
    // A stick the ISO's files are copied onto (by Rufus) keeps the logs too.
    iso.add_dir(LOGS)?;
    iso.build(|start| fat.build(sectors, start))
}

/// The file at `path` on the EFI system partition of `image`, a stick the
/// live system's image was written to (its MBR names the partition): what
/// a test's guest wrote there.
pub fn esp_file(image: &[u8], path: &str) -> Result<Vec<u8>, String> {
    const MBR_TYPE_ESP: u8 = 0xEF;
    if image.get(510..512) != Some(&[0x55, 0xAA]) {
        return Err(String::from("no partition table"));
    }
    let entry =
        image[446..510].as_chunks::<16>().0.iter().find(|e| e[4] == MBR_TYPE_ESP).ok_or("no EFI system partition")?;
    let start = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize * SECTOR;
    let len = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize * SECTOR;
    let volume = image.get(start..start + len).ok_or("the partition goes beyond the image")?;
    fat32::read_file(volume, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_live_image_has_room_for_the_logs() {
        let esp = EspContents {
            bootloader: b"MZ loader",
            kernel: b"MZ kernel",
            initrd: &[7; 10_000],
            symbols: &[],
            boot_cfg: "run=editor",
        };
        let image = iso_image(&esp).unwrap();
        assert_eq!(esp_file(&image, "VEDA/BOOT.CFG").unwrap(), b"run=editor");
        assert_eq!(esp_file(&image, "efi/boot/bootx64.efi").unwrap(), b"MZ loader");
        assert!(esp_file(&image, LOGS).unwrap_err().contains("a directory"));
        assert!(image.len() as u64 > LOGS_ROOM_MIB * 1024 * 1024);
        assert!(esp_file(&[0; 4096], "VEDA/BOOT.CFG").is_err());
    }
}
