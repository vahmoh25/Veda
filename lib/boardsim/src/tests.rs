use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use vcs35l41::group::Amps;
use vcs35l41::regs::*;

use crate::halo::{Core, Halo, STATE_WORD, STATUS_PAUSED, STATUS_RUNNING};
use crate::zenbook::*;

/// The drivers started on the laptop, and its amplifiers found (what
/// devmgr and `lpss-spi` do before the bring-up).
fn start(z: &Zenbook) -> (Drivers<'_>, Amps) {
    let mut d = Drivers::start(z).expect("the drivers find the amplifiers");
    let links = spi_links(&d.device);
    let sub = d.device.identity.sub.clone().unwrap_or_default();
    let amps = Amps::find(&d.device.path.to_string(), &sub, &links).expect("a board Veda drives");
    d.spi.set_speed(amps.speed_hz);
    (d, amps)
}

fn problems(z: &Zenbook) -> Vec<String> {
    z.world.borrow().all_problems()
}

const FIRMWARE: &str = "cirrus/cs35l41-dsp1-spk-prot-10431f62.wmfw";
const TUNINGS: [&str; 2] =
    ["cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0-l0.bin", "cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0-r0.bin"];

fn le32(file: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(file[at..at + 4].try_into().unwrap())
}

/// The memory blocks of a `.wmfw` file and the registers they go to,
/// decoded here on their own (not with `vcs35l41::wmfw`): after the
/// header, blocks of a 24-bit offset in words, a type and a length.
fn firmware_blocks(file: &[u8]) -> Vec<(u32, &[u8])> {
    let mut pos = le32(file, 4) as usize;
    let mut blocks = Vec::new();
    while pos < file.len() {
        let (offset, kind, len) = (le32(file, pos) & 0xFF_FFFF, file[pos + 3], le32(file, pos + 4) as usize);
        let register = match kind {
            0x10 => Some(DSP1_PMEM_0 + offset * 5),
            0x11 => Some((DSP1_XMEM_PACK_0 + offset * 3) & !3),
            0x12 => Some((DSP1_YMEM_PACK_0 + offset * 3) & !3),
            _ => None,
        };
        if let Some(r) = register {
            blocks.push((r, &file[pos + 8..pos + 8 + len]));
        }
        pos += 8 + len;
    }
    blocks
}

/// The speaker protection's part of Y memory starts at word 14, as the
/// firmware's header lists it.
const CSPL_YM: u32 = 14;

/// The blocks of a `.bin` tuning, all for the speaker protection
/// (algorithm 0xCD), and the registers they go to, decoded here on their
/// own: after the header, blocks of an offset in bytes past the
/// algorithm's part, a type, the algorithm, its version, a 32-bit offset
/// and a length, padded to 32 bits.
fn tuning_blocks(file: &[u8]) -> Vec<(u32, &[u8])> {
    let mut pos = le32(file, 4) as usize;
    let mut blocks = Vec::new();
    while pos < file.len() {
        let (offset, kind) = (le32(file, pos) & 0xFFFF, le32(file, pos) >> 16);
        let (algorithm, len) = (le32(file, pos + 4), le32(file, pos + 16) as usize);
        let base = match kind {
            0x12 => Some((DSP1_YMEM_PACK_0 + CSPL_YM * 3) & !3),
            0x06 => Some(DSP1_YMEM_UNPACK24_0 + CSPL_YM * 4),
            _ => None,
        };
        if let Some(b) = base {
            assert_eq!(algorithm, 0xCD);
            blocks.push((b + offset, &file[pos + 20..pos + 20 + len]));
        }
        pos += 20 + ((len + 3) & !3);
    }
    blocks
}

/// A DSP's memories with blocks written in order (the firmware's, then
/// its tuning's over them).
fn memories<'a>(blocks: impl Iterator<Item = (u32, &'a [u8])>) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let mut dsp = Halo::new("expected");
    for (register, data) in blocks {
        for (i, w) in data.as_chunks::<4>().0.iter().enumerate() {
            dsp.write(register + 4 * i as u32, u32::from_be_bytes(*w), 0);
        }
    }
    (dsp.xm, dsp.ym, dsp.pm)
}

/// The log line of an amplifier running its firmware.
fn running(side: &str, tuning: &str, name: &str) -> String {
    format!(
        "\\_SB.PC00.SPI1.SPK1: the {} amplifier: firmware {} (v0.43.1), tuning {} ({}), at 17.5 dB",
        side, FIRMWARE, tuning, name
    )
}

#[test]
fn the_zenbook_plays_through_its_amplifiers() {
    let z = Zenbook::new();
    let (mut d, mut amps) = start(&z);
    assert_eq!(format!("{}", d.device.path), "\\_SB.PC00.SPI1.SPK1");
    assert_eq!(spi_links(&d.device).len(), 2);
    assert_eq!(d.spi.set_speed(amps.speed_hz), SPI_HZ);
    assert!(amps.bring_up(&mut d), "{:#?}", d.log);
    assert_eq!(
        d.log,
        ["\\_SB.PC00.SPI1.SPK1: board 10431F62, external boost, speaker id 0: \
             left at chip select 0 (revision B2, OTP id 8, 98 trims), \
             right at chip select 1 by GPIO (revision B2, OTP id 8, 98 trims)"]
    );
    {
        let w = z.world.borrow();
        for a in &w.amps {
            // Reset, booted, fixed for its revision, trimmed: a software
            // reset, then the trims and fixes behind the unlocked key.
            assert_eq!(a.writes[0], (SFT_RESET, SOFTWARE_RESET), "{}", a.name);
            assert_eq!(a.reg(BSTCVRT_DCM_CTRL), 0x51);
            // External boost: its own converter off, GPIO1 the supply's
            // switch (off), GPIO2 its interrupt.
            assert_eq!(a.reg(PWR_CTRL2) & BST_EN_MASK, 0);
            assert_eq!(a.reg(GPIO1_CTRL1), 1);
            assert_eq!(a.reg(GPIO_PAD_CONTROL), 0x0201_0000);
            assert!(!a.plays());
        }
        assert_eq!(w.amps[0].reg(SP_FRAME_RX_SLOT) & 0x3F, 0);
        assert_eq!(w.amps[1].reg(SP_FRAME_RX_SLOT) & 0x3F, 1);
    }

    // Their DSPs' firmware and tunings, by the names Linux looks for.
    assert!(amps.start_firmware(&mut d), "{:#?}", d.log);
    assert_eq!(
        d.log[1..],
        [
            running("left", TUNINGS[0], "UX7602_tooling_sample_Lch_RTL.bin"),
            running("right", TUNINGS[1], "UX7602_tooling_sample_Rch_RTL.bin")
        ]
    );
    {
        let w = z.world.borrow();
        for (a, tuning) in w.amps.iter().zip(TUNINGS) {
            // When its core started, its memories held every block of the
            // firmware where the file puts it, then the tuning in the
            // speaker protection's memory, and nothing else.
            let blocks = firmware_blocks(&d.files[FIRMWARE]);
            assert_eq!(blocks.len(), 6);
            let expected = memories(blocks.into_iter().chain(tuning_blocks(&d.files[tuning])));
            assert!(a.dsp.started_with.as_ref() == Some(&expected), "{}", a.name);
            // Running, and paused until playback starts.
            assert_eq!(a.dsp.core, Core::Running);
            assert_eq!(a.dsp.status, STATUS_PAUSED);
        }
    }

    // The codec's stream starts: its clock runs, the amplifiers power up,
    // their DSPs run, between the codec and the amplifier.
    z.world.borrow_mut().i2s_clock = true;
    assert_eq!(amps.playing(&mut d, true), Vec::<String>::new());
    for a in &z.world.borrow().amps {
        assert!(a.plays(), "{}", a.name);
        assert_eq!(a.reg(DAC_PCM1_SRC), SRC_DSP1TX1);
        assert_eq!(a.dsp.status, STATUS_RUNNING);
        assert_eq!(a.gain(), 17);
    }
    assert!(amps.is_playing());
    // Headphones plugged in, and out.
    assert!(amps.mute(&mut d, true));
    assert!(z.world.borrow().amps.iter().all(|a| a.active && !a.plays()));
    assert!(amps.mute(&mut d, false));
    assert!(z.world.borrow().amps.iter().all(|a| a.plays()));

    // Before the stream stops: down while the clock still runs, the DSPs
    // paused, the speakers no longer measured.
    assert_eq!(amps.playing(&mut d, false), Vec::<String>::new());
    z.world.borrow_mut().i2s_clock = false;
    for a in &z.world.borrow().amps {
        assert!(!a.active && !a.plays());
        assert_eq!(a.dsp.status, STATUS_PAUSED);
        assert_eq!(a.reg(PWR_CTRL2) & (VMON_EN | IMON_EN), 0);
    }
    // And again, as each new song starts.
    z.world.borrow_mut().i2s_clock = true;
    assert_eq!(amps.playing(&mut d, true), Vec::<String>::new());
    assert!(z.world.borrow().amps.iter().all(|a| a.plays()));

    // Never two devices selected at once, nothing written behind the
    // lock, nothing too fast, no frame cut short, the DSPs started as
    // they should be, no gain above 4.5 dB without their protection.
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn without_the_firmware_files_they_play_without_it() {
    let z = Zenbook::new();
    let (mut d, mut amps) = start(&z);
    d.files.clear();
    assert!(amps.bring_up(&mut d));
    assert!(!amps.start_firmware(&mut d));
    assert_eq!(
        d.log[1..],
        ["l0", "r0"].map(|amp| format!(
            "\\_SB.PC00.SPI1.SPK1: the {} amplifier: none of its firmware files, \
             cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0-{}.wmfw and the rest, is here; \
             it plays without its firmware",
            if amp == "l0" { "left" } else { "right" },
            amp
        ))
    );
    z.world.borrow_mut().i2s_clock = true;
    assert!(amps.playing(&mut d, true).is_empty());
    for a in &z.world.borrow().amps {
        // Straight from the codec, at Linux's gain without protection.
        assert!(a.plays());
        assert_eq!(a.reg(DAC_PCM1_SRC), SRC_ASPRX1);
        assert_eq!(a.gain(), 4);
        assert_eq!(a.dsp.core, Core::Off);
    }
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn a_tuning_missing() {
    let z = Zenbook::new();
    let (mut d, mut amps) = start(&z);
    d.files.remove(TUNINGS[1]);
    assert!(amps.bring_up(&mut d));
    assert!(amps.start_firmware(&mut d));
    assert_eq!(
        d.log[1..],
        [
            running("left", TUNINGS[0], "UX7602_tooling_sample_Lch_RTL.bin"),
            format!(
                "\\_SB.PC00.SPI1.SPK1: the right amplifier: {} is here, but no tuning for it ({} or \
                 cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0.bin); it plays without its firmware",
                FIRMWARE, TUNINGS[1]
            )
        ]
    );
    z.world.borrow_mut().i2s_clock = true;
    assert!(amps.playing(&mut d, true).is_empty());
    let w = z.world.borrow();
    assert!(w.amps.iter().all(|a| a.plays()));
    assert_eq!([w.amps[0].reg(DAC_PCM1_SRC), w.amps[1].reg(DAC_PCM1_SRC)], [SRC_DSP1TX1, SRC_ASPRX1]);
    assert_eq!([w.amps[0].gain(), w.amps[1].gain()], [17, 4]);
    drop(w);
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn firmware_that_does_not_start() {
    let z = Zenbook::with(Settings::default(), |w| w.amps[0].dsp.hangs = true);
    let (mut d, mut amps) = start(&z);
    assert!(amps.bring_up(&mut d));
    assert!(amps.start_firmware(&mut d));
    // The state word as the firmware's image left it.
    let state = z.world.borrow().amps[0].dsp.xm[STATE_WORD as usize];
    assert_eq!(
        d.log[1],
        format!(
            "\\_SB.PC00.SPI1.SPK1: the left amplifier: the firmware did not start (state {}); \
             it plays without its firmware",
            state
        )
    );
    // Its core stopped again; it plays without it, the right one with.
    assert_eq!(z.world.borrow().amps[0].dsp.core, Core::Off);
    z.world.borrow_mut().i2s_clock = true;
    assert!(amps.playing(&mut d, true).is_empty());
    let w = z.world.borrow();
    assert!(w.amps.iter().all(|a| a.plays()));
    assert_eq!([w.amps[0].gain(), w.amps[1].gain()], [4, 17]);
    drop(w);
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn a_damaged_firmware_file() {
    let z = Zenbook::new();
    let (mut d, mut amps) = start(&z);
    d.files.get_mut(FIRMWARE).unwrap().truncate(20_000);
    assert!(amps.bring_up(&mut d));
    assert!(!amps.start_firmware(&mut d));
    assert_eq!(
        d.log[1],
        format!(
            "\\_SB.PC00.SPI1.SPK1: the left amplifier: {}: it is cut short; it plays without its firmware",
            FIRMWARE
        )
    );
    // Nothing of it was written.
    let w = z.world.borrow();
    assert!(w.amps.iter().all(|a| a.dsp.pm.iter().all(|&r| r == 0) && a.dsp.core == Core::Off));
    drop(w);
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn firmware_that_stops_answering() {
    let z = Zenbook::new();
    let (mut d, mut amps) = start(&z);
    assert!(amps.bring_up(&mut d));
    assert!(amps.start_firmware(&mut d));
    z.world.borrow_mut().amps[1].dsp.deaf = true;
    z.world.borrow_mut().i2s_clock = true;
    // Told, and reported; the right speaker stays silent (its gain never
    // set), the left plays.
    assert_eq!(
        amps.playing(&mut d, true),
        [
            "the right amplifier (chip select 1): the firmware did not resume (status 0x1)",
            "the right amplifier (chip select 1): the firmware did not switch the speaker on (status 0x1)"
        ]
    );
    let w = z.world.borrow();
    assert!(w.amps[0].plays() && !w.amps[1].plays());
    assert_eq!(w.amps[1].gain(), 0);
    drop(w);
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn the_speaker_id_chooses_the_tuning() {
    let z = Zenbook::with(Settings::default(), |w| w.pads.hold(PIN_SPEAKER_ID, true));
    let (mut d, mut amps) = start(&z);
    assert!(amps.bring_up(&mut d));
    assert!(d.log[0].contains(", speaker id 1: "), "{}", d.log[0]);
    assert!(amps.start_firmware(&mut d));
    assert_eq!(
        d.log[1..],
        [
            running("left", "cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid1-l0.bin", "UX7602_tooling_sample_Lch_RTL.bin"),
            running(
                "right",
                "cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid1-r0.bin",
                "UX7602_tooling_sample_Rch_RTL.bin"
            )
        ]
    );
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn without_the_codecs_clock_they_do_not_power_up() {
    let z = Zenbook::new();
    let (mut d, mut amps) = start(&z);
    assert!(amps.bring_up(&mut d));
    let failed = amps.playing(&mut d, true);
    assert_eq!(failed.len(), 2, "{:?}", failed);
    assert!(failed.iter().all(|f| f.ends_with("it did not power up")), "{:?}", failed);
    assert!(z.world.borrow().amps.iter().all(|a| !a.active));
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn a_locked_reset_line_leaves_the_software_reset() {
    let z = Zenbook::with(Settings::default(), |w| w.pads.lock(PIN_RESET, true));
    let (mut d, mut amps) = start(&z);
    assert!(amps.bring_up(&mut d), "{:#?}", d.log);
    assert_eq!(d.log[0], "\\_SB.PC00.SPI1.SPK1: cannot drive the amplifiers' reset line (GPIO connection 1)");
    z.world.borrow_mut().i2s_clock = true;
    assert!(amps.playing(&mut d, true).is_empty());
    assert!(z.world.borrow().amps.iter().all(|a| a.plays()));
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn a_chip_select_the_firmware_keeps() {
    let z = Zenbook::with(Settings::default(), |w| w.pads.give_away(PIN_CHIP_SELECT));
    let (mut d, mut amps) = start(&z);
    assert!(!amps.bring_up(&mut d));
    assert_eq!(d.log, ["\\_SB.PC00.SPI1.SPK1: cannot drive the second amplifier's chip select (GPIO connection 0)"]);
    // Nothing went on the bus.
    assert!(z.world.borrow().amps.iter().all(|a| a.writes.is_empty()));
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn a_missing_amplifier() {
    let z = Zenbook::with(Settings::default(), |w| w.amps[1].absent = true);
    let (mut d, mut amps) = start(&z);
    assert!(amps.bring_up(&mut d));
    assert_eq!(
        d.log[0],
        "\\_SB.PC00.SPI1.SPK1: the right amplifier (chip select 1): \
         it reported an OTP error at boot (0xffffffff)"
    );
    assert!(d.log[1].ends_with(": left at chip select 0 (revision B2, OTP id 8, 98 trims)"), "{}", d.log[1]);
    assert!(amps.start_firmware(&mut d));
    assert_eq!(d.log[2..], [running("left", TUNINGS[0], "UX7602_tooling_sample_Lch_RTL.bin")]);
    z.world.borrow_mut().i2s_clock = true;
    assert!(amps.playing(&mut d, true).is_empty());
    assert!(z.world.borrow().amps[0].plays());
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn errors_while_playing_are_reported_and_released() {
    let z = Zenbook::new();
    let (mut d, mut amps) = start(&z);
    assert!(amps.bring_up(&mut d));
    z.world.borrow_mut().i2s_clock = true;
    assert!(amps.playing(&mut d, true).is_empty());
    z.world.borrow_mut().amps[0].fail(TEMP_ERR);
    assert_eq!(amps.errors(&mut d), ["the left amplifier: overheated"]);
    assert!(amps.errors(&mut d).is_empty());
    assert!(amps.playing(&mut d, false).is_empty());
    let w = z.world.borrow();
    let released: Vec<u32> =
        w.amps[0].writes.iter().filter(|(r, _)| *r == PROTECT_REL_ERR_IGN).map(|(_, v)| *v).collect();
    assert_eq!(released, [0, TEMP_ERR_RLS, 0]);
    assert_eq!(problems(&z), Vec::<String>::new());
}

#[test]
fn a_controller_the_firmware_hides() {
    // With the SPI controller in ACPI mode (SM01 = 2), it has no PCI
    // address: no driver is started for it.
    let mut settings = Settings::default();
    settings.0[0] = 2;
    let z = Zenbook::with(settings, |_| {});
    assert_eq!(Drivers::start(&z).err().as_deref(), Some("no ACPI device for 00:1e.3"));
}

#[test]
fn boards_veda_does_not_drive() {
    let z = Zenbook::new();
    let d = Drivers::start(&z).unwrap();
    let links = spi_links(&d.device);
    let path = d.device.path.to_string();
    let internal = Amps::find(&path, "10431F12", &links).err().unwrap();
    assert!(internal.ends_with("boost their own supply, which Veda does not drive yet"), "{}", internal);
    let unknown = Amps::find(&path, "10431F00", &links).err().unwrap();
    assert!(unknown.ends_with("no settings known for board 10431F00"), "{}", unknown);
}
