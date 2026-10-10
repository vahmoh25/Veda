//! ACPI tables that tests add to QEMU's (`-acpitable`), describing what a
//! PC's firmware does and QEMU's does not, for the driver VM to pass on.

use vacpi::asm::{cat, device, dsm, i2c_descriptor, int, interrupt_of, name, resource_template, scope, string, uuid};

/// HID over I2C's `_DSM`.
const HID_OVER_I2C: &str = "3cdff6f7-4267-4555-ad05-b30a3d8938de";

/// The SSDT `name` stands for: `touchpad`, a HID-over-I2C device (as a
/// laptop's touchpad is described, on GSI 10, active high, since nothing
/// drives that input of QEMU's I/O APIC) below QEMU's virtio GPU at 00:10.0
/// (the firmware's `\_SB.PCI0.S80`), on its "bus".
pub fn table(name_of: &str) -> Option<Vec<u8>> {
    let aml = match name_of {
        "touchpad" => {
            let controller = "\\_SB.PCI0.S80";
            let crs = cat(&[
                &i2c_descriptor(0x15, 400_000, false, controller),
                &interrupt_of(10, false, false, false, false),
            ]);
            let body = cat(&[
                &name("_HID", &string("VTST0001")),
                &name("_CID", &string("PNP0C50")),
                &name("_UID", &int(1)),
                &name("_CRS", &resource_template(&crs)),
                &dsm(&uuid(HID_OVER_I2C)?, 1, &int(0x20)),
            ]);
            scope(controller, &device("TPD0", &body))
        }
        _ => return None,
    };
    let mut t = Vec::new();
    t.extend_from_slice(b"SSDT");
    t.extend_from_slice(&((36 + aml.len()) as u32).to_le_bytes());
    t.extend_from_slice(&[2, 0]);
    t.extend_from_slice(b"VEDA  TESTSSDT");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(b"VEDA");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&aml);
    t[9] = 0u8.wrapping_sub(t.iter().fold(0u8, |a, &b| a.wrapping_add(b)));
    Some(t)
}
