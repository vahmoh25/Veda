//! ISO 9660 images that boot with UEFI, from a disc or from a USB stick.
//!
//! An image holds its files twice. Once in an ISO 9660 file system (level 1:
//! upper-case 8.3 names), which is what tools such as Rufus copy onto a FAT32
//! stick. And once in a FAT file system, which is two things at once: the El
//! Torito boot image that UEFI firmware starts from a disc, and, through a
//! partition table in the image's system area, the EFI system partition of a
//! stick the image is written to as it is (a "hybrid" image). The FAT file
//! system comes last: an El Torito size of 0, used when it is too large to
//! record, means "up to the end of the disc".

use std::collections::BTreeMap;
use std::ops::Range;

/// Bytes in an ISO 9660 logical block.
pub const BLOCK: usize = 2048;
/// Bytes in a sector of the disk a stick with the image on it becomes.
const DISK_SECTOR: usize = 512;
/// The system area (with the partition table) is the first 16 blocks.
const SYSTEM_AREA_BLOCKS: u32 = 16;
/// Fixed timestamps for reproducible images, 2026-01-01 12:00:00 UTC, as
/// the FAT file systems have: in directory records, and in the volume
/// descriptor.
const RECORD_DATE: [u8; 7] = [126, 1, 1, 12, 0, 0, 0];
const VOLUME_DATE: &[u8; 16] = b"2026010112000000";
const NO_DATE: &[u8; 16] = b"0000000000000000";
/// El Torito platform of a UEFI boot image.
const PLATFORM_EFI: u8 = 0xEF;
/// MBR partition type of an EFI system partition.
const MBR_TYPE_ESP: u8 = 0xEF;

#[derive(Default)]
struct Dir<'a> {
    dirs: BTreeMap<String, Dir<'a>>,
    files: BTreeMap<String, &'a [u8]>,
}

/// The files of an image, serialised by [`IsoBuilder::build`].
pub struct IsoBuilder<'a> {
    volume_id: String,
    root: Dir<'a>,
}

/// A directory being laid out: its parent's number in the path table, and
/// its block.
struct Placed<'d, 'a> {
    dir: &'d Dir<'a>,
    name: &'d str,
    parent: u16,
    block: u32,
}

/// Checks a level 1 name: up to eight d-characters (A-Z, 0-9, `_`), and
/// for files an extension of up to three.
fn check_name(name: &str, file: bool) -> Result<(), String> {
    let (base, ext) = match name.split_once('.') {
        Some((b, e)) if file => (b, e),
        _ => (name, ""),
    };
    let valid = |s: &str| s.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_');
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || !valid(base) || !valid(ext) {
        return Err(format!("'{name}' is not a valid ISO 9660 level 1 name"));
    }
    Ok(())
}

/// A file's identifier: name, `.`, extension (possibly empty), `;1`.
fn file_id(name: &str) -> String {
    if name.contains('.') { format!("{name};1") } else { format!("{name}.;1") }
}

/// The order of directory records: by name, then by extension, each as if
/// padded with spaces (ECMA-119 9.3).
fn sort_key(name: &str) -> (String, String) {
    let (base, ext) = name.split_once('.').unwrap_or((name, ""));
    (format!("{base:<8}"), format!("{ext:<3}"))
}

/// A 32-bit number in both byte orders (little-endian first).
fn both32(v: u32) -> [u8; 8] {
    let (le, be) = (v.to_le_bytes(), v.to_be_bytes());
    [le[0], le[1], le[2], le[3], be[0], be[1], be[2], be[3]]
}

/// A 16-bit number in both byte orders.
fn both16(v: u16) -> [u8; 4] {
    let (le, be) = (v.to_le_bytes(), v.to_be_bytes());
    [le[0], le[1], be[0], be[1]]
}

/// The bytes of block `b`.
fn blocks(b: u32) -> Range<usize> {
    b as usize * BLOCK..(b as usize + 1) * BLOCK
}

/// A directory record (ECMA-119 9.1); `id` is `[0]` for ".", `[1]` for "..".
fn dir_record(id: &[u8], extent: u32, len: u32, dir: bool) -> Vec<u8> {
    let size = 33 + id.len() + (id.len() + 1) % 2;
    let mut r = vec![0u8; size];
    r[0] = size as u8;
    r[2..10].copy_from_slice(&both32(extent));
    r[10..18].copy_from_slice(&both32(len));
    r[18..25].copy_from_slice(&RECORD_DATE);
    r[25] = if dir { 0x02 } else { 0 };
    r[28..32].copy_from_slice(&both16(1));
    r[32] = id.len() as u8;
    r[33..33 + id.len()].copy_from_slice(id);
    r
}

/// A path table record (ECMA-119 9.4), little- or big-endian.
fn path_record(id: &[u8], extent: u32, parent: u16, big: bool) -> Vec<u8> {
    let mut r = vec![0u8; 8 + id.len() + id.len() % 2];
    r[0] = id.len() as u8;
    r[2..6].copy_from_slice(&if big { extent.to_be_bytes() } else { extent.to_le_bytes() });
    r[6..8].copy_from_slice(&if big { parent.to_be_bytes() } else { parent.to_le_bytes() });
    r[8..8 + id.len()].copy_from_slice(id);
    r
}

/// Cylinder-head-sector address of a disk sector, for the partition table
/// (255 heads, 63 sectors a track; the largest address beyond its reach).
fn chs(lba: u64) -> [u8; 3] {
    let (c, h, s) = match lba / (255 * 63) {
        c if c > 1023 => (1023, 254, 63),
        c => (c, (lba / 63) % 255, lba % 63 + 1),
    };
    [h as u8, (s as u8) | ((c >> 2) as u8 & 0xC0), c as u8]
}

impl<'a> IsoBuilder<'a> {
    pub fn new(volume_id: &str) -> Self {
        IsoBuilder { volume_id: volume_id.to_ascii_uppercase(), root: Dir::default() }
    }

    /// Adds a file at `path` (components separated by `/`), creating parent
    /// directories as needed.
    pub fn add_file(&mut self, path: &str, data: &'a [u8]) -> Result<(), String> {
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let file = parts.pop().ok_or("empty path")?;
        let dir = self.dir(&parts)?;
        check_name(file, true)?;
        dir.files.insert(file.into(), data);
        Ok(())
    }

    /// Adds the directory at `path` (empty, if nothing goes into it), and
    /// its parents.
    pub fn add_dir(&mut self, path: &str) -> Result<(), String> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        self.dir(&parts).map(drop)
    }

    /// The directory at `parts`, made where it is not.
    fn dir(&mut self, parts: &[&str]) -> Result<&mut Dir<'a>, String> {
        let mut dir = &mut self.root;
        for p in parts {
            check_name(p, false)?;
            dir = dir.dirs.entry((*p).into()).or_default();
        }
        Ok(dir)
    }

    /// Serialises the image. `boot_image` makes the FAT file system given
    /// the disk sector it starts at (its partition's start, which its boot
    /// sector records); it goes last, padded to a whole block.
    pub fn build(&self, boot_image: impl FnOnce(u32) -> Result<Vec<u8>, String>) -> Result<Vec<u8>, String> {
        // Blocks 16-18: the primary volume descriptor, the El Torito boot
        // record and the terminator; then the boot catalog and the two path
        // tables, one block each.
        let (pvd_block, record_block, end_block) = (SYSTEM_AREA_BLOCKS, SYSTEM_AREA_BLOCKS + 1, SYSTEM_AREA_BLOCKS + 2);
        let (catalog, l_table, m_table) = (SYSTEM_AREA_BLOCKS + 3, SYSTEM_AREA_BLOCKS + 4, SYSTEM_AREA_BLOCKS + 5);
        let mut next = SYSTEM_AREA_BLOCKS + 6;

        // Directories in path table order (breadth first, each one's
        // subdirectories by name; the root is its own parent), a block each.
        let mut dirs = vec![Placed { dir: &self.root, name: "", parent: 1, block: 0 }];
        let mut i = 0;
        while i < dirs.len() {
            dirs[i].block = next;
            next += 1;
            let dir = dirs[i].dir;
            for (name, sub) in &dir.dirs {
                dirs.push(Placed { dir: sub, name, parent: (i + 1) as u16, block: 0 });
            }
            i += 1;
        }
        // Then the files, in the same order (empty ones have no extent).
        let mut file_block = BTreeMap::new();
        for (d, p) in dirs.iter().enumerate() {
            for (name, data) in &p.dir.files {
                file_block.insert((d, name.as_str()), if data.is_empty() { 0 } else { next });
                next += data.len().div_ceil(BLOCK) as u32;
            }
        }
        // And the FAT file system.
        let boot_block = next;
        let mut boot = boot_image(boot_block * (BLOCK / DISK_SECTOR) as u32)?;
        if boot.is_empty() || boot.len() % DISK_SECTOR != 0 {
            return Err("the boot image must be whole disk sectors".into());
        }
        let boot_sectors = (boot.len() / DISK_SECTOR) as u32;
        boot.resize(boot.len().next_multiple_of(BLOCK), 0);
        let total = boot_block + (boot.len() / BLOCK) as u32;
        let mut image = vec![0u8; total as usize * BLOCK];
        let at = boot_block as usize * BLOCK;
        image[at..at + boot.len()].copy_from_slice(&boot);

        // Directory extents and file data.
        for (d, p) in dirs.iter().enumerate() {
            let mut records = dir_record(&[0], p.block, BLOCK as u32, true);
            records.extend(dir_record(&[1], dirs[p.parent as usize - 1].block, BLOCK as u32, true));
            let mut entries: Vec<(&str, Vec<u8>)> = Vec::new();
            for (n, sub) in dirs.iter().enumerate() {
                if n > 0 && sub.parent as usize == d + 1 {
                    entries.push((sub.name, dir_record(sub.name.as_bytes(), sub.block, BLOCK as u32, true)));
                }
            }
            for (name, data) in &p.dir.files {
                let b = file_block[&(d, name.as_str())];
                entries.push((name, dir_record(file_id(name).as_bytes(), b, data.len() as u32, false)));
                let at = b as usize * BLOCK;
                image[at..at + data.len()].copy_from_slice(data);
            }
            entries.sort_by_key(|(name, _)| sort_key(name));
            records.extend(entries.into_iter().flat_map(|(_, r)| r));
            if records.len() > BLOCK {
                return Err(format!("directory '{}' has too many entries for one block", p.name));
            }
            image[blocks(p.block)][..records.len()].copy_from_slice(&records);
        }

        // Path tables.
        let (mut l, mut m) = (Vec::new(), Vec::new());
        for p in &dirs {
            let id: &[u8] = if p.name.is_empty() { &[0] } else { p.name.as_bytes() };
            l.extend(path_record(id, p.block, p.parent, false));
            m.extend(path_record(id, p.block, p.parent, true));
        }
        if l.len() > BLOCK {
            return Err("too many directories for a one-block path table".into());
        }
        image[blocks(l_table)][..l.len()].copy_from_slice(&l);
        image[blocks(m_table)][..m.len()].copy_from_slice(&m);

        // The primary volume descriptor.
        let pvd = &mut image[blocks(pvd_block)];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[8..72].fill(b' ');
        let vid = &self.volume_id.as_bytes()[..self.volume_id.len().min(32)];
        pvd[40..40 + vid.len()].copy_from_slice(vid);
        pvd[80..88].copy_from_slice(&both32(total));
        pvd[120..124].copy_from_slice(&both16(1));
        pvd[124..128].copy_from_slice(&both16(1));
        pvd[128..132].copy_from_slice(&both16(BLOCK as u16));
        pvd[132..140].copy_from_slice(&both32(l.len() as u32));
        pvd[140..144].copy_from_slice(&l_table.to_le_bytes());
        pvd[148..152].copy_from_slice(&m_table.to_be_bytes());
        pvd[156..190].copy_from_slice(&dir_record(&[0], dirs[0].block, BLOCK as u32, true));
        pvd[190..813].fill(b' ');
        for (at, date) in [(813, VOLUME_DATE), (830, VOLUME_DATE), (847, NO_DATE), (864, NO_DATE)] {
            pvd[at..at + 16].copy_from_slice(date);
        }
        pvd[881] = 1;

        // El Torito: the boot record points at the catalog, whose default
        // entry is the FAT file system. Its size counts 512-byte sectors; one
        // too large to record is 0 (to UEFI: "to the end of the disc").
        let record = &mut image[blocks(record_block)];
        record[1..6].copy_from_slice(b"CD001");
        record[6] = 1;
        record[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
        record[71..75].copy_from_slice(&catalog.to_le_bytes());
        let end = &mut image[blocks(end_block)];
        end[0] = 255;
        end[1..6].copy_from_slice(b"CD001");
        end[6] = 1;
        let cat = &mut image[blocks(catalog)];
        cat[0] = 1;
        cat[1] = PLATFORM_EFI;
        cat[4..8].copy_from_slice(b"VEDA");
        cat[30] = 0x55;
        cat[31] = 0xAA;
        let sum = cat[..32].chunks(2).fold(0u16, |s, w| s.wrapping_add(u16::from_le_bytes([w[0], w[1]])));
        cat[28..30].copy_from_slice(&0u16.wrapping_sub(sum).to_le_bytes());
        cat[32] = 0x88; // bootable, no emulation
        let count = u16::try_from(boot_sectors).unwrap_or(0);
        cat[38..40].copy_from_slice(&count.to_le_bytes());
        cat[40..44].copy_from_slice(&boot_block.to_le_bytes());

        // The partition table: the FAT file system is the EFI system
        // partition of a stick the image is written to.
        let start = boot_block as u64 * (BLOCK / DISK_SECTOR) as u64;
        let mbr = &mut image[..DISK_SECTOR];
        mbr[440..444].copy_from_slice(b"VEDA");
        let entry = &mut mbr[446..462];
        entry[1..4].copy_from_slice(&chs(start));
        entry[4] = MBR_TYPE_ESP;
        entry[5..8].copy_from_slice(&chs(start + boot_sectors as u64 - 1));
        entry[8..12].copy_from_slice(&(start as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&boot_sectors.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        Ok(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32le(b: &[u8]) -> u32 {
        u32::from_le_bytes(b[..4].try_into().unwrap())
    }

    /// Finds `path` by walking directory records from the root, as a reader
    /// such as Rufus does; returns the file's bytes.
    fn read(image: &[u8], path: &str) -> Option<Vec<u8>> {
        let pvd = &image[blocks(16)];
        let mut extent = u32le(&pvd[156 + 2..]);
        let parts: Vec<&str> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i == parts.len() - 1;
            let want = if last { file_id(part) } else { part.to_string() };
            let dir = &image[blocks(extent)];
            let (mut at, mut found) = (0, None);
            while at < BLOCK && dir[at] != 0 {
                let r = &dir[at..at + dir[at] as usize];
                if &r[33..33 + r[32] as usize] == want.as_bytes() {
                    found = Some((u32le(&r[2..]), u32le(&r[10..]) as usize, r[25] & 2 != 0));
                }
                at += r[0] as usize;
            }
            let (e, len, dir) = found?;
            assert_eq!(dir, !last);
            if last {
                return Some(image[blocks(e).start..blocks(e).start + len].to_vec());
            }
            extent = e;
        }
        None
    }

    fn initrd() -> Vec<u8> {
        (0..5000u32).map(|i| i as u8).collect()
    }

    /// An image with a boot image of `boot_bytes`, and the disk sector the
    /// builder said that starts at.
    fn sample(boot_bytes: usize) -> (Vec<u8>, u32) {
        let initrd = initrd();
        let mut b = IsoBuilder::new("veda");
        b.add_file("EFI/BOOT/BOOTX64.EFI", b"loader").unwrap();
        b.add_file("VEDA/INITRD.IMG", &initrd).unwrap();
        b.add_file("VEDA/BOOT.CFG", b"cmdline=live\n").unwrap();
        b.add_file("VEDA/EMPTY", b"").unwrap();
        let mut start = 0;
        let image = b
            .build(|s| {
                start = s;
                Ok(vec![0xFA; boot_bytes])
            })
            .unwrap();
        (image, start)
    }

    #[test]
    fn files_and_volume() {
        let (image, _) = sample(5 * BLOCK);
        assert_eq!(image.len() % BLOCK, 0);
        let pvd = &image[blocks(16)];
        assert_eq!(&pvd[1..6], b"CD001");
        assert_eq!(&pvd[40..45], b"VEDA ");
        assert_eq!(u32le(&pvd[80..]) as usize, image.len() / BLOCK);
        assert_eq!(&image[blocks(18)][..6], b"\xffCD001");
        assert_eq!(read(&image, "EFI/BOOT/BOOTX64.EFI").unwrap(), b"loader");
        assert_eq!(read(&image, "VEDA/BOOT.CFG").unwrap(), b"cmdline=live\n");
        assert_eq!(read(&image, "VEDA/INITRD.IMG").unwrap(), initrd());
        assert_eq!(read(&image, "VEDA/EMPTY").unwrap(), b"");
        assert!(read(&image, "VEDA/MISSING.TXT").is_none());
        // The little-endian path table: the root, EFI, VEDA, then BOOT (in EFI).
        let l = &image[blocks(u32le(&pvd[140..])).start..];
        let (mut at, mut names) = (0, Vec::new());
        for _ in 0..4 {
            let len = l[at] as usize;
            names.push((l[at + 8..at + 8 + len].to_vec(), u16::from_le_bytes([l[at + 6], l[at + 7]])));
            at += 8 + len + len % 2;
        }
        assert_eq!(names, [(vec![0], 1), (b"EFI".to_vec(), 1), (b"VEDA".to_vec(), 1), (b"BOOT".to_vec(), 2)]);
        assert_eq!(at, u32le(&pvd[132..]) as usize);
    }

    #[test]
    fn el_torito_and_partition() {
        let (image, start) = sample(5 * BLOCK);
        let record = &image[blocks(17)];
        assert_eq!(&record[7..30], b"EL TORITO SPECIFICATION");
        let cat = &image[blocks(u32le(&record[71..]))];
        let sum = cat[..32].chunks(2).fold(0u16, |s, w| s.wrapping_add(u16::from_le_bytes([w[0], w[1]])));
        assert_eq!((sum, cat[0], cat[1], cat[30], cat[31]), (0, 1, 0xEF, 0x55, 0xAA));
        assert_eq!((cat[32], cat[33]), (0x88, 0));
        assert_eq!(u16::from_le_bytes([cat[38], cat[39]]), 20);
        let boot = u32le(&cat[40..]);
        assert_eq!(&image[blocks(boot)][..4], &[0xFA; 4]);
        assert_eq!(boot as usize + 5, image.len() / BLOCK, "the boot image comes last");
        // The partition starts where the boot image does, in disk sectors.
        assert_eq!((image[510], image[511], image[446 + 4]), (0x55, 0xAA, 0xEF));
        assert_eq!(u32le(&image[446 + 8..]), start);
        assert_eq!(start, boot * 4);
        assert_eq!(u32le(&image[446 + 12..]), 20);
    }

    #[test]
    fn oversized_boot_image_runs_to_the_end() {
        let (image, _) = sample(65536 * DISK_SECTOR);
        let cat = &image[blocks(u32le(&image[blocks(17)][71..]))];
        assert_eq!(u16::from_le_bytes([cat[38], cat[39]]), 0);
        assert_eq!(u32le(&image[446 + 12..]), 65536);
    }

    #[test]
    fn names() {
        let mut b = IsoBuilder::new("VEDA");
        assert!(b.add_file("VEDA/TOOLONGNAME.TXT", b"").is_err());
        assert!(b.add_file("VEDA/lower.txt", b"").is_err());
        assert!(b.add_file("TOOLONGDIR/A.TXT", b"").is_err());
        assert!(b.add_file("VEDA/A.TEXT", b"").is_err());
        assert!(b.add_file("VEDA/README", b"").is_ok());
        assert_eq!(file_id("README"), "README.;1");
        assert_eq!(file_id("BOOT.CFG"), "BOOT.CFG;1");
        assert_eq!(chs(0), [0, 1, 0]);
        assert_eq!(chs(u32::MAX as u64), [254, 0xFF, 0xFF]);
    }
}
