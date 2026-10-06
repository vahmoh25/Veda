//! The CS35L41's registers and bits that the sequences use (Linux's
//! `include/sound/cs35l41.h`).

pub const DEVID: u32 = 0x0000_0000;
pub const REVID: u32 = 0x0000_0004;
pub const OTPID: u32 = 0x0000_0010;
pub const SFT_RESET: u32 = 0x0000_0020;
pub const TEST_KEY_CTL: u32 = 0x0000_0040;
pub const OTP_MEM0: u32 = 0x0000_0400;
pub const PWR_CTRL1: u32 = 0x0000_2014;
pub const PWR_CTRL2: u32 = 0x0000_2018;
pub const PROTECT_REL_ERR_IGN: u32 = 0x0000_2034;
pub const GPIO_PAD_CONTROL: u32 = 0x0000_242C;
pub const PLL_CLK_CTRL: u32 = 0x0000_2C04;
pub const DSP_CLK_CTRL: u32 = 0x0000_2C08;
pub const GLOBAL_CLK_CTRL: u32 = 0x0000_2C0C;
pub const BSTCVRT_DCM_CTRL: u32 = 0x0000_381C;
pub const VIMON_SPKMON_RESYNC: u32 = 0x0000_4100;
pub const VPVBST_FS_SEL: u32 = 0x0000_4400;
pub const SP_ENABLES: u32 = 0x0000_4800;
pub const SP_RATE_CTRL: u32 = 0x0000_4804;
pub const SP_FORMAT: u32 = 0x0000_4808;
pub const SP_HIZ_CTRL: u32 = 0x0000_480C;
pub const SP_FRAME_RX_SLOT: u32 = 0x0000_4820;
pub const SP_TX_WL: u32 = 0x0000_4830;
pub const SP_RX_WL: u32 = 0x0000_4840;
pub const DAC_PCM1_SRC: u32 = 0x0000_4C00;
pub const ASP_TX1_SRC: u32 = 0x0000_4C20;
pub const ASP_TX2_SRC: u32 = 0x0000_4C24;
pub const ASP_TX3_SRC: u32 = 0x0000_4C28;
pub const ASP_TX4_SRC: u32 = 0x0000_4C2C;
pub const DSP1_RX1_SRC: u32 = 0x0000_4C40;
pub const DSP1_RX2_SRC: u32 = 0x0000_4C44;
pub const DSP1_RX3_SRC: u32 = 0x0000_4C48;
pub const DSP1_RX4_SRC: u32 = 0x0000_4C4C;
pub const DSP1_RX5_SRC: u32 = 0x0000_4C50;
pub const DSP1_RX6_SRC: u32 = 0x0000_4C54;
pub const AMP_DIG_VOL_CTRL: u32 = 0x0000_6000;
pub const AMP_GAIN_CTRL: u32 = 0x0000_6C04;
pub const OTP_TRIM_30: u32 = 0x0000_7418;
pub const IRQ1_STATUS1: u32 = 0x0001_0010;
pub const IRQ1_STATUS3: u32 = 0x0001_0018;
pub const IRQ1_STATUS4: u32 = 0x0001_001C;
pub const IRQ1_DB3: u32 = 0x0001_0318;
pub const IRQ2_DB3: u32 = 0x0001_0B18;
pub const GPIO1_CTRL1: u32 = 0x0001_1008;
pub const GPIO2_CTRL1: u32 = 0x0001_100C;
/// The DSP firmware's mailbox: its status, and the commands sent to it.
pub const DSP_MBOX_2: u32 = 0x0001_3004;
pub const DSP_VIRT1_MBOX_1: u32 = 0x0001_3020;
pub const DIE_STS1: u32 = 0x0001_7040;
pub const DIE_STS2: u32 = 0x0001_7044;

// The DSP's memories, each through a window of registers: packed (four
// 24-bit words to three registers, four 40-bit program words to five) and
// one data word to a register ("unpacked"). Their first and last
// registers.
pub const DSP1_XMEM_PACK_0: u32 = 0x0200_0000;
pub const DSP1_XMEM_PACK_LAST: u32 = 0x0200_2FF0;
pub const DSP1_XMEM_UNPACK24_0: u32 = 0x0280_0000;
pub const DSP1_XMEM_UNPACK24_LAST: u32 = 0x0280_3FF4;
pub const DSP1_YMEM_PACK_0: u32 = 0x02C0_0000;
pub const DSP1_YMEM_PACK_LAST: u32 = 0x02C0_17F0;
pub const DSP1_YMEM_UNPACK24_0: u32 = 0x0340_0000;
pub const DSP1_YMEM_UNPACK24_LAST: u32 = 0x0340_1FF4;
pub const DSP1_PMEM_0: u32 = 0x0380_0000;
pub const DSP1_PMEM_LAST: u32 = 0x0380_4FE8;

// The DSP's controls (`cs_dsp`'s HALO registers, from its base).
pub const DSP1_CTRL_BASE: u32 = 0x02B8_0000;
pub const DSP1_CORE_SOFT_RESET: u32 = DSP1_CTRL_BASE + 0x0_0010;
/// The sample rates of its eight inputs and eight outputs (every 8 bytes).
pub const DSP1_RX1_RATE: u32 = 0x02B8_0080;
pub const DSP1_TX1_RATE: u32 = 0x02B8_0280;
pub const DSP1_CCM_CORE_CTRL: u32 = DSP1_CTRL_BASE + 0x4_1000;
pub const DSP1_XM_ACCEL_PL0_PRI: u32 = 0x02BC_2020;
pub const DSP1_YM_ACCEL_PL0_PRI: u32 = 0x02BC_20E0;
/// Its memory protection: per window, access to X and Y memory, the
/// register windows, X and Y registers (`HALO_MPU_*`).
pub const DSP1_MPU_XMEM_ACCESS_0: u32 = DSP1_CTRL_BASE + 0x4_3000;
pub const DSP1_MPU_LOCK_CONFIG: u32 = DSP1_CTRL_BASE + 0x4_3140;
pub const DSP1_WDT_CONTROL: u32 = DSP1_CTRL_BASE + 0x4_7000;

// DSP1_CCM_CORE_CTRL, DSP1_CORE_SOFT_RESET, DSP1_WDT_CONTROL
pub const HALO_CORE_EN: u32 = 1 << 0;
pub const HALO_CORE_RESET: u32 = 1 << 9;
pub const HALO_CORE_SOFT_RESET: u32 = 1 << 0;
pub const HALO_WDT_EN: u32 = 1 << 0;

/// Words of OTP memory.
pub const OTP_WORDS: usize = 32;

/// `DEVID` of a CS35L41, and of a CS35L41R (odd metal revisions).
pub const CHIP_ID: u32 = 0x35A40;
pub const CHIP_ID_R: u32 = 0x35B40;
pub const MTLREVID_MASK: u32 = 0x0F;
pub const REVID_A0: u32 = 0xA0;
pub const REVID_B0: u32 = 0xB0;
pub const REVID_B2: u32 = 0xB2;

pub const SOFTWARE_RESET: u32 = 0x5A00_0000;

// PWR_CTRL1, PWR_CTRL2
pub const GLOBAL_EN: u32 = 1 << 0;
pub const AMP_EN: u32 = 1 << 0;
pub const BST_EN_MASK: u32 = 0x30;
/// Voltage and current monitoring of the speaker, which the DSP's
/// protection reads.
pub const VMON_EN: u32 = 1 << 12;
pub const IMON_EN: u32 = 1 << 13;

// SP_ENABLES
pub const ASP_TX1_EN: u32 = 1 << 0;
pub const ASP_RX1_EN: u32 = 1 << 16;
pub const ASP_RX2_EN: u32 = 1 << 17;

// Routing sources (DAC_PCM1_SRC, ASP_TXn_SRC, DSP1_RXn_SRC)
pub const SRC_ASPRX1: u32 = 0x08;
pub const SRC_ASPRX2: u32 = 0x09;
pub const SRC_VBSTMON: u32 = 0x29;
pub const SRC_DSP1TX1: u32 = 0x32;

// AMP_GAIN_CTRL: the PCM path's gain (in 1 dB steps from 0.5 dB) and the
// PDM path's.
pub const AMP_GAIN_PCM_SHIFT: u32 = 5;
pub const AMP_GAIN_PCM_MASK: u32 = 0x3E0;

// IRQ1_STATUS1: power up and down done, and the errors that make the
// amplifier shut itself down.
pub const PDN_DONE: u32 = 1 << 23;
pub const PUP_DONE: u32 = 1 << 24;
pub const BST_OVP_ERR: u32 = 1 << 6;
pub const BST_DCM_UVP_ERR: u32 = 1 << 7;
pub const BST_SHORT_ERR: u32 = 1 << 8;
pub const TEMP_WARN: u32 = 1 << 15;
pub const TEMP_ERR: u32 = 1 << 17;
pub const AMP_SHORT_ERR: u32 = 1 << 31;
pub const ERRORS: u32 = BST_OVP_ERR | BST_DCM_UVP_ERR | BST_SHORT_ERR | TEMP_WARN | TEMP_ERR | AMP_SHORT_ERR;

// IRQ1_STATUS3, IRQ1_STATUS4
pub const OTP_BOOT_ERR: u32 = 1 << 31;
pub const OTP_BOOT_DONE: u32 = 1 << 1;

// PROTECT_REL_ERR_IGN: releases each error.
pub const AMP_SHORT_ERR_RLS: u32 = 0x02;
pub const BST_SHORT_ERR_RLS: u32 = 0x04;
pub const BST_OVP_ERR_RLS: u32 = 0x08;
pub const BST_UVP_ERR_RLS: u32 = 0x10;
pub const TEMP_WARN_ERR_RLS: u32 = 0x20;
pub const TEMP_ERR_RLS: u32 = 0x40;

// GPIOx_CTRL1, GPIO_PAD_CONTROL
pub const GPIO_DIR: u32 = 1 << 31;
pub const GPIO_POL: u32 = 1 << 12;
pub const GPIO1_CTRL_MASK: u32 = 0x0003_0000;
pub const GPIO1_CTRL_SHIFT: u32 = 16;
pub const GPIO2_CTRL_MASK: u32 = 0x0700_0000;
pub const GPIO2_CTRL_SHIFT: u32 = 24;
/// GPIO1 as a GPIO (here, the switch of the external boost supply).
pub const GPIO1_GPIO: u32 = 1;
/// GPIO2 as an open-drain interrupt output.
pub const GPIO2_INT_OPEN_DRAIN: u32 = 2;
