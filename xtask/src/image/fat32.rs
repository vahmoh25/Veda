//! A minimal FAT32 formatter that writes a populated volume into memory,
//! and a reader of files on such a volume (what a test's guest wrote).
//!
//! Only what the EFI system partition needs is implemented: short (8.3)
//! upper-case names, nested directories (empty ones too) and regular
//! files. The result is a spec-conformant volume that OVMF (and Windows,
//! Linux, ...) can mount.

use std::collections::BTreeMap;

pub const SECTOR: usize = 512;
const RESERVED_SECTORS: u32 = 32;
const NUM_FATS: u32 = 2;
const MIN_FAT32_CLUSTERS: u32 = 65_525;
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_VOLUME_ID: u8 = 0x08;

#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: BTreeMap<String, Vec<u8>>,
}

/// An in-memory directory tree that is serialised by [`Fat32Builder::build`].
#[derive(Default)]
pub struct Fat32Builder {
    root: Dir,
    label: String,
}

/// Fixed timestamp for reproducible images: 2026-01-01 12:00:00.
const FAT_DATE: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1;
const FAT_TIME: u16 = 12 << 11;

fn short_name(component: &str) -> Result<[u8; 11], String> {
    let upper = component.to_ascii_uppercase();
    let (base, ext) = match upper.rsplit_once('.') {
        Some((b, e)) => (b, e),
        None => (upper.as_str(), ""),
    };
    let valid = |s: &str| s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-~!#$%&'()@^`{}".contains(&c));
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || !valid(base) || !valid(ext) {
        return Err(format!("'{component}' is not a valid 8.3 file name"));
    }
    let mut name = [b' '; 11];
    name[..base.len()].copy_from_slice(base.as_bytes());
    name[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    Ok(name)
}

impl Fat32Builder {
    pub fn new(label: &str) -> Self {
        Self { root: Dir::default(), label: label.to_ascii_uppercase() }
    }

    /// Adds a file at `path` (components separated by `/`), creating parent
    /// directories as needed.
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) -> Result<(), String> {
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let file = parts.pop().ok_or("empty path")?;
        let dir = self.dir(&parts)?;
        short_name(file)?;
        dir.files.insert(file.to_ascii_uppercase(), data);
        Ok(())
    }

    /// Adds the directory at `path` (empty, if nothing goes into it),
    /// creating parent directories as needed.
    pub fn add_dir(&mut self, path: &str) -> Result<(), String> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        self.dir(&parts).map(drop)
    }

    /// The directory at `parts`, made where it is not.
    fn dir(&mut self, parts: &[&str]) -> Result<&mut Dir, String> {
        let mut dir = &mut self.root;
        for p in parts {
            short_name(p)?;
            dir = dir.dirs.entry(p.to_ascii_uppercase()).or_default();
        }
        Ok(dir)
    }

    /// Serialises the volume into exactly `total_sectors` sectors.
    /// `hidden_sectors` is the partition's starting LBA.
    pub fn build(&self, total_sectors: u32, hidden_sectors: u32) -> Result<Vec<u8>, String> {
        let geometry = Geometry::choose(total_sectors)?;
        let mut vol = Volume {
            image: vec![0u8; total_sectors as usize * SECTOR],
            fat: vec![0u32; geometry.cluster_count as usize + 2],
            next_free: 2,
            g: geometry,
        };
        vol.fat[0] = 0x0FFF_FFF8;
        vol.fat[1] = END_OF_CHAIN;

        // The root directory always starts at cluster 2.
        let root_cluster = vol.write_dir(&self.root, None, Some(&self.label))?;
        assert_eq!(root_cluster, 2);

        let used = vol.next_free - 2;
        let free = vol.g.cluster_count - used;
        vol.write_boot_sectors(hidden_sectors, free, &self.label);
        vol.write_fats();
        Ok(vol.image)
    }
}

#[derive(Clone, Copy)]
struct Geometry {
    total_sectors: u32,
    sectors_per_cluster: u32,
    fat_sectors: u32,
    cluster_count: u32,
}

impl Geometry {
    /// Picks the largest cluster size (up to 4 KiB) that still yields a valid
    /// FAT32 cluster count, following the computation in Microsoft's FAT spec.
    fn choose(total_sectors: u32) -> Result<Self, String> {
        for spc in [8u32, 4, 2, 1] {
            let tmp1 = total_sectors - RESERVED_SECTORS;
            let tmp2 = (256 * spc + NUM_FATS) / 2;
            let fat_sectors = tmp1.div_ceil(tmp2);
            let data_sectors = total_sectors - RESERVED_SECTORS - NUM_FATS * fat_sectors;
            let cluster_count = data_sectors / spc;
            if cluster_count >= MIN_FAT32_CLUSTERS + 16 {
                return Ok(Geometry { total_sectors, sectors_per_cluster: spc, fat_sectors, cluster_count });
            }
        }
        Err(format!("volume of {total_sectors} sectors is too small for FAT32"))
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR
    }

    fn data_start_sector(&self) -> u32 {
        RESERVED_SECTORS + NUM_FATS * self.fat_sectors
    }
}

struct Volume {
    image: Vec<u8>,
    fat: Vec<u32>,
    next_free: u32,
    g: Geometry,
}

impl Volume {
    /// Allocates a contiguous cluster chain big enough for `bytes` (at least
    /// one cluster) and returns its first cluster.
    fn alloc_chain(&mut self, bytes: usize) -> Result<u32, String> {
        let n = bytes.div_ceil(self.g.cluster_bytes()).max(1) as u32;
        let first = self.next_free;
        if first + n > self.g.cluster_count + 2 {
            return Err("FAT32 volume is full; increase the ESP size".into());
        }
        for c in first..first + n {
            self.fat[c as usize] = if c == first + n - 1 { END_OF_CHAIN } else { c + 1 };
        }
        self.next_free += n;
        Ok(first)
    }

    fn cluster_offset(&self, cluster: u32) -> usize {
        let sector = self.g.data_start_sector() + (cluster - 2) * self.g.sectors_per_cluster;
        sector as usize * SECTOR
    }

    fn write_at_cluster(&mut self, cluster: u32, data: &[u8]) {
        let off = self.cluster_offset(cluster);
        self.image[off..off + data.len()].copy_from_slice(data);
    }

    /// Writes `dir` (recursively) and returns its first cluster.
    fn write_dir(&mut self, dir: &Dir, parent: Option<u32>, label: Option<&str>) -> Result<u32, String> {
        let is_root = parent.is_none();
        let entries = dir.dirs.len() + dir.files.len() + if is_root { 1 } else { 2 };
        let first = self.alloc_chain(entries * 32)?;

        let mut buf: Vec<u8> = Vec::with_capacity(entries * 32);
        if let Some(label) = label {
            let mut name = [b' '; 11];
            for (i, b) in label.bytes().take(11).enumerate() {
                name[i] = b;
            }
            push_entry(&mut buf, &name, ATTR_VOLUME_ID, 0, 0);
        }
        if let Some(parent) = parent {
            push_entry(&mut buf, b".          ", ATTR_DIRECTORY, first, 0);
            // A ".." entry pointing at the root uses cluster 0.
            let p = if parent == 2 { 0 } else { parent };
            push_entry(&mut buf, b"..         ", ATTR_DIRECTORY, p, 0);
        }
        for (name, sub) in &dir.dirs {
            let cluster = self.write_dir(sub, Some(first), None)?;
            push_entry(&mut buf, &short_name(name)?, ATTR_DIRECTORY, cluster, 0);
        }
        for (name, data) in &dir.files {
            let cluster = if data.is_empty() { 0 } else { self.alloc_chain(data.len())? };
            if cluster != 0 {
                self.write_at_cluster(cluster, data);
            }
            push_entry(&mut buf, &short_name(name)?, ATTR_ARCHIVE, cluster, data.len() as u32);
        }
        self.write_at_cluster(first, &buf);
        Ok(first)
    }

    fn write_boot_sectors(&mut self, hidden: u32, free_clusters: u32, label: &str) {
        let g = self.g;
        let mut bs = [0u8; SECTOR];
        bs[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        bs[3..11].copy_from_slice(b"VEDA    ");
        bs[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        bs[13] = g.sectors_per_cluster as u8;
        bs[14..16].copy_from_slice(&(RESERVED_SECTORS as u16).to_le_bytes());
        bs[16] = NUM_FATS as u8;
        bs[21] = 0xF8; // fixed media
        bs[24..26].copy_from_slice(&63u16.to_le_bytes());
        bs[26..28].copy_from_slice(&255u16.to_le_bytes());
        bs[28..32].copy_from_slice(&hidden.to_le_bytes());
        bs[32..36].copy_from_slice(&g.total_sectors.to_le_bytes());
        bs[36..40].copy_from_slice(&g.fat_sectors.to_le_bytes());
        bs[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
        bs[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
        bs[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
        bs[64] = 0x80;
        bs[66] = 0x29;
        bs[67..71].copy_from_slice(&0x5645_4441u32.to_le_bytes()); // volume id "VEDA"
        let mut lab = [b' '; 11];
        for (i, b) in label.bytes().take(11).enumerate() {
            lab[i] = b;
        }
        bs[71..82].copy_from_slice(&lab);
        bs[82..90].copy_from_slice(b"FAT32   ");
        bs[510] = 0x55;
        bs[511] = 0xAA;

        let mut fsinfo = [0u8; SECTOR];
        fsinfo[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        fsinfo[488..492].copy_from_slice(&free_clusters.to_le_bytes());
        fsinfo[492..496].copy_from_slice(&self.next_free.to_le_bytes());
        fsinfo[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());

        for (sector, data) in [(0usize, &bs), (1, &fsinfo), (6, &bs), (7, &fsinfo)] {
            self.image[sector * SECTOR..(sector + 1) * SECTOR].copy_from_slice(data);
        }
    }

    fn write_fats(&mut self) {
        let fat_bytes = self.g.fat_sectors as usize * SECTOR;
        let mut fat = vec![0u8; fat_bytes];
        for (i, e) in self.fat.iter().enumerate() {
            fat[i * 4..i * 4 + 4].copy_from_slice(&e.to_le_bytes());
        }
        for n in 0..NUM_FATS as usize {
            let off = (RESERVED_SECTORS as usize) * SECTOR + n * fat_bytes;
            self.image[off..off + fat_bytes].copy_from_slice(&fat);
        }
    }
}

/// The file at `path` (8.3 names, any case) on FAT32 `volume`, as the
/// volume has it: written by [`Fat32Builder`] or by anyone.
pub fn read_file(volume: &[u8], path: &str) -> Result<Vec<u8>, String> {
    let u16_at = |at: usize| volume.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |at: usize| volume.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let not_fat32 = || String::from("not a FAT32 file system");
    if volume.get(82..90) != Some(b"FAT32   ") {
        return Err(not_fat32());
    }
    let sector = u16_at(11).ok_or_else(not_fat32)?;
    let cluster_bytes = sector * *volume.get(13).ok_or_else(not_fat32)? as usize;
    let fat_at = sector * u16_at(14).ok_or_else(not_fat32)?;
    let fats = *volume.get(16).ok_or_else(not_fat32)? as usize;
    let data_at = fat_at + fats * sector * u32_at(36).ok_or_else(not_fat32)?;
    // A chain's clusters, in order; at most as many as the volume has.
    let chain = |first: usize| -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        let mut cluster = first;
        while (2..0x0FFF_FFF8).contains(&cluster) {
            let at = data_at + (cluster - 2) * cluster_bytes;
            bytes.extend_from_slice(volume.get(at..at + cluster_bytes).ok_or("a cluster beyond the volume")?);
            if bytes.len() > volume.len() {
                return Err(String::from("a cluster chain that loops"));
            }
            cluster = u32_at(fat_at + cluster * 4).ok_or("a cluster beyond the FAT")? & 0x0FFF_FFFF;
        }
        Ok(bytes)
    };
    let mut entries = chain(u32_at(44).ok_or_else(not_fat32)?)?;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        let name = short_name(part)?;
        let entry = entries
            .as_chunks::<32>()
            .0
            .iter()
            .take_while(|e| e[0] != 0)
            .find(|e| e[0] != 0xE5 && e[11] != 0x0F && e[11] & ATTR_VOLUME_ID == 0 && e[..11] == name)
            .ok_or_else(|| format!("{path}: no {part}"))?;
        let first = (u16::from_le_bytes([entry[20], entry[21]]) as usize) << 16
            | u16::from_le_bytes([entry[26], entry[27]]) as usize;
        let size = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]) as usize;
        let last = i + 1 == parts.len();
        match (entry[11] & ATTR_DIRECTORY != 0, last) {
            (false, true) => {
                let mut data = if first == 0 { Vec::new() } else { chain(first)? };
                if data.len() < size {
                    return Err(format!("{path}: shorter than its size"));
                }
                data.truncate(size);
                return Ok(data);
            }
            (true, false) => entries = chain(first)?,
            (true, true) => return Err(format!("{path}: a directory")),
            (false, false) => return Err(format!("{path}: {part} is a file")),
        }
    }
    Err(format!("{path}: no file named"))
}

fn push_entry(buf: &mut Vec<u8>, name: &[u8; 11], attr: u8, cluster: u32, size: u32) {
    let mut e = [0u8; 32];
    e[0..11].copy_from_slice(name);
    e[11] = attr;
    e[14..16].copy_from_slice(&FAT_TIME.to_le_bytes());
    e[16..18].copy_from_slice(&FAT_DATE.to_le_bytes());
    e[18..20].copy_from_slice(&FAT_DATE.to_le_bytes());
    e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[22..24].copy_from_slice(&FAT_TIME.to_le_bytes());
    e[24..26].copy_from_slice(&FAT_DATE.to_le_bytes());
    e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&e);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(&short_name("bootx64.efi").unwrap(), b"BOOTX64 EFI");
        assert_eq!(&short_name("EFI").unwrap(), b"EFI        ");
        assert!(short_name("waytoolongname.txt").is_err());
        assert!(short_name("a.toolong").is_err());
    }

    #[test]
    fn reads_back_what_it_wrote() {
        let mut b = Fat32Builder::new("VEDA");
        let big: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        b.add_file("VEDA/BOOT.CFG", b"run=editor".to_vec()).unwrap();
        b.add_file("VEDA/BIG.BIN", big.clone()).unwrap();
        b.add_file("VEDA/EMPTY.TXT", Vec::new()).unwrap();
        b.add_dir("VEDA/LOGS").unwrap();
        let img = b.build(64 * 1024 * 1024 / SECTOR as u32, 0).unwrap();
        assert_eq!(read_file(&img, "veda/boot.cfg").unwrap(), b"run=editor");
        assert_eq!(read_file(&img, "VEDA/BIG.BIN").unwrap(), big);
        assert_eq!(read_file(&img, "VEDA/EMPTY.TXT").unwrap(), b"");
        assert!(read_file(&img, "VEDA/LOGS").unwrap_err().contains("a directory"));
        assert!(read_file(&img, "VEDA/LOGS/0001/VEDA.TXT").unwrap_err().contains("no 0001"));
        assert!(read_file(&img, "VEDA/NONE.TXT").is_err());
        assert!(read_file(&[0; 4096], "VEDA/BOOT.CFG").is_err());
    }

    #[test]
    fn builds_volume_with_files() {
        let mut b = Fat32Builder::new("VEDA");
        b.add_file("EFI/BOOT/BOOTX64.EFI", vec![0xAB; 10_000]).unwrap();
        b.add_file("VEDA/BOOT.CFG", b"hello".to_vec()).unwrap();
        let sectors = 128 * 1024 * 1024 / SECTOR as u32;
        let img = b.build(sectors, 2048).unwrap();
        assert_eq!(img.len(), sectors as usize * SECTOR);
        assert_eq!(&img[510..512], &[0x55, 0xAA]);
        assert_eq!(&img[82..90], b"FAT32   ");
        let g = Geometry::choose(sectors).unwrap();
        assert!(g.cluster_count >= MIN_FAT32_CLUSTERS);
        // The first FAT entry carries the media descriptor.
        let fat0 = RESERVED_SECTORS as usize * SECTOR;
        assert_eq!(&img[fat0..fat0 + 4], &0x0FFF_FFF8u32.to_le_bytes());
    }
}
