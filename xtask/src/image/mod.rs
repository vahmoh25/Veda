//! Assembly of the bootable Vindows disk image.

pub mod fat32;
pub mod gpt;

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

/// Builds a GPT disk with a single FAT32 EFI system partition and writes it to
/// `out`. The partition is sized to fit its contents with ample headroom.
pub fn write_disk_image(out: &Path, esp: &EspContents) -> Result<u64, String> {
    let mut fat = Fat32Builder::new("VINDOWS");
    fat.add_file("EFI/BOOT/BOOTX64.EFI", esp.bootloader.to_vec())?;
    fat.add_file("VINDOWS/VKERNEL.EXE", esp.kernel.to_vec())?;
    fat.add_file("VINDOWS/INITRD.IMG", esp.initrd.to_vec())?;
    if !esp.symbols.is_empty() {
        fat.add_file("VINDOWS/VKERNEL.SYM", esp.symbols.to_vec())?;
    }
    fat.add_file("VINDOWS/BOOT.CFG", esp.boot_cfg.as_bytes().to_vec())?;

    let payload = esp.bootloader.len() + esp.kernel.len() + esp.initrd.len() + esp.symbols.len();
    // FAT32 needs >= 65525 clusters; 128 MiB is the smallest comfortable size.
    let mib = ((payload as u64 * 3 / 2) / (1024 * 1024) + 32).max(128).next_multiple_of(32);
    let sectors = (mib * 1024 * 1024 / SECTOR as u64) as u32;
    let volume = fat.build(sectors, gpt::FIRST_PARTITION_LBA as u32)?;
    let disk = gpt::build_disk(&[gpt::Partition::esp(&volume)]);
    std::fs::write(out, &disk).map_err(|e| format!("writing {}: {e}", out.display()))?;
    Ok(disk.len() as u64)
}
