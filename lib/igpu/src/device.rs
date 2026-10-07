//! The GPUs this driver knows: Intel's integrated graphics with display
//! versions 12 and 13 (Tiger Lake to Raptor Lake), by PCI device id
//! (Linux's `include/drm/intel/pciids.h`).

/// A family of GPUs with the same display engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub name: &'static str,
    /// The display engine's version.
    pub display: u8,
    /// How many pipes it has (A, B, ...).
    pub pipes: u8,
}

const TIGER_LAKE: Platform = Platform { name: "Tiger Lake", display: 12, pipes: 4 };
const ROCKET_LAKE: Platform = Platform { name: "Rocket Lake", display: 12, pipes: 3 };
const ALDER_LAKE_S: Platform = Platform { name: "Alder Lake-S", display: 12, pipes: 4 };
const ALDER_LAKE_P: Platform = Platform { name: "Alder Lake-P", display: 13, pipes: 4 };
const ALDER_LAKE_N: Platform = Platform { name: "Alder Lake-N", display: 13, pipes: 4 };
const RAPTOR_LAKE_S: Platform = Platform { name: "Raptor Lake-S", display: 12, pipes: 4 };
const RAPTOR_LAKE_P: Platform = Platform { name: "Raptor Lake-P", display: 13, pipes: 4 };
const RAPTOR_LAKE_U: Platform = Platform { name: "Raptor Lake-U", display: 13, pipes: 4 };

/// Every known device id and its platform.
pub const DEVICES: &[(u16, Platform)] = &[
    // Tiger Lake (GT1, then GT2)
    (0x9A60, TIGER_LAKE),
    (0x9A68, TIGER_LAKE),
    (0x9A70, TIGER_LAKE),
    (0x9A40, TIGER_LAKE),
    (0x9A49, TIGER_LAKE),
    (0x9A59, TIGER_LAKE),
    (0x9A78, TIGER_LAKE),
    (0x9AC0, TIGER_LAKE),
    (0x9AC9, TIGER_LAKE),
    (0x9AD9, TIGER_LAKE),
    (0x9AF8, TIGER_LAKE),
    // Rocket Lake
    (0x4C80, ROCKET_LAKE),
    (0x4C8A, ROCKET_LAKE),
    (0x4C8B, ROCKET_LAKE),
    (0x4C8C, ROCKET_LAKE),
    (0x4C90, ROCKET_LAKE),
    (0x4C9A, ROCKET_LAKE),
    // Alder Lake-S
    (0x4680, ALDER_LAKE_S),
    (0x4682, ALDER_LAKE_S),
    (0x4688, ALDER_LAKE_S),
    (0x468A, ALDER_LAKE_S),
    (0x468B, ALDER_LAKE_S),
    (0x4690, ALDER_LAKE_S),
    (0x4692, ALDER_LAKE_S),
    (0x4693, ALDER_LAKE_S),
    // Alder Lake-P
    (0x46A0, ALDER_LAKE_P),
    (0x46A1, ALDER_LAKE_P),
    (0x46A2, ALDER_LAKE_P),
    (0x46A3, ALDER_LAKE_P),
    (0x46A6, ALDER_LAKE_P),
    (0x46A8, ALDER_LAKE_P),
    (0x46AA, ALDER_LAKE_P),
    (0x462A, ALDER_LAKE_P),
    (0x4626, ALDER_LAKE_P),
    (0x4628, ALDER_LAKE_P),
    (0x46B0, ALDER_LAKE_P),
    (0x46B1, ALDER_LAKE_P),
    (0x46B2, ALDER_LAKE_P),
    (0x46B3, ALDER_LAKE_P),
    (0x46C0, ALDER_LAKE_P),
    (0x46C1, ALDER_LAKE_P),
    (0x46C2, ALDER_LAKE_P),
    (0x46C3, ALDER_LAKE_P),
    // Alder Lake-N
    (0x46D0, ALDER_LAKE_N),
    (0x46D1, ALDER_LAKE_N),
    (0x46D2, ALDER_LAKE_N),
    (0x46D3, ALDER_LAKE_N),
    (0x46D4, ALDER_LAKE_N),
    // Raptor Lake-S
    (0xA780, RAPTOR_LAKE_S),
    (0xA781, RAPTOR_LAKE_S),
    (0xA782, RAPTOR_LAKE_S),
    (0xA783, RAPTOR_LAKE_S),
    (0xA788, RAPTOR_LAKE_S),
    (0xA789, RAPTOR_LAKE_S),
    (0xA78A, RAPTOR_LAKE_S),
    (0xA78B, RAPTOR_LAKE_S),
    // Raptor Lake-P
    (0xA720, RAPTOR_LAKE_P),
    (0xA7A0, RAPTOR_LAKE_P),
    (0xA7A8, RAPTOR_LAKE_P),
    (0xA7AA, RAPTOR_LAKE_P),
    (0xA7AB, RAPTOR_LAKE_P),
    // Raptor Lake-U
    (0xA721, RAPTOR_LAKE_U),
    (0xA7A1, RAPTOR_LAKE_U),
    (0xA7A9, RAPTOR_LAKE_U),
    (0xA7AC, RAPTOR_LAKE_U),
    (0xA7AD, RAPTOR_LAKE_U),
];

/// Intel's PCI vendor id.
pub const VENDOR: u16 = 0x8086;

/// The platform of Intel device `device`, if this driver knows it.
pub fn platform(device: u16) -> Option<Platform> {
    DEVICES.iter().find(|&&(id, _)| id == device).map(|&(_, p)| p)
}
