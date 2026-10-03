//! A small, allocation-free parser for PE32+ (x86-64 Portable Executable)
//! images.
//!
//! Vindows uses PE as its executable format for the kernel and for every
//! user-space program. Images are linked at a fixed base without imports, so a
//! loader only has to:
//!
//! 1. allocate `size_of_image` bytes at `image_base`,
//! 2. copy the headers and each section's raw data to `base + virtual_address`,
//!    zero-filling the remainder of each section,
//! 3. apply section permissions, and
//! 4. jump to `image_base + entry_rva`.
//!
//! Images that were linked with base relocations can be loaded at another
//! address with [`PeImage::relocations`].

#![no_std]

use core::fmt;

/// Errors produced while parsing an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeError {
    /// The buffer is too small for a header it claims to contain.
    Truncated,
    /// Missing `MZ` or `PE\0\0` signature.
    BadSignature,
    /// Not an x86-64 image.
    WrongMachine,
    /// Not a PE32+ optional header.
    NotPe32Plus,
    /// A section or data directory points outside the file or image.
    BadSection,
    /// The image imports symbols from DLLs, which Vindows does not support.
    HasImports,
    /// Malformed base relocation block.
    BadRelocation,
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            PeError::Truncated => "image is truncated",
            PeError::BadSignature => "bad MZ/PE signature",
            PeError::WrongMachine => "image is not x86-64",
            PeError::NotPe32Plus => "image is not PE32+",
            PeError::BadSection => "section or directory out of bounds",
            PeError::HasImports => "image has DLL imports",
            PeError::BadRelocation => "malformed base relocation",
        };
        f.write_str(msg)
    }
}

const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const PE32_PLUS_MAGIC: u16 = 0x20b;
const DIR_IMPORT: usize = 1;
const DIR_BASERELOC: usize = 5;

/// Section may be executed.
pub const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
/// Section may be read.
pub const SCN_MEM_READ: u32 = 0x4000_0000;
/// Section may be written.
pub const SCN_MEM_WRITE: u32 = 0x8000_0000;
/// Section contains uninitialized data.
pub const SCN_CNT_UNINITIALIZED_DATA: u32 = 0x0000_0080;
/// Section can be discarded after loading (e.g. `.reloc`).
pub const SCN_MEM_DISCARDABLE: u32 = 0x0200_0000;

/// The subsystem field of the optional header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    Native,
    WindowsGui,
    WindowsCui,
    EfiApplication,
    Other(u16),
}

fn rd_u16(b: &[u8], off: usize) -> Result<u16, PeError> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or(PeError::Truncated)
}

fn rd_u32(b: &[u8], off: usize) -> Result<u32, PeError> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(PeError::Truncated)
}

fn rd_u64(b: &[u8], off: usize) -> Result<u64, PeError> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or(PeError::Truncated)
}

/// A parsed, validated PE32+ image backed by its file bytes.
#[derive(Clone, Copy)]
pub struct PeImage<'a> {
    data: &'a [u8],
    image_base: u64,
    entry_rva: u32,
    size_of_image: u32,
    size_of_headers: u32,
    section_alignment: u32,
    subsystem: u16,
    stack_reserve: u64,
    sections_off: usize,
    num_sections: usize,
    num_dirs: usize,
    dirs_off: usize,
}

impl fmt::Debug for PeImage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeImage")
            .field("image_base", &format_args!("{:#x}", self.image_base))
            .field("entry_rva", &format_args!("{:#x}", self.entry_rva))
            .field("size_of_image", &self.size_of_image)
            .field("sections", &self.num_sections)
            .finish()
    }
}

impl<'a> PeImage<'a> {
    /// Parses and validates `data` as an x86-64 PE32+ image.
    pub fn parse(data: &'a [u8]) -> Result<Self, PeError> {
        if data.get(0..2) != Some(b"MZ") {
            return Err(PeError::BadSignature);
        }
        let pe_off = rd_u32(data, 0x3c)? as usize;
        if data.get(pe_off..pe_off + 4) != Some(b"PE\0\0") {
            return Err(PeError::BadSignature);
        }
        let coff = pe_off + 4;
        if rd_u16(data, coff)? != IMAGE_FILE_MACHINE_AMD64 {
            return Err(PeError::WrongMachine);
        }
        let num_sections = rd_u16(data, coff + 2)? as usize;
        let opt_size = rd_u16(data, coff + 16)? as usize;
        let opt = coff + 20;
        if rd_u16(data, opt)? != PE32_PLUS_MAGIC {
            return Err(PeError::NotPe32Plus);
        }
        let entry_rva = rd_u32(data, opt + 16)?;
        let image_base = rd_u64(data, opt + 24)?;
        let section_alignment = rd_u32(data, opt + 32)?;
        let size_of_image = rd_u32(data, opt + 56)?;
        let size_of_headers = rd_u32(data, opt + 60)?;
        let subsystem = rd_u16(data, opt + 68)?;
        let stack_reserve = rd_u64(data, opt + 72)?;
        let num_dirs = rd_u32(data, opt + 108)? as usize;
        let dirs_off = opt + 112;
        let sections_off = opt + opt_size;
        if data.len() < sections_off + num_sections * 40 || num_dirs > 16 {
            return Err(PeError::Truncated);
        }
        if size_of_headers as usize > data.len() || size_of_headers > size_of_image {
            return Err(PeError::BadSection);
        }
        let image = PeImage {
            data,
            image_base,
            entry_rva,
            size_of_image,
            size_of_headers,
            section_alignment,
            subsystem,
            stack_reserve,
            sections_off,
            num_sections,
            num_dirs,
            dirs_off,
        };
        for s in image.sections() {
            // `sections()` normalises a zero virtual size to the raw size.
            if s.virtual_address as u64 + s.virtual_size as u64 > size_of_image as u64 {
                return Err(PeError::BadSection);
            }
            if s.raw_size > 0 && s.raw_offset as usize + s.raw_size as usize > data.len() {
                return Err(PeError::BadSection);
            }
        }
        if entry_rva >= size_of_image {
            return Err(PeError::BadSection);
        }
        Ok(image)
    }

    /// Preferred load address.
    pub fn image_base(&self) -> u64 {
        self.image_base
    }

    /// Entry point, relative to the image base.
    pub fn entry_rva(&self) -> u32 {
        self.entry_rva
    }

    /// Entry point virtual address at the preferred base.
    pub fn entry_point(&self) -> u64 {
        self.image_base + self.entry_rva as u64
    }

    /// Total bytes of address space the image occupies once loaded.
    pub fn size_of_image(&self) -> u32 {
        self.size_of_image
    }

    /// Bytes of headers at the start of the file (loaded at RVA 0).
    pub fn size_of_headers(&self) -> u32 {
        self.size_of_headers
    }

    pub fn section_alignment(&self) -> u32 {
        self.section_alignment
    }

    /// Stack size requested by the image (`/STACK` linker option).
    pub fn stack_reserve(&self) -> u64 {
        self.stack_reserve
    }

    pub fn subsystem(&self) -> Subsystem {
        match self.subsystem {
            1 => Subsystem::Native,
            2 => Subsystem::WindowsGui,
            3 => Subsystem::WindowsCui,
            10 => Subsystem::EfiApplication,
            other => Subsystem::Other(other),
        }
    }

    /// The raw file bytes.
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    /// The header bytes, which are mapped at RVA 0.
    pub fn header_bytes(&self) -> &'a [u8] {
        &self.data[..self.size_of_headers as usize]
    }

    /// Iterates over the section table.
    pub fn sections(&self) -> impl Iterator<Item = Section<'a>> + '_ {
        (0..self.num_sections).map(move |i| {
            let off = self.sections_off + i * 40;
            let d = self.data;
            let mut name = [0u8; 8];
            name.copy_from_slice(&d[off..off + 8]);
            let virtual_size = rd_u32(d, off + 8).unwrap_or(0);
            let virtual_address = rd_u32(d, off + 12).unwrap_or(0);
            let raw_size = rd_u32(d, off + 16).unwrap_or(0);
            let raw_offset = rd_u32(d, off + 20).unwrap_or(0);
            let characteristics = rd_u32(d, off + 36).unwrap_or(0);
            // Only the part of the raw data that belongs to the section's
            // virtual size is meaningful; the rest is file-alignment padding.
            let len = raw_size.min(if virtual_size == 0 { raw_size } else { virtual_size }) as usize;
            let data = if raw_size == 0 {
                &d[0..0]
            } else {
                d.get(raw_offset as usize..raw_offset as usize + len).unwrap_or(&d[0..0])
            };
            Section {
                name,
                virtual_address,
                virtual_size: if virtual_size == 0 { raw_size } else { virtual_size },
                raw_offset,
                raw_size,
                characteristics,
                data,
            }
        })
    }

    fn directory(&self, index: usize) -> Option<(u32, u32)> {
        if index >= self.num_dirs {
            return None;
        }
        let off = self.dirs_off + index * 8;
        let rva = rd_u32(self.data, off).ok()?;
        let size = rd_u32(self.data, off + 4).ok()?;
        if rva == 0 || size == 0 { None } else { Some((rva, size)) }
    }

    /// Returns an error if the image depends on DLL imports.
    pub fn check_no_imports(&self) -> Result<(), PeError> {
        match self.directory(DIR_IMPORT) {
            Some(_) => Err(PeError::HasImports),
            None => Ok(()),
        }
    }

    /// Translates an RVA into a slice of file bytes, if it is backed by data.
    fn rva_to_file(&self, rva: u32, len: u32) -> Option<&'a [u8]> {
        for s in self.sections() {
            if rva >= s.virtual_address && rva + len <= s.virtual_address + s.raw_size {
                let off = (s.raw_offset + (rva - s.virtual_address)) as usize;
                return self.data.get(off..off + len as usize);
            }
        }
        None
    }

    /// Returns `true` if the image carries base relocations.
    pub fn has_relocations(&self) -> bool {
        self.directory(DIR_BASERELOC).is_some()
    }

    /// Iterates over every `IMAGE_REL_BASED_DIR64` relocation, yielding the
    /// RVA of the 64-bit word that must be adjusted by `new_base - image_base`.
    pub fn relocations(&self) -> Result<Relocations<'a>, PeError> {
        let block = match self.directory(DIR_BASERELOC) {
            Some((rva, size)) => self.rva_to_file(rva, size).ok_or(PeError::BadRelocation)?,
            None => &[],
        };
        Ok(Relocations { data: block, page_rva: 0, entries: &[] })
    }
}

/// One entry of the section table.
#[derive(Debug, Clone, Copy)]
pub struct Section<'a> {
    pub name: [u8; 8],
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_offset: u32,
    pub raw_size: u32,
    pub characteristics: u32,
    /// Initialized bytes to copy to `virtual_address` (may be shorter than
    /// `virtual_size`; the rest must be zero-filled).
    pub data: &'a [u8],
}

impl Section<'_> {
    /// The section name with trailing NULs removed.
    pub fn name(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(8);
        core::str::from_utf8(&self.name[..len]).unwrap_or("?")
    }

    pub fn readable(&self) -> bool {
        self.characteristics & SCN_MEM_READ != 0
    }

    pub fn writable(&self) -> bool {
        self.characteristics & SCN_MEM_WRITE != 0
    }

    pub fn executable(&self) -> bool {
        self.characteristics & SCN_MEM_EXECUTE != 0
    }

    pub fn discardable(&self) -> bool {
        self.characteristics & SCN_MEM_DISCARDABLE != 0
    }
}

/// Iterator over DIR64 base relocations.
pub struct Relocations<'a> {
    data: &'a [u8],
    page_rva: u32,
    entries: &'a [u8],
}

impl Iterator for Relocations<'_> {
    type Item = Result<u32, PeError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.entries.len() >= 2 {
                let e = u16::from_le_bytes([self.entries[0], self.entries[1]]);
                self.entries = &self.entries[2..];
                match e >> 12 {
                    0 => continue,                                   // ABSOLUTE: padding
                    10 => return Some(Ok(self.page_rva + (e & 0xfff) as u32)), // DIR64
                    _ => return Some(Err(PeError::BadRelocation)),
                }
            }
            if self.data.len() < 8 {
                return None;
            }
            let page = u32::from_le_bytes(self.data[0..4].try_into().unwrap());
            let size = u32::from_le_bytes(self.data[4..8].try_into().unwrap()) as usize;
            if size < 8 || size > self.data.len() {
                self.data = &[];
                return Some(Err(PeError::BadRelocation));
            }
            self.page_rva = page;
            self.entries = &self.data[8..size];
            self.data = &self.data[size..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tiny but valid PE32+ image with one .text section.
    fn tiny_image() -> [u8; 1024] {
        let mut img = [0u8; 1024];
        img[0..2].copy_from_slice(b"MZ");
        img[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        img[0x80..0x84].copy_from_slice(b"PE\0\0");
        let coff = 0x84;
        img[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
        img[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        img[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
        let opt = coff + 20;
        img[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        img[opt + 16..opt + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        img[opt + 24..opt + 32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());
        img[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes());
        img[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        img[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());
        img[opt + 68..opt + 70].copy_from_slice(&1u16.to_le_bytes());
        img[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        let sec = opt + 240;
        img[sec..sec + 5].copy_from_slice(b".text");
        img[sec + 8..sec + 12].copy_from_slice(&0x10u32.to_le_bytes());
        img[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        img[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes());
        img[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes());
        img[sec + 36..sec + 40].copy_from_slice(&(SCN_MEM_READ | SCN_MEM_EXECUTE).to_le_bytes());
        img[0x200] = 0xC3;
        img
    }

    #[test]
    fn parses_minimal_image() {
        let bytes = tiny_image();
        let pe = PeImage::parse(&bytes).unwrap();
        assert_eq!(pe.image_base(), 0x1_4000_0000);
        assert_eq!(pe.entry_point(), 0x1_4000_1000);
        assert_eq!(pe.subsystem(), Subsystem::Native);
        let secs: [Section; 1] = [pe.sections().next().unwrap()];
        assert_eq!(secs[0].name(), ".text");
        assert!(secs[0].executable() && !secs[0].writable());
        assert_eq!(secs[0].data.len(), 0x10);
        assert_eq!(secs[0].data[0], 0xC3);
        assert!(pe.check_no_imports().is_ok());
        assert_eq!(pe.relocations().unwrap().count(), 0);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(PeImage::parse(b"hello").unwrap_err(), PeError::BadSignature);
        let mut bytes = tiny_image();
        bytes[0x84] = 0x4c; // i386 machine
        bytes[0x85] = 0x01;
        assert_eq!(PeImage::parse(&bytes).unwrap_err(), PeError::WrongMachine);
    }
}
