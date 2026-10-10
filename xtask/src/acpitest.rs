//! ACPI tables that tests add to QEMU's (`-acpitable`), describing what a
//! PC's firmware does and QEMU's does not, for the driver VM to pass on.

use vacpi::asm::{
    cat, device, dsm, gpio_descriptor_of, i2c_descriptor, int, interrupt_of, name, resource_template, scope, string,
    uuid,
};
use vacpi::resource::Gpio;

/// HID over I2C's `_DSM`.
const HID_OVER_I2C: &str = "3cdff6f7-4267-4555-ad05-b30a3d8938de";

/// The table `name` stands for: `touchpad`, an SSDT with a HID-over-I2C
/// device (as a laptop's touchpad is described, on GSI 10, active high,
/// since nothing drives that input of QEMU's I/O APIC) below QEMU's virtio
/// GPU at 00:10.0 (the firmware's `\_SB.PCI0.S80`), on its "bus"; `gpio`,
/// an SSDT with a device there wired to pins of a simulated GPIO
/// controller of devmgr's; `nhlt`, an NHLT (an Intel audio DSP's links),
/// with none.
pub fn table(name_of: &str) -> Option<Vec<u8>> {
    if name_of == "nhlt" {
        return Some(whole(b"NHLT", 0, b"TESTNHLT", &[0]));
    }
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
        "gpio" => {
            // One of devmgr's simulated GPIO controllers, and a device below
            // QEMU's virtio GPU wired to three of its pins: 0 an output, 1
            // an input that interrupts on both edges, 3 an input.
            let controller = "\\_SB.TGPI";
            let pin = |pin: u16, interrupt: bool, restriction: u8| Gpio {
                interrupt,
                pins: vec![pin],
                controller: controller.into(),
                pull: 3,
                restriction,
                edge: interrupt,
                polarity: if interrupt { 2 } else { 0 },
                shared: false,
                wake: false,
                debounce: 0,
            };
            let crs = cat(&[
                &gpio_descriptor_of(&pin(0, false, 2)),
                &gpio_descriptor_of(&pin(1, false, 1)),
                &gpio_descriptor_of(&pin(1, true, 0)),
                &gpio_descriptor_of(&pin(3, false, 1)),
            ]);
            let simulated = device("TGPI", &cat(&[&name("_HID", &string("VTST0002")), &name("_UID", &int(0))]));
            let wired =
                device("GPT0", &cat(&[&name("_HID", &string("VTST0003")), &name("_CRS", &resource_template(&crs))]));
            cat(&[&scope("\\_SB", &simulated), &scope("\\_SB.PCI0.S80", &wired)])
        }
        _ => return None,
    };
    Some(whole(b"SSDT", 2, b"TESTSSDT", &aml))
}

/// A table: its header (with the checksum), then `body`.
fn whole(signature: &[u8; 4], revision: u8, oem_table_id: &[u8; 8], body: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(signature);
    t.extend_from_slice(&((36 + body.len()) as u32).to_le_bytes());
    t.extend_from_slice(&[revision, 0]);
    t.extend_from_slice(b"VEDA  ");
    t.extend_from_slice(oem_table_id);
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(b"VEDA");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(body);
    t[9] = 0u8.wrapping_sub(t.iter().fold(0u8, |a, &b| a.wrapping_add(b)));
    t
}
