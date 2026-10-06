//! What particular codecs need before they play, beyond the
//! specification, as Linux's codec drivers do it. So far Realtek's, which
//! most PCs have:
//!
//! * **EAPD by verb.** Many Realtek codecs can run the EAPD pins (which
//!   power external amplifiers, the speakers' above all) themselves; a
//!   processing coefficient makes them follow the EAPD verb instead, which
//!   [`Routing::setup`](crate::route::Routing::setup) sends.
//! * **Amplifiers on a GPIO.** The firmware describes the board in an
//!   "assembly ID": in the codec's subsystem ID, or in the configuration
//!   of a pin that is never connected. Its amplifier field may say that a
//!   general-purpose pin of the codec switches the external amplifier;
//!   that pin is then driven high. Otherwise some older codecs get their
//!   EAPD driven high through a coefficient.

use crate::codec::{Bus, Codec};
use crate::verb::{self, id, id4, verb4};

/// Realtek's codecs (and Huawei's ALC256-alike, which Linux treats as
/// one).
const REALTEK: u32 = 0x10EC;
const HUAWEI_8326: u32 = 0x19E5_8326;
/// The node of Realtek's processing coefficients.
const COEF_NODE: u8 = 0x20;
/// The ALC260's amplifier coefficient is on another node.
const ALC260_COEF_NODE: u8 = 0x1A;

/// Boards whose assembly ID is known to be wrong (Linux's
/// `ALC*_FIXUP_SKU_IGNORE`), by PCI subsystem vendor and device.
const ASSEMBLY_ID_WRONG: &[(u16, u16)] = &[
    (0x17AA, 0x20F2), // ThinkPad SL410/510
    (0x17AA, 0x215E), // ThinkPad L512
    (0x17AA, 0x21B8), // ThinkPad Edge 14
    (0x17AA, 0x21CA), // ThinkPad L412
    (0x17AA, 0x21E9), // ThinkPad Edge 15
    (0x1025, 0x031C), // Gateway NV79
];

/// What [`prepare`] did, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Prepared {
    /// The GPIOs switched on for the external amplifier (a mask).
    pub amplifier_gpio: u8,
}

fn realtek(codec: &Codec) -> bool {
    codec.vendor_id >> 16 == REALTEK || codec.vendor_id == HUAWEI_8326
}

/// Prepares `codec` (powered, not yet set up) for playback. `board` is the
/// PCI subsystem vendor and device of the controller.
pub fn prepare(codec: &Codec, bus: &mut impl Bus, board: Option<(u16, u16)>) -> Prepared {
    let mut prepared = Prepared::default();
    if !realtek(codec) {
        return prepared;
    }
    let mut coefs = Coefficients { bus, revision: None };
    eapd_by_verb(codec, &mut coefs);
    match amplifier_gpio(codec, board) {
        Some(mask) => {
            let bus = &mut *coefs.bus;
            bus.command(codec.afg, verb::verb(id::SET_GPIO_ENABLE, mask));
            bus.command(codec.afg, verb::verb(id::SET_GPIO_DIRECTION, mask));
            bus.command(codec.afg, verb::verb(id::SET_GPIO_DATA, mask));
            prepared.amplifier_gpio = mask;
        }
        None => eapd_high(codec, &mut coefs),
    }
    prepared
}

/// Realtek's processing coefficients, through a bus.
struct Coefficients<'a, B: Bus> {
    bus: &'a mut B,
    /// Coefficient 0, whose bits 4-7 tell revisions apart.
    revision: Option<u16>,
}

impl<B: Bus> Coefficients<'_, B> {
    fn read(&mut self, node: u8, index: u8) -> Option<u16> {
        self.bus.command(node, verb4(id4::SET_COEF_INDEX, index as u16))?;
        self.bus.command(node, verb4(id4::GET_PROC_COEF, 0)).map(|v| v as u16)
    }

    fn write(&mut self, node: u8, index: u8, value: u16) {
        self.bus.command(node, verb4(id4::SET_COEF_INDEX, index as u16));
        self.bus.command(node, verb4(id4::SET_PROC_COEF, value));
    }

    /// Clears `mask` and sets `bits` in a coefficient.
    fn update(&mut self, index: u8, mask: u16, bits: u16) {
        self.update_on(COEF_NODE, index, mask, bits);
    }

    fn update_on(&mut self, node: u8, index: u8, mask: u16, bits: u16) {
        if let Some(v) = self.read(node, index) {
            self.write(node, index, (v & !mask) | bits);
        }
    }

    fn revision(&mut self) -> u16 {
        if self.revision.is_none() {
            self.revision = Some(self.read(COEF_NODE, 0).unwrap_or(0));
        }
        self.revision.unwrap_or(0) & 0x00F0
    }
}

/// Makes the codec's EAPD pins follow the EAPD verb (Linux's
/// `alc_fill_eapd_coef`).
fn eapd_by_verb<B: Bus>(codec: &Codec, c: &mut Coefficients<B>) {
    match codec.vendor_id {
        0x10EC_0262 => c.update(0x07, 0, 1 << 5),
        0x10EC_0267 | 0x10EC_0268 => c.update(0x07, 0, 1 << 13),
        0x10EC_0269 => match c.revision() {
            0x10 => c.update(0x0D, 0, 1 << 14),
            0x20 => c.update(0x04, 1 << 15, 0),
            0x30 => c.update(0x10, 1 << 9, 0),
            _ => {}
        },
        0x10EC_0280 | 0x10EC_0284 | 0x10EC_0290 | 0x10EC_0292 => c.update(0x04, 1 << 15, 0),
        0x10EC_0225 | 0x10EC_0295 | 0x10EC_0299 => {
            c.update(0x67, 0xF000, 0x3000);
            c.update(0x36, 1 << 13, 0);
            c.update(0x10, 1 << 9, 0);
        }
        0x10EC_0215 | 0x10EC_0285 | 0x10EC_0289 => {
            c.update(0x36, 1 << 13, 0);
            c.update(0x10, 1 << 9, 0);
        }
        0x10EC_0230 | 0x10EC_0233 | 0x10EC_0235 | 0x10EC_0236 | 0x10EC_0245 | 0x10EC_0255 | 0x10EC_0256
        | HUAWEI_8326 | 0x10EC_0257 | 0x10EC_0282 | 0x10EC_0283 | 0x10EC_0286 | 0x10EC_0288 | 0x10EC_0298
        | 0x10EC_0300 => c.update(0x10, 1 << 9, 0),
        0x10EC_0275 => c.update(0x0E, 0, 1 << 0),
        0x10EC_0287 => {
            c.update(0x10, 1 << 9, 0);
            c.write(COEF_NODE, 0x08, 0x4AB7);
        }
        0x10EC_0293 => c.update(0x0A, 1 << 13, 0),
        0x10EC_0234 | 0x10EC_0274 | 0x10EC_0294 | 0x10EC_0700 | 0x10EC_0701 | 0x10EC_0703 | 0x10EC_0711 => {
            c.update(0x10, 1 << 15, 0)
        }
        0x10EC_0662 => {
            if c.revision() == 0x30 {
                c.update(0x04, 1 << 10, 0);
            }
        }
        0x10EC_0272 | 0x10EC_0273 | 0x10EC_0663 | 0x10EC_0665 | 0x10EC_0670 | 0x10EC_0671 | 0x10EC_0672 => {
            c.update(0x0D, 0, 1 << 14)
        }
        0x10EC_0222 | 0x10EC_0623 => c.update(0x19, 1 << 13, 0),
        0x10EC_0668 => c.update(0x07, 3 << 13, 0),
        0x10EC_0867 => c.update(0x04, 1 << 10, 0),
        0x10EC_0888 => {
            if matches!(c.revision(), 0x20 | 0x30) {
                c.update(0x07, 1 << 5, 0);
            }
        }
        0x10EC_0892 | 0x10EC_0897 => c.update(0x07, 1 << 5, 0),
        0x10EC_0899 | 0x10EC_0900 | 0x10EC_0B00 | 0x10EC_1168 | 0x10EC_1220 => c.update(0x07, 1 << 1, 0),
        _ => {}
    }
}

/// Drives EAPD high through a coefficient on the older codecs that need
/// it (Linux's `alc_auto_init_amp` without a GPIO).
fn eapd_high<B: Bus>(codec: &Codec, c: &mut Coefficients<B>) {
    match codec.vendor_id {
        0x10EC_0260 => c.update_on(ALC260_COEF_NODE, 0x07, 0, 0x2010),
        0x10EC_0880 | 0x10EC_0882 | 0x10EC_0883 | 0x10EC_0885 => c.update(0x07, 0, 0x2030),
        0x10EC_0888 => {
            if matches!(c.revision(), 0x00 | 0x10) {
                c.update(0x07, 0, 0x2030);
            }
        }
        _ => {}
    }
}

/// The firmware's assembly ID: the codec's subsystem ID when it is not
/// the board's, or else the configuration of the pin that is never
/// connected (0x1D, 0x17 on the ALC260), with its checksum (Linux's
/// `alc_subsystem_id`).
fn assembly_id(codec: &Codec, board: Option<(u16, u16)>) -> Option<u32> {
    if board.is_some_and(|b| ASSEMBLY_ID_WRONG.contains(&b)) {
        return None;
    }
    let ssid = codec.subsystem & 0xFFFF;
    if board.is_some_and(|(_, device)| device as u32 != ssid) && ssid & 1 != 0 {
        return Some(ssid);
    }
    let nid = if codec.vendor_id == 0x10EC_0260 { 0x17 } else { 0x1D };
    let id = codec.widget(nid)?.config.0;
    // Valid, on a pin without a connection, and the checksum (bits 16-19)
    // counts the bits set in 1-15.
    let ones = (1..16).filter(|bit| id >> bit & 1 != 0).count() as u32;
    (id & 1 != 0 && id >> 30 == 1 && (id >> 16) & 0xF == ones).then_some(id)
}

/// The GPIO that switches the external amplifier, if the assembly ID
/// names one (bits 3-5: 1, 3 and 7 are GPIO 0, 1 and 2).
fn amplifier_gpio(codec: &Codec, board: Option<(u16, u16)>) -> Option<u8> {
    match (assembly_id(codec, board)? & 0x38) >> 3 {
        1 => Some(0x01),
        3 => Some(0x02),
        7 => Some(0x04),
        _ => None,
    }
}
