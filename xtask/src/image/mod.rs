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

    /// A FAT32 file system with the partition's files; and their size.
    fn fat(&self) -> Result<(Fat32Builder, u64), String> {
        let mut fat = Fat32Builder::new("VEDA");
        let mut payload = 0;
        for (path, data) in self.files() {
            fat.add_file(path, data.to_vec())?;
            payload += data.len() as u64;
        }
        Ok((fat, payload))
    }
}

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
    let (fat, payload) = esp.fat()?;
    // Only a little room beyond the files: nothing is ever written to it. A
    // FAT32 file system has at least 65525 clusters, about 34 MiB.
    let mib = (payload * 11 / 10 / (1024 * 1024) + 4).max(40);
    let sectors = (mib * 1024 * 1024 / SECTOR as u64) as u32;
    let mut iso = iso9660::IsoBuilder::new("VEDA");
    let files = esp.files();
    for &(path, data) in &files {
        iso.add_file(path, data)?;
    }
    let image = iso.build(|start| fat.build(sectors, start))?;
    std::fs::write(out, &image).map_err(|e| format!("writing {}: {e}", out.display()))?;
    Ok(image.len() as u64)
}
