use crate::device;
use crate::gtt::Gtt;
use crate::regs::{self, Pipe};

#[test]
fn registers_where_i915_has_them() {
    // Pipe A's and B's plane 1 and 2 (`_PLANE_CTL_1_A` 0x70180, `_2_A`
    // 0x70280, `_1_B` 0x71180).
    assert_eq!(regs::plane_ctl(Pipe(0), 1), 0x70180);
    assert_eq!(regs::plane_ctl(Pipe(0), 2), 0x70280);
    assert_eq!(regs::plane_ctl(Pipe(1), 1), 0x71180);
    assert_eq!(regs::plane_stride(Pipe(0), 1), 0x70188);
    assert_eq!(regs::plane_size(Pipe(0), 1), 0x70190);
    assert_eq!(regs::plane_surf(Pipe(0), 1), 0x7019C);
    assert_eq!(regs::plane_offset(Pipe(0), 1), 0x701A4);
    assert_eq!(regs::plane_surf_live(Pipe(0), 1), 0x701AC);
    assert_eq!(regs::transconf(Pipe(2)), 0x72008);
    assert_eq!(regs::pipesrc(Pipe(1)), 0x6101C);
    assert_eq!(regs::frame_count(Pipe(0)), 0x70040);
    assert_eq!(regs::scaler_ctl(Pipe(1), 1), 0x68A80);
    assert_eq!(regs::pipe_iir(Pipe(2)), 0x44428);
    assert_eq!(regs::display_int_pipe(Pipe(2)), 1 << 18);
    // Cursors (`_CURACNTR` 0x70080, `_CURBBASE_IVB` 0x71084).
    assert_eq!(regs::cursor_ctl(Pipe(0)), 0x70080);
    assert_eq!(regs::cursor_base(Pipe(1)), 0x71084);
    // XRGB 8888 is 4 in Skylake's format field (bits 27:24).
    assert_eq!(regs::plane_format(4 << 24), regs::FORMAT_8888);
}

#[test]
fn knows_the_gpus() {
    let zenbook = device::platform(0x46A6).expect("Alder Lake-P's Iris Xe");
    assert_eq!((zenbook.name, zenbook.display, zenbook.pipes), ("Alder Lake-P", 13, 4));
    assert_eq!(device::platform(0x9A49).map(|p| p.display), Some(12));
    assert_eq!(device::platform(0x4C8A).map(|p| p.pipes), Some(3));
    // Meteor Lake's display engine (version 14) works differently.
    assert_eq!(device::platform(0x7D55), None);
}

#[test]
fn the_tables_size() {
    // GGMS 3: 8 MiB of entries, in the upper half of a 16 MiB BAR.
    let gtt = Gtt::new(16 << 20, 3 << 6).unwrap();
    assert_eq!((gtt.base, gtt.entries, gtt.span()), (8 << 20, 1 << 20, 4 << 30));
    // A table that does not fit in the BAR, or none at all.
    assert_eq!(Gtt::new(8 << 20, 3 << 6), None);
    assert_eq!(Gtt::new(16 << 20, 0), None);
}

mod gem {
    //! The GEM model on a GPU that runs only when told.

    use alloc::vec;
    use alloc::vec::Vec;

    use crate::gem::{EngineId, Gem, GemError, Hardware, Submission, Use};

    const RENDER: EngineId = EngineId { class: 0, instance: 0 };
    const COPY: EngineId = EngineId { class: 1, instance: 0 };

    /// A GPU whose engines finish what they ran when the test says.
    struct Model {
        engines: Vec<EngineId>,
        done: Vec<u64>,
        /// What ran: (engine, batch address, length, seqno).
        runs: Vec<(usize, u64, u32, u64)>,
        /// Spaces and contexts there are, and contexts ever made.
        spaces: usize,
        contexts: usize,
        serial: u32,
    }

    impl Hardware for Model {
        /// A buffer's size.
        type Memory = u64;
        /// What is mapped: (address, size).
        type Space = Vec<(u64, u64)>;
        /// Its engine, and a serial number.
        type Context = (usize, u32);

        fn engines(&self) -> &[EngineId] {
            &self.engines
        }

        fn space_size(&self) -> u64 {
            1 << 48
        }

        fn size(&self, memory: &u64) -> u64 {
            *memory
        }

        fn new_space(&mut self) -> Result<Vec<(u64, u64)>, GemError> {
            self.spaces += 1;
            Ok(Vec::new())
        }

        fn drop_space(&mut self, space: Vec<(u64, u64)>) {
            assert!(space.is_empty(), "a space given back with buffers bound");
            self.spaces -= 1;
        }

        fn bind(&mut self, space: &mut Vec<(u64, u64)>, address: u64, memory: &u64) -> Result<(), GemError> {
            assert!(space.iter().all(|&(a, s)| a + s <= address || address + memory <= a), "a binding overlaps");
            space.push((address, *memory));
            Ok(())
        }

        fn unbind(&mut self, space: &mut Vec<(u64, u64)>, address: u64, size: u64) {
            let i = space.iter().position(|&m| m == (address, size)).expect("unbinding what is bound");
            space.swap_remove(i);
        }

        fn new_context(&mut self, engine: usize, _: &Vec<(u64, u64)>) -> Result<(usize, u32), GemError> {
            self.contexts += 1;
            self.serial += 1;
            Ok((engine, self.serial))
        }

        fn drop_context(&mut self, _: (usize, u32)) {
            self.contexts -= 1;
        }

        fn run(&mut self, context: &mut (usize, u32), _: &Vec<(u64, u64)>, batch: u64, len: u32, seqno: u64) {
            self.runs.push((context.0, batch, len, seqno));
        }

        fn completed(&self, engine: usize) -> u64 {
            self.done[engine]
        }
    }

    /// A client with an address space and a context on both engines.
    fn setup() -> (Gem<Model>, u32) {
        let mut g = Gem::new(Model {
            engines: vec![RENDER, COPY],
            done: vec![0, 0],
            runs: Vec::new(),
            spaces: 0,
            contexts: 0,
            serial: 0,
        });
        g.connect(1);
        let space = g.space_create(1).unwrap();
        let ctx = g.context_create(1, space, &[RENDER, COPY]).unwrap();
        (g, ctx)
    }

    fn submit(ctx: u32, engine: u32, buffers: &[(u32, u64)], waits: &[(u32, u64)]) -> Submission {
        Submission {
            context: ctx,
            engine,
            buffers: buffers.iter().map(|&(handle, address)| Use { handle, address }).collect(),
            batch_start: 0,
            batch_len: 64,
            waits: waits.to_vec(),
        }
    }

    #[test]
    fn numbers_submissions_per_engine_and_binds_their_buffers() {
        let (mut g, ctx) = setup();
        let batch = g.create(1, 4096).unwrap();
        let target = g.create(1, 8192).unwrap();
        let s = submit(ctx, 0, &[(batch, 0x10000), (target, 0x20000)], &[]);
        assert_eq!(g.execute(1, &s), Ok((0, 1)));
        assert_eq!(g.execute(1, &s), Ok((0, 2)));
        let copy = submit(ctx, 1, &[(batch, 0x10000)], &[]);
        assert_eq!(g.execute(1, &copy), Ok((1, 1)));
        assert_eq!(g.hardware().runs, vec![(0, 0x10000, 64, 1), (0, 0x10000, 64, 2), (1, 0x10000, 64, 1)]);
        // A buffer no submission names is refused, and so is a context
        // engine the context does not have.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(99, 0x10000)], &[])), Err(GemError::Invalid));
        assert_eq!(g.execute(1, &submit(ctx, 2, &[(batch, 0x10000)], &[])), Err(GemError::Invalid));
    }

    #[test]
    fn refuses_buffers_where_others_are() {
        let (mut g, ctx) = setup();
        let a = g.create(1, 8192).unwrap();
        let b = g.create(1, 4096).unwrap();
        // Overlapping in one submission.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(a, 0x10000), (b, 0x11000)], &[])), Err(GemError::Invalid));
        // Overlapping what an earlier one bound.
        g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])).unwrap();
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(b, 0x11000)], &[])), Err(GemError::Invalid));
        // Unaligned, or past the end of the space.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(b, 0x30010)], &[])), Err(GemError::Invalid));
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(b, (1 << 48) - 0x800)], &[])), Err(GemError::Invalid));
    }

    #[test]
    fn keeps_a_closed_buffer_bound_until_the_gpu_is_done() {
        let (mut g, ctx) = setup();
        let a = g.create(1, 4096).unwrap();
        let b = g.create(1, 4096).unwrap();
        g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])).unwrap();
        g.close(1, a);
        // Its address is still taken.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(b, 0x10000)], &[])), Err(GemError::Invalid));
        g.hardware_mut().done[0] = 1;
        g.poll();
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(b, 0x10000)], &[])), Ok((0, 2)));
    }

    #[test]
    fn runs_a_submission_once_what_it_waits_for_is_done() {
        let (mut g, ctx) = setup();
        let a = g.create(1, 4096).unwrap();
        g.execute(1, &submit(ctx, 1, &[(a, 0x10000)], &[])).unwrap();
        // The render engine waits for the copy engine's first.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[(1, 1)])), Ok((0, 1)));
        assert_eq!(g.hardware().runs.len(), 1);
        // And the next on the render engine after it, in order.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])), Ok((0, 2)));
        assert_eq!(g.hardware().runs.len(), 1);
        g.hardware_mut().done[1] = 1;
        g.poll();
        let runs: Vec<_> = g.hardware().runs.iter().map(|r| (r.0, r.3)).collect();
        assert_eq!(runs, vec![(1, 1), (0, 1), (0, 2)]);
        // Waiting for what was never given out is refused.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[(1, 9)])), Err(GemError::Invalid));
    }

    #[test]
    fn forgets_a_space_once_the_gpu_is_done_in_it() {
        let (mut g, ctx) = setup();
        let a = g.create(1, 4096).unwrap();
        let other = g.space_create(1).unwrap();
        let ctx2 = g.context_create(1, other, &[RENDER]).unwrap();
        g.execute(1, &submit(ctx2, 0, &[(a, 0x10000)], &[])).unwrap();
        // Not while a context is in it.
        assert_eq!(g.space_destroy(1, other), Err(GemError::Invalid));
        g.context_destroy(1, ctx2);
        assert_eq!(g.space_destroy(1, other), Ok(()));
        // Gone for new contexts at once; still mapped for the GPU.
        assert_eq!(g.context_create(1, other, &[RENDER]), Err(GemError::Invalid));
        assert_eq!(g.space_destroy(1, other), Err(GemError::Invalid));
        g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])).unwrap();
        assert_eq!(g.hardware().runs.len(), 2);
        g.hardware_mut().done[0] = 2;
        g.poll();
        // Unbound from it alone: the buffer stays where the other space has it.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])), Ok((0, 3)));
    }

    #[test]
    fn gives_contexts_and_spaces_back_once_the_gpu_is_done() {
        let (mut g, ctx) = setup();
        // The context's state on each of its engines.
        assert_eq!(g.hardware().contexts, 2);
        let a = g.create(1, 4096).unwrap();
        g.execute(1, &submit(ctx, 1, &[(a, 0x10000)], &[])).unwrap();
        // Busy: it stays, but takes nothing more.
        g.context_destroy(1, ctx);
        assert_eq!(g.hardware().contexts, 2);
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])), Err(GemError::Invalid));
        g.hardware_mut().done[1] = 1;
        g.poll();
        assert_eq!(g.hardware().contexts, 0);
        // A client that goes leaves nothing behind once the GPU is done.
        let space = g.space_create(1).unwrap();
        let ctx = g.context_create(1, space, &[COPY]).unwrap();
        g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])).unwrap();
        g.disconnect(1);
        assert_eq!((g.hardware().spaces, g.hardware().contexts), (2, 1));
        g.hardware_mut().done[1] = 2;
        g.poll();
        assert_eq!((g.hardware().spaces, g.hardware().contexts), (0, 0));
    }

    #[test]
    fn forgets_a_client_once_the_gpu_is_done_with_its_buffers() {
        let (mut g, ctx) = setup();
        let a = g.create(1, 4096).unwrap();
        g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])).unwrap();
        g.disconnect(1);
        assert_eq!(g.clients(), 0);
        // Still there for the GPU: its space keeps the buffer mapped.
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(a, 0x10000)], &[])), Err(GemError::Invalid));
        g.hardware_mut().done[0] = 1;
        g.poll();
        g.connect(1);
        assert_eq!(g.clients(), 1);
    }
}

mod ppgtt {
    //! The page tables, written into a model of memory.

    use std::collections::BTreeMap;
    use std::vec;
    use std::vec::Vec;

    use crate::ppgtt::{ADDRESS_MASK, AddressSpace, Error, PRESENT, TableMemory, pat_bits};

    #[derive(Default)]
    struct Memory {
        tables: BTreeMap<u64, Vec<u64>>,
        next: u64,
        /// A limit on the tables there may be.
        room: Option<usize>,
    }

    impl TableMemory for Memory {
        fn alloc(&mut self) -> Option<u64> {
            if self.room.is_some_and(|r| self.tables.len() >= r) {
                return None;
            }
            self.next += 0x1000;
            self.tables.insert(0x1_0000_0000 + self.next, vec![0; 512]);
            Some(0x1_0000_0000 + self.next)
        }

        fn free(&mut self, table: u64) {
            self.tables.remove(&table).expect("freeing a table there is");
        }

        fn write(&mut self, table: u64, index: usize, entry: u64) {
            self.tables.get_mut(&table).expect("writing a table there is")[index] = entry;
        }
    }

    impl Memory {
        /// What the GPU would find at `address`, walking from `root`: the
        /// page's entry.
        fn walk(&self, root: u64, address: u64) -> Option<u64> {
            let mut table = root;
            for level in (1..=4).rev() {
                let i = ((address >> (12 + 9 * (level - 1))) & 511) as usize;
                let entry = self.tables.get(&table)?[i];
                if entry & PRESENT == 0 {
                    return None;
                }
                if level == 1 {
                    return Some(entry);
                }
                table = entry & ADDRESS_MASK;
            }
            None
        }
    }

    /// The scratch page and tables of a new space.
    const SCRATCH: usize = 4;

    #[test]
    fn maps_pages_where_the_gpu_finds_them() {
        let mut m = Memory::default();
        let mut space = AddressSpace::new(&mut m).unwrap();
        let scratch = space.scratch_page() | 0b11;
        // Nothing mapped: every address reaches the scratch page.
        assert_eq!(m.walk(space.root(), 0x1234_5000), Some(scratch));
        // Across the boundary of a page table, a directory and a PDP.
        let at = (1 << 39) - 0x2000;
        space.map(&mut m, at, &[0x5000, 0x7000, 0x9000, 0xB000], pat_bits(3)).unwrap();
        let entry = |a: u64| m.walk(space.root(), a);
        assert_eq!(entry(at).map(|e| e & ADDRESS_MASK), Some(0x5000));
        assert_eq!(entry(at + 0x3000).map(|e| e & ADDRESS_MASK), Some(0xB000));
        // Writable, present, and PAT index 3 (bits 3 and 4).
        assert_eq!(entry(at + 0x1000), Some(0x7000 | 0b11011));
        assert_eq!(entry(at + 0x4000), Some(scratch));
        assert_eq!(entry(at - 0x1000), Some(scratch));
        // A root, two branches of three tables below it.
        assert_eq!(m.tables.len(), SCRATCH + 7);
        assert_eq!(pat_bits(4), 1 << 7);
    }

    #[test]
    fn frees_the_tables_left_empty() {
        let mut m = Memory::default();
        let mut space = AddressSpace::new(&mut m).unwrap();
        let scratch = space.scratch_page() | 0b11;
        space.map(&mut m, 0x10000, &[0x1000, 0x2000], 0).unwrap();
        space.map(&mut m, 0x4000_0000, &[0x3000], 0).unwrap();
        assert_eq!(m.tables.len(), SCRATCH + 6);
        // Unmapping what was never mapped changes nothing.
        space.unmap(&mut m, 0x20000, 4);
        assert_eq!(m.tables.len(), SCRATCH + 6);
        space.unmap(&mut m, 0x10000, 1);
        assert_eq!(m.walk(space.root(), 0x10000), Some(scratch));
        assert_eq!(m.walk(space.root(), 0x11000), Some(0x2003));
        assert_eq!(m.tables.len(), SCRATCH + 6);
        // The last page of a table: it goes, with the directory above it.
        space.unmap(&mut m, 0x11000, 1);
        assert_eq!(m.tables.len(), SCRATCH + 4);
        assert_eq!(m.walk(space.root(), 0x11000), Some(scratch));
        space.unmap(&mut m, 0x4000_0000, 1);
        assert_eq!(m.tables.len(), SCRATCH + 1);
        assert_eq!(m.walk(space.root(), 0x4000_0000), Some(scratch));
        space.destroy(&mut m);
        assert!(m.tables.is_empty());
    }

    #[test]
    fn refuses_what_does_not_fit() {
        let mut m = Memory::default();
        let mut space = AddressSpace::new(&mut m).unwrap();
        assert_eq!(space.map(&mut m, 0x800, &[0x1000], 0), Err(Error::Invalid));
        assert_eq!(space.map(&mut m, (1 << 48) - 0x1000, &[0x1000, 0x2000], 0), Err(Error::Invalid));
        // No room for the tables below the root.
        m.room = Some(SCRATCH + 2);
        assert_eq!(space.map(&mut m, 0, &[0x1000], 0), Err(Error::NoMemory));
        space.unmap(&mut m, 0, 1);
        space.destroy(&mut m);
        assert!(m.tables.is_empty());
        // Nor for a space at all.
        m.room = Some(2);
        assert!(AddressSpace::new(&mut m).is_err());
        assert!(m.tables.is_empty());
    }
}

mod lrc {
    //! Contexts' images and requests, against i915's layout.

    use std::vec;

    use crate::lrc::{self, Emitter, Kind, Placement, cmd, ctx, mi};

    fn image(kind: Kind) -> std::vec::Vec<u32> {
        let mut image = vec![0u32; ((kind.image_pages() + 2) * lrc::PAGE / 4) as usize];
        let at = Placement { image: 0x40_0000, ring: 0x50_0000, root: 0x1_2345_6000 };
        lrc::init_image(&mut image, kind, at, 1, None);
        image
    }

    #[test]
    fn registers_are_where_i915_has_them() {
        let image = image(Kind::Render);
        let regs = &image[1024..2048];
        let base = 0x2000;
        // Each value's register just before it (`CTX_*`, `lrc_ring_*`).
        for (value, reg) in [
            (ctx::CONTEXT_CONTROL, 0x244),
            (ctx::RING_HEAD, 0x34),
            (ctx::RING_TAIL, 0x30),
            (ctx::RING_START, 0x38),
            (ctx::RING_CTL, 0x3C),
            (ctx::BB_STATE, 0x110),
            (ctx::PER_CTX_BB, 0x1C0),
            (ctx::INDIRECT_CTX, 0x1C4),
            (ctx::INDIRECT_CTX_OFFSET, 0x1C8),
            (ctx::TIMESTAMP, 0x3A8),
            (ctx::PDP0_UDW, 0x274),
            (ctx::PDP0_LDW, 0x270),
            (ctx::R_PWR_CLK_STATE, 0xC8),
            (ctx::MI_MODE, 0x9C),
            (ctx::BB_OFFSET, 0x158),
            (ctx::GPR0, 0x600),
            (ctx::CMD_BUF_CCTL, 0x84),
        ] {
            assert_eq!(regs[value - 1], base + reg, "the register of value {value}");
        }
        // The loads: 13 posted, 9 posted, 3 posted, 1, 51 posted, relative
        // to the engine.
        let lri = |n: u32, posted: bool| {
            cmd::mi_load_register_imm(n) | cmd::MI_LRI_LRM_CS_MMIO | if posted { cmd::MI_LRI_FORCE_POSTED } else { 0 }
        };
        assert_eq!(
            (regs[1], regs[33], regs[52], regs[65], regs[81]),
            (lri(13, true), lri(9, true), lri(3, true), lri(1, false), lri(51, true))
        );
        // A new context: no restore, its ring, its space, its slices.
        assert_eq!(regs[ctx::CONTEXT_CONTROL], 0x0009_0009);
        assert_eq!(regs[ctx::RING_START], 0x50_0000);
        assert_eq!(regs[ctx::RING_CTL], (16 * 1024 - 4096) | 1);
        assert_eq!((regs[ctx::PDP0_UDW], regs[ctx::PDP0_LDW]), (0x1, 0x2345_6000));
        assert_eq!(regs[ctx::R_PWR_CLK_STATE], 0x8004_1000);
        assert_eq!(regs[ctx::MI_MODE], 0x0100_0000);
        // The workaround batches after the image (14 pages): the indirect
        // context's address and cache lines, and the per-context batch.
        let wa = 0x40_0000 + 14 * 4096;
        assert_eq!(regs[ctx::INDIRECT_CTX] & !0x3F, wa);
        assert!(regs[ctx::INDIRECT_CTX] & 0x3F > 0);
        assert_eq!(regs[ctx::INDIRECT_CTX_OFFSET], 0xD << 6);
        assert_eq!(regs[ctx::PER_CTX_BB], (wa + 4096) | 0b101);
        assert_eq!(image[(14 + 1) * 1024], cmd::MI_BATCH_BUFFER_END);
    }

    #[test]
    fn the_copy_engine_has_fewer() {
        let image = image(Kind::Copy);
        let regs = &image[1024..2048];
        assert_eq!(regs[ctx::RING_TAIL - 1], 0x22030);
        assert_eq!(regs[ctx::PDP0_LDW - 1], 0x22270);
        // The golden image ends after the address space's registers.
        assert_eq!(regs[52], cmd::MI_BATCH_BUFFER_END | 1);
        assert_eq!(regs[ctx::R_PWR_CLK_STATE], 0);
    }

    #[test]
    fn contexts_start_from_the_golden_image() {
        // The golden image as the engine saved it: where its ring stopped,
        // its mode stopped with another bit on, the engine's state.
        let mut saved = image(Kind::Render);
        saved[1024 + ctx::RING_HEAD] = 0x1C0;
        saved[1024 + ctx::MI_MODE] = 0xFFFF_0000 | (1 << 8) | (1 << 4);
        saved[1024 + 300] = 0xABCD;
        let mut copy = vec![0u32; saved.len()];
        let at = Placement { image: 0x60_0000, ring: 0x70_0000, root: 0x2000 };
        lrc::init_image(&mut copy, Kind::Render, at, 1, Some(&saved));
        let regs = &copy[1024..2048];
        // Loaded whole, its own ring and space, the engine's state kept.
        assert_eq!(regs[ctx::CONTEXT_CONTROL], 0x0009_0008);
        assert_eq!((regs[ctx::RING_HEAD], regs[ctx::RING_START], regs[ctx::PDP0_LDW]), (0, 0x70_0000, 0x2000));
        assert_eq!(regs[ctx::MI_MODE], 0xFFFF_0010);
        assert_eq!(regs[300], 0xABCD);
        assert_eq!(regs[ctx::INDIRECT_CTX] & !0x3F, 0x60_0000 + 14 * 4096);
    }

    #[test]
    fn descriptors() {
        // Valid, legacy 64-bit addressing, privileged, normal priority,
        // restored whole (no L3 coherence bit: Gen8 only); tag 0 is
        // context ID 1.
        let d = lrc::descriptor(0x40_0000, 0, 0, 0, true, true);
        assert_eq!(d as u32, 0x40_0000 | 0x31D);
        assert_eq!((d >> 32) as u32, 1 << 5);
        // The copy engine: class 1, no priority.
        let d = lrc::descriptor(0x40_0000, 4, 1, 0, false, false);
        assert_eq!(d as u32, 0x40_0000 | 0x119);
        assert_eq!((d >> 32) as u32, (5 << 5) | (1 << 29));
    }

    #[test]
    fn a_request_runs_the_batch_then_writes_its_number() {
        for kind in [Kind::Render, Kind::Copy] {
            let mut words = [0u32; lrc::MAX_REQUEST_WORDS];
            let mut e = Emitter::new(&mut words);
            lrc::request(&mut e, kind, Some(0x1_0000_2000), 7, 0x40_1100, None);
            let n = e.len();
            assert_eq!(n % 2, 0);
            let w = &words[..n];
            let bb = w.iter().position(|&x| x == cmd::MI_BATCH_BUFFER_START | cmd::MI_BATCH_PPGTT).unwrap();
            assert_eq!((w[bb + 1], w[bb + 2]), (0x2000, 0x1));
            // The number, after the batch, then the interrupt.
            let at = w.iter().position(|&x| x == 0x40_1100 || x == 0x40_1100 | cmd::MI_FLUSH_DW_USE_GTT).unwrap();
            assert!(at > bb);
            assert!(w[at..at + 3].contains(&7));
            let irq = w.iter().position(|&x| x == cmd::MI_USER_INTERRUPT).unwrap();
            assert!(irq > at);
            assert_eq!(w[n - 2..], [cmd::MI_ARB_CHECK, cmd::MI_NOOP]);
        }
        // The golden context's: its workarounds, loaded between flushes.
        let first = crate::render::golden_request(Kind::Render, 0x100);
        let lri = first.iter().position(|&x| x == cmd::mi_load_register_imm(5)).unwrap();
        assert_eq!(first[lri + 1], 0x2580);
        let first = crate::render::golden_request(Kind::Copy, 0x100);
        let lri = first.iter().position(|&x| x == cmd::mi_load_register_imm(1)).unwrap();
        assert!(!first.contains(&(cmd::MI_BATCH_BUFFER_START | cmd::MI_BATCH_PPGTT)));
        // BLIT_CCTL: MOCS 3 for both.
        assert_eq!((first[lri + 1], first[lri + 2]), (0x22204, (6 << 8) | 6));
        let _ = mi(0, 0);
    }
}
