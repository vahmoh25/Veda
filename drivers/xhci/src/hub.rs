//! USB 2.0 hubs (USB 2.0 chapter 11): powering their ports, and finding
//! the devices on them through the hub's status change endpoint.
//!
//! SuperSpeed hubs are left alone: keyboards and mice are on the USB 2.0
//! hub that every USB 3 hub also contains.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use vrt::println;
use vusb::Speed;
use vusb::descriptor::{Configuration, TransferType, class};
use vusb::hub::{HubDescriptor, PortStatus, changed_ports, feature};
use vusb::request::Setup;

use crate::device::{ATTEMPTS, DEBOUNCE_MS, Function, Pipe, RESET_RECOVERY_MS};
use crate::{MS, UsbError, Work, Xhci, queue};

/// Hubs in a chain the route string can describe below a root port.
const MAX_DEPTH: u8 = 5;
/// Ports the route string can address.
const MAX_PORTS: u8 = 15;
/// How long a port reset may take.
const RESET_TIMEOUT_MS: u64 = 1000;

/// A configured hub.
pub struct Hub {
    /// The status change endpoint.
    pub pipe: Pipe,
    /// The ports that are looked after.
    pub ports: u8,
    /// The device on each port (index: port - 1).
    pub children: Vec<Option<u8>>,
}

impl Xhci {
    /// Configures a hub and powers its ports; the devices on them are
    /// looked for afterwards. Returns what the hub is, for the log.
    pub(crate) fn start_hub(&mut self, slot: u8, config: &Configuration) -> Result<Option<String>, UsbError> {
        let dev = self.devices.get(&slot).ok_or(UsbError::Gone)?;
        if dev.speed >= Speed::Super || dev.at.depth >= MAX_DEPTH {
            return Ok(None);
        }
        let endpoint = config
            .default_interfaces()
            .filter(|i| i.class == class::HUB)
            .flat_map(|i| i.endpoints.iter())
            .find(|e| e.is_in() && e.transfer_type() == TransferType::Interrupt)
            .copied()
            .ok_or(UsbError::BadDescriptor)?;
        let bytes = self.control_in(slot, Setup::hub_descriptor(HubDescriptor::MAX_LEN))?;
        let descriptor = HubDescriptor::parse(&bytes).ok_or(UsbError::BadDescriptor)?;
        // The status change report has a bit for the hub and one per port;
        // it is read a packet at a time.
        let mut pipe =
            self.configure(slot, config.value, &[endpoint], Some(&descriptor))?.pop().ok_or(UsbError::NoMemory)?;
        for port in 1..=descriptor.ports {
            let _ = self.control(slot, Setup::hub_set_port_feature(port, feature::PORT_POWER), &[]);
        }
        self.sleep((descriptor.power_on_delay_ms as u64).max(100));
        pipe.arm(&self.hc);
        let ports = descriptor.ports.min(MAX_PORTS);
        let dev = self.devices.get_mut(&slot).ok_or(UsbError::Gone)?;
        dev.function = Function::Hub(Hub { pipe, ports, children: vec![None; ports as usize] });
        // Look at every port once; changes are reported from now on.
        let mut all = vec![0u8; ports as usize / 8 + 1];
        for port in 1..=ports as usize {
            all[port / 8] |= 1 << (port % 8);
        }
        queue(&mut self.work, Work::HubPorts(slot, all));
        Ok(Some(format!("hub with {} ports", descriptor.ports)))
    }

    /// A hub reported changes (its status change bitmap).
    pub(crate) fn hub_changed(&mut self, hub: u8, report: &[u8]) {
        let Some(ports) = self.hub(hub).map(|h| h.ports) else { return };
        if report.first().is_some_and(|b| b & 1 != 0) {
            self.hub_status_changed(hub);
        }
        for port in changed_ports(report, ports).collect::<Vec<_>>() {
            self.hub_port_changed(hub, port);
        }
    }

    fn hub(&self, slot: u8) -> Option<&Hub> {
        match &self.devices.get(&slot)?.function {
            Function::Hub(h) => Some(h),
            _ => None,
        }
    }

    /// The hub itself changed (its power supply, or over-current).
    fn hub_status_changed(&mut self, hub: u8) {
        let Some(status) = self.control_in(hub, Setup::hub_status()).ok().and_then(|b| PortStatus::parse(&b)) else {
            return;
        };
        for (bit, f) in [(0, feature::C_HUB_LOCAL_POWER), (1, feature::C_HUB_OVER_CURRENT)] {
            if status.change & (1 << bit) != 0 {
                let _ = self.control(hub, Setup::hub_clear_feature(f), &[]);
            }
        }
        if status.change & 2 != 0 && status.status & 2 != 0 {
            println!("hub in slot {}: over-current", hub);
        }
    }

    fn hub_port_status(&mut self, hub: u8, port: u8) -> Result<PortStatus, UsbError> {
        PortStatus::parse(&self.control_in(hub, Setup::hub_port_status(port))?).ok_or(UsbError::BadDescriptor)
    }

    /// A hub port changed: a device came or went.
    fn hub_port_changed(&mut self, hub: u8, port: u8) {
        let Ok(status) = self.hub_port_status(hub, port) else { return };
        for f in status.change_features() {
            let _ = self.control(hub, Setup::hub_clear_port_feature(port, f), &[]);
        }
        let child = self.hub(hub).and_then(|h| h.children.get(port as usize - 1).copied().flatten());
        if let Some(child) = child {
            if status.connected() && !status.connection_changed() {
                return;
            }
            self.detach(child);
        }
        if status.connected() {
            self.attach_hub_port(hub, port);
        }
    }

    fn attach_hub_port(&mut self, hub: u8, port: u8) {
        for attempt in 1..=ATTEMPTS {
            self.sleep(DEBOUNCE_MS);
            match self.hub_port_status(hub, port) {
                Ok(status) if status.connected() => {}
                _ => return,
            }
            let Some(speed) = self.reset_hub_port(hub, port) else { continue };
            let Some(at) = self.devices.get(&hub).map(|h| h.at.child(hub, h.speed, port, speed)) else { return };
            let name = at.name();
            match self.enumerate(at, speed) {
                Ok(slot) => {
                    if let Some(Function::Hub(h)) = self.devices.get_mut(&hub).map(|d| &mut d.function)
                        && let Some(entry) = h.children.get_mut(port as usize - 1)
                    {
                        *entry = Some(slot);
                    }
                    return;
                }
                Err(e) => println!("port {}: {} (attempt {})", name, e, attempt),
            }
        }
    }

    /// Resets a hub port; returns the speed of the device on it.
    fn reset_hub_port(&mut self, hub: u8, port: u8) -> Option<Speed> {
        self.control(hub, Setup::hub_set_port_feature(port, feature::PORT_RESET), &[]).ok()?;
        let end = vrt::time::now_ns() + RESET_TIMEOUT_MS * MS;
        loop {
            self.sleep(10);
            let status = self.hub_port_status(hub, port).ok()?;
            if status.reset_changed() && !status.resetting() {
                let _ = self.control(hub, Setup::hub_clear_port_feature(port, feature::C_PORT_RESET), &[]);
                if !status.connected() || !status.enabled() {
                    return None;
                }
                self.sleep(RESET_RECOVERY_MS);
                return Some(status.speed());
            }
            if vrt::time::now_ns() > end {
                return None;
            }
        }
    }
}
