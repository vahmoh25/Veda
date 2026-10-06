//! Keyboards, mice and tablets (the HID class): reading their report
//! descriptors, polling their interrupt endpoints, and setting keyboard
//! lights. Reports are decoded by `vusb::hid`.

use alloc::string::String;
use alloc::vec::Vec;

use vrt::println;
use vusb::descriptor::{Configuration, Endpoint, Interface, TransferType, class, kind};
use vusb::hid::{Decoder, ReportDescriptor, boot};
use vusb::request::Setup;

use crate::device::{Function, Pipe};
use crate::{UsbError, Xhci};

/// The interface subclass of keyboards and mice that offer the boot
/// protocol, and their protocols.
const BOOT_SUBCLASS: u8 = 1;
const BOOT_KEYBOARD: u8 = 1;
const BOOT_MOUSE: u8 = 2;
/// The largest report read (the pipe's buffer).
const MAX_REPORT: usize = 4096;

/// A keyboard, mouse or tablet interface being polled.
pub struct HidInterface {
    pub number: u8,
    pub pipe: Pipe,
    pub decoder: Decoder,
}

impl Xhci {
    /// Configures the device and starts polling its keyboard, mouse and
    /// tablet interfaces. Returns what it is, for the log (`None`: no such
    /// interface).
    pub(crate) fn start_hid(&mut self, slot: u8, config: &Configuration) -> Result<Option<String>, UsbError> {
        let candidates: Vec<(Interface, Endpoint)> = config
            .default_interfaces()
            .filter(|i| i.class == class::HID)
            .filter_map(|i| {
                let e = i.endpoints.iter().find(|e| e.is_in() && e.transfer_type() == TransferType::Interrupt)?;
                Some((i.clone(), *e))
            })
            .collect();
        if candidates.is_empty() {
            return Ok(None);
        }
        let endpoints: Vec<Endpoint> = candidates.iter().map(|c| c.1).collect();
        let pipes = self.configure(slot, config.value, &endpoints, None)?;
        let mut interfaces = Vec::new();
        for ((interface, endpoint), mut pipe) in candidates.into_iter().zip(pipes) {
            let Some(decoder) = self.decoder(slot, &interface) else { continue };
            // A report shorter than a packet ends with a short packet; a
            // longer one takes packets until it is complete.
            pipe.len = decoder.max_report_len().max(endpoint.packet_size() as usize).clamp(1, MAX_REPORT) as u32;
            interfaces.push(HidInterface { number: interface.number, pipe, decoder });
        }
        if interfaces.is_empty() {
            return Ok(None);
        }
        let mut kinds: Vec<&str> = interfaces.iter().map(|h| h.decoder.describe()).collect();
        kinds.dedup();
        let what = kinds.join(", ");
        for h in interfaces.iter_mut() {
            h.pipe.arm(&self.hc);
        }
        self.devices.get_mut(&slot).ok_or(UsbError::Gone)?.function = Function::Hid(interfaces);
        self.set_leds(slot);
        Ok(Some(what))
    }

    /// The decoder for an interface's reports, from its report descriptor;
    /// boot keyboards and mice whose descriptor cannot be used are switched
    /// to the boot protocol. `None`: neither keys nor a pointer.
    fn decoder(&mut self, slot: u8, interface: &Interface) -> Option<Decoder> {
        let n = interface.number;
        let parsed = match interface.hid_report_length {
            Some(len) if len > 0 => self
                .control_in(slot, Setup::get_interface_descriptor(kind::HID_REPORT, n, len))
                .ok()
                .and_then(|bytes| ReportDescriptor::parse(&bytes).ok()),
            _ => None,
        };
        let boot_interface = interface.subclass == BOOT_SUBCLASS;
        if let Some(decoder) = parsed.map(|r| Decoder::new(&r)).filter(|d| d.is_useful()) {
            if boot_interface {
                // Devices start with the report protocol, but the firmware
                // may have left them in the boot protocol.
                let _ = self.control(slot, Setup::hid_set_protocol(n, true), &[]);
            }
            self.quiet(slot, n, &decoder);
            return Some(decoder);
        }
        let layout = match (boot_interface, interface.protocol) {
            (true, BOOT_KEYBOARD) => boot::KEYBOARD,
            (true, BOOT_MOUSE) => boot::MOUSE,
            _ => return None,
        };
        self.control(slot, Setup::hid_set_protocol(n, false), &[]).ok()?;
        let decoder = Decoder::new(&ReportDescriptor::parse(layout).ok()?);
        if let Some(dev) = self.devices.get(&slot) {
            println!(
                "port {}: interface {}: the boot protocol, as the report descriptor is unusable",
                dev.at.name(),
                n
            );
        }
        self.quiet(slot, n, &decoder);
        Some(decoder)
    }

    /// Keyboards report only changes (SET_IDLE 0), rather than repeating
    /// their state twice a second; many mice reject the request.
    fn quiet(&mut self, slot: u8, interface: u8, decoder: &Decoder) {
        if decoder.is_keyboard() {
            let _ = self.control(slot, Setup::hid_set_idle(interface), &[]);
        }
    }

    /// Sets the lights of a device's keyboards.
    pub(crate) fn set_leds(&mut self, slot: u8) {
        let reports: Vec<(u8, u8, Vec<u8>)> = match self.devices.get(&slot).map(|d| &d.function) {
            Some(Function::Hid(interfaces)) => interfaces
                .iter()
                .filter_map(|h| h.decoder.led_report(self.leds).map(|(id, data)| (h.number, id, data)))
                .collect(),
            _ => return,
        };
        for (interface, id, data) in reports {
            let _ = self.control(slot, Setup::hid_set_output_report(interface, id, data.len() as u16), &data);
        }
    }
}
