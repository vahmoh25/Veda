extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

/// A segment of a test executable.
struct Seg {
    flags: u32,
    vaddr: u64,
    data: Vec<u8>,
    mem_size: u64,
}

fn seg(flags: u32, vaddr: u64, file: usize, mem: u64) -> Seg {
    Seg { flags, vaddr, data: (0..file).map(|i| (i % 251) as u8 + 1).collect(), mem_size: mem }
}

/// Builds an executable: header and program headers in the first page,
/// segment contents after it. The first segment (if it starts at file
/// offset 0) holds the headers, as with GNU ld.
struct Exe {
    kind: u16,
    machine: u16,
    entry: u64,
    segs: Vec<Seg>,
    /// Other program headers: type, address, memory size.
    extra: Vec<(u32, u64, u64)>,
    headers_in_first: bool,
}

impl Exe {
    fn new(entry: u64, segs: Vec<Seg>) -> Exe {
        Exe { kind: ET_EXEC, machine: EM_X86_64, entry, segs, extra: Vec::new(), headers_in_first: true }
    }

    fn build(&self) -> Vec<u8> {
        let phnum = self.segs.len() + self.extra.len();
        let mut out = vec![0u8; 64 + phnum * PHDR_SIZE];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[6] = 1;
        out[16..18].copy_from_slice(&self.kind.to_le_bytes());
        out[18..20].copy_from_slice(&self.machine.to_le_bytes());
        out[20..24].copy_from_slice(&1u32.to_le_bytes());
        out[24..32].copy_from_slice(&self.entry.to_le_bytes());
        out[32..40].copy_from_slice(&64u64.to_le_bytes());
        out[52..54].copy_from_slice(&64u16.to_le_bytes());
        out[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        out[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
        let mut ph = 64;
        let mut write_ph = |out: &mut Vec<u8>, kind: u32, flags: u32, offset: u64, vaddr: u64, file: u64, mem: u64| {
            let fields = [(0, kind as u64, 4), (4, flags as u64, 4), (8, offset, 8), (16, vaddr, 8), (32, file, 8)];
            for (at, v, n) in fields.into_iter().chain([(40, mem, 8), (48, 4096, 8)]) {
                out[ph + at..ph + at + n].copy_from_slice(&v.to_le_bytes()[..n]);
            }
            ph += PHDR_SIZE;
        };
        for (i, s) in self.segs.iter().enumerate() {
            if i == 0 && self.headers_in_first {
                // The first segment maps the headers too: file offset 0.
                out.extend_from_slice(&s.data);
                let file = out.len() as u64;
                write_ph(&mut out, pt::LOAD, s.flags, 0, s.vaddr, file, s.mem_size.max(file));
            } else {
                let offset = out.len().next_multiple_of(4096) + (s.vaddr % 4096) as usize;
                out.resize(offset, 0);
                out.extend_from_slice(&s.data);
                write_ph(&mut out, pt::LOAD, s.flags, offset as u64, s.vaddr, s.data.len() as u64, s.mem_size);
            }
        }
        for &(kind, vaddr, mem) in &self.extra {
            write_ph(&mut out, kind, pf::R, 0, vaddr, 0, mem);
        }
        out
    }
}

const R: u32 = pf::R;
const RX: u32 = pf::R | pf::X;
const RW: u32 = pf::R | pf::W;

/// The layout of a typical static executable from GNU ld.
fn typical() -> Exe {
    Exe::new(
        0x401040,
        vec![
            seg(R, 0x400000, 0x100, 0),
            seg(RX, 0x401000, 0x1800, 0x1800),
            seg(R, 0x403000, 0x400, 0x400),
            seg(RW, 0x404e10, 0x200, 0x1400),
        ],
    )
}

#[test]
fn parses_a_static_executable() {
    let bytes = typical().build();
    let elf = Elf::parse(&bytes).unwrap();
    assert!(!elf.is_position_independent());
    assert_eq!(elf.entry(), 0x401040);
    assert_eq!(elf.phdr_vaddr(), 0x400040);
    assert_eq!(elf.phnum(), 4);
    assert_eq!(elf.span(), (0x400000, 0x407000));
    let segs: Vec<Segment> = elf.segments().collect();
    assert_eq!(segs.len(), 4);
    assert_eq!(segs[1].vaddr, 0x401000);
    assert_eq!(segs[1].data.len(), 0x1800);
    assert_eq!(segs[1].data[0], 1);
    assert_eq!(segs[3].mem_size, 0x1400);
    assert_eq!(segs[3].perms, Perms { read: true, write: true, exec: false });
    assert_eq!(elf.stack_size(), None);
}

#[test]
fn groups_pages_by_permissions() {
    let bytes = typical().build();
    let runs = page_runs(&Elf::parse(&bytes).unwrap());
    let p = |f| Perms::from_flags(f);
    assert_eq!(
        runs,
        vec![
            PageRun { start: 0x400000, len: 0x1000, perms: p(R) },
            PageRun { start: 0x401000, len: 0x2000, perms: p(RX) },
            PageRun { start: 0x403000, len: 0x1000, perms: p(R) },
            PageRun { start: 0x404000, len: 0x3000, perms: p(RW) },
        ]
    );
}

#[test]
fn shared_pages_get_both_permissions() {
    let exe = Exe::new(0x401000, vec![seg(RX, 0x401000, 0x900, 0x900), seg(RW, 0x401a00, 0x10, 0x1000)]);
    let mut exe = exe;
    exe.headers_in_first = false;
    // The program headers are not in a segment of their own here: PT_PHDR
    // says they are at the start of the code.
    exe.extra.push((pt::PHDR, 0x401000, 0));
    let bytes = exe.build();
    let elf = Elf::parse(&bytes).unwrap();
    let runs = page_runs(&elf);
    let p = |f| Perms::from_flags(f);
    assert_eq!(
        runs,
        vec![
            PageRun { start: 0x401000, len: 0x1000, perms: p(RX | RW) },
            PageRun { start: 0x402000, len: 0x1000, perms: p(RW) },
        ]
    );
}

#[test]
fn position_independent_executables() {
    let mut exe = Exe::new(0x1040, vec![seg(R, 0, 0x100, 0), seg(RX, 0x1000, 0x100, 0x100)]);
    exe.kind = ET_DYN;
    let bytes = exe.build();
    let elf = Elf::parse(&bytes).unwrap();
    assert!(elf.is_position_independent());
    assert_eq!(elf.span(), (0, 0x2000));
}

#[test]
fn stack_size_request() {
    let mut exe = typical();
    exe.extra.push((pt::GNU_STACK, 0, 16 << 20));
    let bytes = exe.build();
    assert_eq!(Elf::parse(&bytes).unwrap().stack_size(), Some(16 << 20));
}

#[test]
fn refuses_what_it_cannot_run() {
    let good = typical().build();
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = good.clone();
        f(&mut b);
        Elf::parse(&b).unwrap_err()
    };
    assert_eq!(Elf::parse(b"MZ\x90\x00").unwrap_err(), ElfError::BadMagic);
    assert_eq!(Elf::parse(&good[..40]).unwrap_err(), ElfError::Truncated);
    assert_eq!(Elf::parse(&good[..100]).unwrap_err(), ElfError::Truncated);
    assert_eq!(with(&|b| b[4] = 1), ElfError::WrongClass);
    assert_eq!(with(&|b| b[5] = 2), ElfError::WrongClass);
    assert_eq!(with(&|b| b[18] = 3), ElfError::WrongMachine);
    assert_eq!(with(&|b| b[16] = 1), ElfError::NotExecutable);
    assert_eq!(with(&|b| b[56] = 0), ElfError::NoSegments);
    assert_eq!(with(&|b| b[54] = 32), ElfError::BadSegment);
    // The entry point in the read-only data.
    assert_eq!(with(&|b| b[24..32].copy_from_slice(&0x403000u64.to_le_bytes())), ElfError::BadEntry);

    let ph = |i: usize, at: usize| 64 + i * PHDR_SIZE + at;
    // A segment's file bytes beyond the end of the file.
    assert_eq!(with(&|b| b[ph(2, 32)..ph(2, 40)].copy_from_slice(&(1u64 << 40).to_le_bytes())), ElfError::BadSegment);
    // More file bytes than memory.
    assert_eq!(with(&|b| b[ph(3, 40)..ph(3, 48)].copy_from_slice(&0x100u64.to_le_bytes())), ElfError::BadSegment);
    // Overlapping segments.
    assert_eq!(with(&|b| b[ph(2, 16)..ph(2, 24)].copy_from_slice(&0x401100u64.to_le_bytes())), ElfError::BadSegment);
    // An address range that wraps around.
    assert_eq!(with(&|b| b[ph(3, 16)..ph(3, 24)].copy_from_slice(&(u64::MAX - 8).to_le_bytes())), ElfError::BadSegment);
    // A huge image.
    assert_eq!(with(&|b| b[ph(3, 40)..ph(3, 48)].copy_from_slice(&(8u64 << 30).to_le_bytes())), ElfError::TooLarge);

    let mut dynamic = typical();
    dynamic.extra.push((pt::INTERP, 0, 0));
    assert_eq!(Elf::parse(&dynamic.build()).unwrap_err(), ElfError::Dynamic);
    // Program headers said to be where nothing is loaded.
    let mut stray = typical();
    stray.extra.push((pt::PHDR, 0x1000_0000, 0));
    assert_eq!(Elf::parse(&stray.build()).unwrap_err(), ElfError::NoSegments);
}

fn word(img: &StackImage, addr: u64) -> u64 {
    let top = img.sp + img.bytes.len() as u64;
    assert!(addr >= img.sp && addr + 8 <= top, "{addr:#x} is outside the stack image");
    let at = (addr - img.sp) as usize;
    u64::from_le_bytes(img.bytes[at..at + 8].try_into().unwrap())
}

fn c_string(img: &StackImage, addr: u64) -> Vec<u8> {
    let at = (addr - img.sp) as usize;
    let len = img.bytes[at..].iter().position(|&b| b == 0).unwrap();
    img.bytes[at..at + len].to_vec()
}

fn aux(execfn: &[u8]) -> AuxInfo<'_> {
    AuxInfo {
        phdr: 0x400040,
        phnum: 4,
        entry: 0x401040,
        hwcap: 0x178bfbff,
        uid: 1000,
        gid: 1000,
        random: *b"0123456789abcdef",
        execfn,
    }
}

#[test]
fn lays_out_the_initial_stack() {
    let top = 0x7fff_0000_0000u64;
    let args: [&[u8]; 3] = [b"cc", b"-o", b"hello world"];
    let env: [&[u8]; 2] = [b"HOME=/home/user", b"PATH=/system/bin"];
    let img = initial_stack(top, &args, &env, &aux(b"/system/bin/cc"), 1 << 20).unwrap();
    assert_eq!(img.sp % 16, 0);
    assert_eq!(img.sp + img.bytes.len() as u64, top);

    let mut p = img.sp;
    let mut next = || {
        let v = word(&img, p);
        p += 8;
        v
    };
    assert_eq!(next(), 3);
    for a in args {
        let ptr = next();
        assert_eq!(c_string(&img, ptr), a);
    }
    assert_eq!(next(), 0);
    for e in env {
        let ptr = next();
        assert_eq!(c_string(&img, ptr), e);
    }
    assert_eq!(next(), 0);
    let mut auxv = Vec::new();
    loop {
        let (k, v) = (next(), next());
        if k == at::NULL {
            break;
        }
        auxv.push((k, v));
    }
    let get = |k| auxv.iter().find(|(kind, _)| *kind == k).map(|&(_, v)| v).unwrap();
    assert_eq!(get(at::PHDR), 0x400040);
    assert_eq!(get(at::PHENT), 56);
    assert_eq!(get(at::PHNUM), 4);
    assert_eq!(get(at::PAGESZ), 4096);
    assert_eq!(get(at::ENTRY), 0x401040);
    assert_eq!(get(at::UID), 1000);
    assert_eq!(get(at::EGID), 1000);
    assert_eq!(get(at::SECURE), 0);
    assert_eq!(c_string(&img, get(at::PLATFORM)), b"x86_64");
    assert_eq!(c_string(&img, get(at::EXECFN)), b"/system/bin/cc");
    let random = get(at::RANDOM);
    assert_eq!(random, top - 16);
    assert_eq!(&img.bytes[(random - img.sp) as usize..], b"0123456789abcdef");
}

#[test]
fn the_stack_has_a_size_limit() {
    let big = vec![b'x'; 5000];
    let args: [&[u8]; 2] = [b"prog", &big];
    assert_eq!(initial_stack(1 << 40, &args, &[], &aux(b"/p"), 4096).unwrap_err(), ElfError::TooLarge);
    assert!(initial_stack(1 << 40, &args, &[], &aux(b"/p"), 8192).is_ok());
    // No arguments at all still gives a valid (empty) vector.
    let img = initial_stack(1 << 40, &[], &[], &aux(b""), 4096).unwrap();
    assert_eq!(word(&img, img.sp), 0);
    assert_eq!(word(&img, img.sp + 8), 0);
    assert_eq!(word(&img, img.sp + 16), 0);
}
