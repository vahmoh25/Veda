//! The GPU's engines driven through the GEM model, as `intel-gpu` drives
//! them: `vigpu::render` on the model of the GT.

use vigpu::gem::{EngineId, Gem, GemError, Submission, Use};
use vigpu::gt::{self, Topology};
use vigpu::gtt::Gtt;
use vigpu::render::{BufferMemory, HANG_NS, Region, Render, Setup, Time};

use crate::gt::{Gt, Memory, SimDma, SimRegion, SimTime};

const GTT_BASE: u32 = 8 << 20;
const RENDER: EngineId = EngineId { class: 0, instance: 0 };
const COPY: EngineId = EngineId { class: 1, instance: 0 };

/// MI_STORE_DATA_IMM of `value` at `address`, then the batch's end.
fn store(address: u64, value: u32) -> [u32; 6] {
    [0x1000_0002, address as u32, (address >> 32) as u32, value, 0x0500_0000, 0]
}

type G<'a> = Gem<Render<&'a Gt, SimDma>>;

fn render(gt: &Gt) -> G<'_> {
    let gtt = Gtt { base: GTT_BASE, entries: 1 << 20 };
    let setup =
        Setup { gtt, ggtt_start: 1 << 30, ggtt_bytes: 256 << 20, topology: Topology::read(gt), interrupts: true };
    assert!(gt::forcewake_get(gt, vigpu::gtregs::FORCEWAKE_GT));
    assert!(gt::forcewake_get(gt, vigpu::gtregs::FORCEWAKE_RENDER));
    let r = Render::new(gt, SimDma(gt.memory.clone()), setup, &SimTime::default()).expect("the GT comes up");
    let mut g = Gem::new(r);
    g.connect(1);
    g
}

/// A buffer of `pages`, its first words `words`; its first page's physical
/// address.
fn buffer(g: &mut G, mem: &Memory, pages: u32, words: &[u32]) -> (u32, u64) {
    let mut r: SimRegion = mem.region(pages);
    r.words()[..words.len()].copy_from_slice(words);
    let phys = r.phys();
    let memory = BufferMemory { pages: (0..u64::from(pages)).map(|i| phys + i * 4096).collect(), keep: r };
    (g.create(1, memory).unwrap(), phys)
}

fn submit(ctx: u32, engine: u32, buffers: &[(u32, u64)], len: u32) -> Submission {
    Submission {
        context: ctx,
        engine,
        buffers: buffers.iter().map(|&(handle, address)| Use { handle, address }).collect(),
        batch_start: 0,
        batch_len: len,
        waits: Vec::new(),
    }
}

fn no_faults(gt: &Gt) {
    assert_eq!(gt.faults.borrow().as_slice(), &[] as &[String]);
}

#[test]
fn runs_a_batch_where_the_driver_mapped_it() {
    let mem = Memory::default();
    let gt = Gt::new(mem.clone(), GTT_BASE);
    let mut g = render(&gt);
    let space = g.space_create(1).unwrap();
    let ctx = g.context_create(1, space, &[RENDER, COPY]).unwrap();
    // A target across the boundary of a page table, three pages long.
    let target_at = 0x7F_FFFF_F000;
    let (batch, _) = buffer(&mut g, &mem, 1, &store(target_at + 0x1004, 0xC0FFEE));
    let (target, target_phys) = buffer(&mut g, &mem, 3, &[]);
    assert_eq!(g.execute(1, &submit(ctx, 0, &[(batch, 0x10000), (target, target_at)], 24)), Ok((0, 1)));
    let events = g.hardware_mut().poll(1);
    assert!(events.moved);
    assert_eq!(g.completed(0), 1);
    no_faults(&gt);
    assert_eq!(gt.batches(0), vec![0x10000]);
    // The second page of the target, as the batch wrote it.
    assert_eq!(mem.read(target_phys + 0x1004), Some(0xC0FFEE));
    // Only the golden context loaded without its state; the others whole.
    assert_eq!(gt.fresh_loads(0), 1);
    // An interrupt, which the driver takes.
    assert!(gt.interrupting());
    let master = vigpu::Mmio::read(&gt, vigpu::regs::MASTER_IRQ);
    let cause = gt::handle_gt_interrupt(&gt, master);
    assert_eq!(cause.render & vigpu::gtregs::GT_RENDER_USER_INTERRUPT as u16, 1);
    assert!(!gt.interrupting());
}

#[test]
fn submissions_go_one_at_a_time() {
    let mem = Memory::default();
    let gt = Gt::new(mem.clone(), GTT_BASE);
    let mut g = render(&gt);
    let space = g.space_create(1).unwrap();
    let ctx = g.context_create(1, space, &[RENDER, COPY]).unwrap();
    let (target, phys) = buffer(&mut g, &mem, 1, &[]);
    let mut batches = Vec::new();
    for i in 0..3u32 {
        let (b, _) = buffer(&mut g, &mem, 1, &store(0x20000 + u64::from(i) * 4, 10 + i));
        batches.push(b);
    }
    for (i, &b) in batches.iter().enumerate() {
        let at = 0x40000 + i as u64 * 0x1000;
        assert_eq!(g.execute(1, &submit(ctx, 0, &[(b, at), (target, 0x20000)], 24)), Ok((0, i as u64 + 1)));
    }
    // The first ran at once; the others wait for it.
    assert_eq!(gt.batches(0).len(), 1);
    for _ in 0..3 {
        g.hardware_mut().poll(1);
    }
    assert_eq!(gt.batches(0), vec![0x40000, 0x41000, 0x42000]);
    assert_eq!(g.completed(0), 3);
    assert_eq!((0..3).map(|i| mem.read(phys + i * 4).unwrap()).collect::<Vec<_>>(), vec![10, 11, 12]);
    // Still only the golden context loaded without its state.
    assert_eq!(gt.fresh_loads(0), 1);
    // The copy engine runs its own, numbered apart.
    let (b, _) = buffer(&mut g, &mem, 1, &store(0x20010, 99));
    assert_eq!(g.execute(1, &submit(ctx, 1, &[(b, 0x50000), (target, 0x20000)], 24)), Ok((1, 1)));
    g.hardware_mut().poll(1);
    assert_eq!(g.completed(1), 1);
    assert_eq!(mem.read(phys + 0x10), Some(99));
    no_faults(&gt);
}

#[test]
fn the_ring_wraps() {
    let mem = Memory::default();
    let gt = Gt::new(mem.clone(), GTT_BASE);
    let mut g = render(&gt);
    let space = g.space_create(1).unwrap();
    let ctx = g.context_create(1, space, &[RENDER]).unwrap();
    let (target, phys) = buffer(&mut g, &mem, 1, &[]);
    // Far more submissions than the ring holds at once.
    for i in 0..400u32 {
        let (b, _) = buffer(&mut g, &mem, 1, &store(0x20000, i));
        g.execute(1, &submit(ctx, 0, &[(b, 0x10000), (target, 0x20000)], 24)).unwrap();
        g.hardware_mut().poll(1);
        g.poll();
        g.close(1, b);
    }
    assert_eq!(g.completed(0), 400);
    assert_eq!(mem.read(phys), Some(399));
    no_faults(&gt);
}

#[test]
fn a_hang_resets_the_engine_and_loses_the_context() {
    let mem = Memory::default();
    let gt = Gt::new(mem.clone(), GTT_BASE);
    let mut g = render(&gt);
    let space = g.space_create(1).unwrap();
    let ctx = g.context_create(1, space, &[RENDER]).unwrap();
    let (target, phys) = buffer(&mut g, &mem, 1, &[]);
    let (b, _) = buffer(&mut g, &mem, 1, &store(0x20000, 7));
    let resets = gt.resets();
    gt.hang_next(0);
    g.execute(1, &submit(ctx, 0, &[(b, 0x10000), (target, 0x20000)], 24)).unwrap();
    // Running, not yet hung.
    assert!(!g.hardware_mut().poll(HANG_NS / 2).moved);
    assert_eq!(g.completed(0), 0);
    let events = g.hardware_mut().poll(HANG_NS * 2);
    assert_eq!(events.hangs.len(), 1);
    assert!(gt.resets() > resets);
    // Done (lost), and its context with it.
    assert_eq!(g.completed(0), 1);
    assert_eq!(mem.read(phys), Some(0));
    assert_eq!(g.execute(1, &submit(ctx, 0, &[(b, 0x10000), (target, 0x20000)], 24)), Err(GemError::Lost));
    assert_eq!(g.reset_stats(1, ctx), Ok((1, 0)));
    // Another context runs on the engine again.
    let ctx2 = g.context_create(1, space, &[RENDER]).unwrap();
    assert_eq!(g.execute(1, &submit(ctx2, 0, &[(b, 0x10000), (target, 0x20000)], 24)), Ok((0, 2)));
    g.hardware_mut().poll(HANG_NS * 2 + 1);
    assert_eq!(g.completed(0), 2);
    assert_eq!(mem.read(phys), Some(7));
}

#[test]
fn an_engine_that_runs_nothing_is_left_alone() {
    let mem = Memory::default();
    let gt = Gt::new(mem.clone(), GTT_BASE);
    gt.kill(1);
    let gtt = Gtt { base: GTT_BASE, entries: 1 << 20 };
    let setup =
        Setup { gtt, ggtt_start: 1 << 30, ggtt_bytes: 256 << 20, topology: Topology::read(&gt), interrupts: false };
    let time = SimTime::default();
    let e = Render::new(&gt, SimDma(mem.clone()), setup, &time).err().expect("the copy engine runs nothing");
    assert_eq!(e.what, "an engine did not run its golden context");
    assert!(e.engine.is_some());
    // Given up after half a second, however long each wait took; the
    // engines reset after, as before.
    assert!((500_000_000..600_000_000).contains(&time.now()), "{}", time.now());
    assert_eq!(gt.resets(), 4);
    // What it was given stays: an engine that wakes up late finds it.
    assert!(mem.pages() > 0);
    // The engines never interrupt.
    assert!(!gt.interrupting());
}

#[test]
fn gives_its_memory_back() {
    let mem = Memory::default();
    let gt = Gt::new(mem.clone(), GTT_BASE);
    let mut g = render(&gt);
    // Once the golden contexts are freed, and a first space's tables have
    // taken their pool's memory.
    g.hardware_mut().poll(1_000_000_000);
    let warm = g.space_create(1).unwrap();
    let before = mem.pages();
    let space = g.space_create(1).unwrap();
    let ctx = g.context_create(1, space, &[RENDER, COPY]).unwrap();
    let (b, _) = buffer(&mut g, &mem, 1, &store(0x20000, 1));
    let (target, _) = buffer(&mut g, &mem, 1, &[]);
    g.execute(1, &submit(ctx, 0, &[(b, 0x10000), (target, 0x20000)], 24)).unwrap();
    g.hardware_mut().poll(1_000_000_001);
    g.poll();
    g.close(1, b);
    g.close(1, target);
    g.context_destroy(1, ctx);
    g.space_destroy(1, space).unwrap();
    // The contexts' memory goes once the engine has surely saved them.
    g.hardware_mut().poll(2_000_000_000);
    g.poll();
    assert_eq!(mem.pages(), before);
    g.space_destroy(1, warm).unwrap();
    no_faults(&gt);
}
