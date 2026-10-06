//! The amplifier's OTP memory: trims measured at the factory, packed bit
//! after bit from word 2, bit 16 on, which the driver writes into their
//! registers (with the test key unlocked) before using the amplifier.

use alloc::vec::Vec;

use crate::regs::OTP_WORDS;

/// One packed trim: its register, and its bit position and width there.
/// Register 0 is a bit to skip.
struct Element(u32, u8, u8);

/// The packing of OTP ids 1 and 8 (`otp_map_1`); ids 2, 3 and 6 differ in
/// one bit, which is `VMON_POL` instead of a spare bit (`otp_map_2`).
const MAP: [Element; 99] = [
    Element(0x2030, 0, 4),   // TRIM_OSC_FREQ_TRIM
    Element(0x2030, 7, 1),   // TRIM_OSC_TRIM_DONE
    Element(0x208C, 24, 6),  // TST_DIGREG_VREF_TRIM
    Element(0x2090, 14, 4),  // TST_REF_TRIM
    Element(0x2090, 10, 4),  // TST_REF_TEMPCO_TRIM
    Element(0x300C, 11, 4),  // PLL_LDOA_TST_VREF_TRIM
    Element(0x394C, 23, 2),  // BST_ATEST_CM_VOFF
    Element(0x3950, 0, 7),   // BST_ATRIM_IADC_OFFSET
    Element(0x3950, 8, 7),   // BST_ATRIM_IADC_GAIN1
    Element(0x3950, 16, 8),  // BST_ATRIM_IPKCOMP_OFFSET1
    Element(0x3950, 24, 8),  // BST_ATRIM_IPKCOMP_GAIN1
    Element(0x3954, 0, 7),   // BST_ATRIM_IADC_OFFSET2
    Element(0x3954, 8, 7),   // BST_ATRIM_IADC_GAIN2
    Element(0x3954, 16, 8),  // BST_ATRIM_IPKCOMP_OFFSET2
    Element(0x3954, 24, 8),  // BST_ATRIM_IPKCOMP_GAIN2
    Element(0x3958, 0, 7),   // BST_ATRIM_IADC_OFFSET3
    Element(0x3958, 8, 7),   // BST_ATRIM_IADC_GAIN3
    Element(0x3958, 16, 8),  // BST_ATRIM_IPKCOMP_OFFSET3
    Element(0x3958, 24, 8),  // BST_ATRIM_IPKCOMP_GAIN3
    Element(0x395C, 0, 7),   // BST_ATRIM_IADC_OFFSET4
    Element(0x395C, 8, 7),   // BST_ATRIM_IADC_GAIN4
    Element(0x395C, 16, 8),  // BST_ATRIM_IPKCOMP_OFFSET4
    Element(0x395C, 24, 8),  // BST_ATRIM_IPKCOMP_GAIN4
    Element(0x416C, 0, 8),   // VMON_GAIN_OTP_VAL
    Element(0x4160, 0, 7),   // VMON_OFFSET_OTP_VAL
    Element(0x416C, 8, 8),   // IMON_GAIN_OTP_VAL
    Element(0x4160, 16, 10), // IMON_OFFSET_OTP_VAL
    Element(0x416C, 16, 12), // VMON_CM_GAIN_OTP_VAL
    Element(0x416C, 28, 1),  // VMON_CM_GAIN_SIGN_OTP_VAL
    Element(0x4170, 0, 6),   // IMON_CAL_TEMPCO_OTP_VAL
    Element(0x4170, 6, 1),   // IMON_CAL_TEMPCO_SIGN_OTP
    Element(0x4170, 8, 6),   // IMON_CAL_TEMPCO2_OTP_VAL
    Element(0x4170, 14, 1),  // IMON_CAL_TEMPCO2_DN_UPB_OTP_VAL
    Element(0x4170, 16, 9),  // IMON_CAL_TEMPCO_TBASE_OTP_VAL
    Element(0x4360, 0, 5),   // TEMP_GAIN_OTP_VAL
    Element(0x4360, 6, 9),   // TEMP_OFFSET_OTP_VAL
    Element(0x4448, 0, 8),   // VP_SARADC_OFFSET
    Element(0x4448, 8, 8),   // VP_GAIN_INDEX
    Element(0x4448, 16, 8),  // VBST_SARADC_OFFSET
    Element(0x4448, 24, 8),  // VBST_GAIN_INDEX
    Element(0x444C, 0, 3),   // ANA_SELINVREF
    Element(0x6E30, 0, 5),   // GAIN_ERR_COEFF_0
    Element(0x6E30, 8, 5),
    Element(0x6E30, 16, 5),
    Element(0x6E30, 24, 5),
    Element(0x6E34, 0, 5),
    Element(0x6E34, 8, 5),
    Element(0x6E34, 16, 5),
    Element(0x6E34, 24, 5),
    Element(0x6E38, 0, 5),
    Element(0x6E38, 8, 5),
    Element(0x6E38, 16, 5),
    Element(0x6E38, 24, 5),
    Element(0x6E3C, 0, 5),
    Element(0x6E3C, 8, 5),
    Element(0x6E3C, 16, 5),
    Element(0x6E3C, 24, 5),
    Element(0x6E40, 0, 5),
    Element(0x6E40, 8, 5),
    Element(0x6E40, 16, 5),
    Element(0x6E40, 24, 5),
    Element(0x6E44, 0, 5),  // GAIN_ERR_COEFF_20
    Element(0x6E48, 0, 10), // VOFF_GAIN_0
    Element(0x6E48, 10, 10),
    Element(0x6E48, 20, 10),
    Element(0x6E4C, 0, 10),
    Element(0x6E4C, 10, 10),
    Element(0x6E4C, 20, 10),
    Element(0x6E50, 0, 10),
    Element(0x6E50, 10, 10),
    Element(0x6E50, 20, 10),
    Element(0x6E54, 0, 10),
    Element(0x6E54, 10, 10),
    Element(0x6E54, 20, 10),
    Element(0x6E58, 0, 10),
    Element(0x6E58, 10, 10),
    Element(0x6E58, 20, 10),
    Element(0x6E5C, 0, 10),
    Element(0x6E5C, 10, 10),
    Element(0x6E5C, 20, 10),
    Element(0x6E60, 0, 10),
    Element(0x6E60, 10, 10),
    Element(0x6E60, 20, 10), // VOFF_GAIN_20
    Element(0x6E64, 0, 10),  // VOFF_INT1
    Element(0x7418, 7, 5),   // DS_SPK_INT1_CAP_TRIM
    Element(0x741C, 0, 5),   // DS_SPK_INT2_CAP_TRIM
    Element(0x741C, 11, 4),  // DS_SPK_LPF_CAP_TRIM
    Element(0x741C, 19, 4),  // DS_SPK_QUAN_CAP_TRIM
    Element(0x7434, 17, 1),  // FORCE_CAL
    Element(0x7434, 18, 7),  // CAL_OVERRIDE
    Element(0x7068, 0, 9),   // MODIX
    Element(0x410C, 7, 1),   // VIMON_DLY_NOT_COMB
    Element(0x400C, 0, 7),   // VIMON_DLY
    Element(0x0000, 0, 1),   // a spare bit, or VMON_POL (see VMON_POL)
    Element(0x17040, 0, 8),  // X_COORDINATE
    Element(0x17040, 8, 8),  // Y_COORDINATE
    Element(0x17040, 16, 8), // WAFER_ID
    Element(0x17040, 24, 8), // DVS
    Element(0x17044, 0, 24), // LOT_NUMBER
];

/// Where the map differs for OTP ids 2, 3 and 6: element 93 is
/// `VMON_POL`.
const SPARE: usize = 93;
const VMON_POL: Element = Element(0x4000, 11, 1);

/// One trim to write: `register = register & !mask | value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trim {
    pub register: u32,
    pub mask: u32,
    pub value: u32,
}

fn bits(high: u32, low: u32) -> u32 {
    (((1u64 << (high + 1)) - 1) & !((1u64 << low) - 1)) as u32
}

/// Unpacks the OTP memory of an amplifier whose OTP id is `otp_id` into
/// the trims to write, in order. `None` if the id is not one Cirrus Logic
/// documents.
pub fn unpack(otp_id: u32, words: &[u32; OTP_WORDS]) -> Option<Vec<Trim>> {
    let vmon_pol = match otp_id {
        0x01 | 0x08 => false,
        0x02 | 0x03 | 0x06 => true,
        _ => return None,
    };
    let mut trims = Vec::new();
    let (mut bit, mut word) = (16u32, 2usize);
    for (i, e) in MAP.iter().enumerate() {
        let e = if vmon_pol && i == SPARE { &VMON_POL } else { e };
        let size = e.2 as u32;
        let value = if bit + size > 32 {
            // Across two words.
            let low = (*words.get(word)? & bits(31, bit)) >> bit;
            let high = *words.get(word + 1)? & bits(bit + size - 33, 0);
            let v = low | high << (32 - bit);
            word += 1;
            bit = bit + size - 32;
            v
        } else {
            let v = (*words.get(word)? & bits(bit + size - 1, bit)) >> bit;
            bit += size;
            v
        };
        if bit == 32 {
            bit = 0;
            word += 1;
        }
        if e.0 != 0 {
            let shift = e.1 as u32;
            trims.push(Trim { register: e.0, mask: bits(shift + size - 1, shift), value: value << shift });
        }
    }
    Some(trims)
}
