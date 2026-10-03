//! The `wlan` protocol for applications.

use alloc::vec::Vec;

use vipc::Bytes;
use vproto::wlan::{
    BssInfo, ConnectRequest, NetworkInfo, SavedNetwork, WlanDiagnostics, WlanError, WlanStatus, ssid_display, wlan,
};
use vrt::object::Channel;

use crate::Wlan;
use crate::scan::to_proto;

pub struct Session<'a> {
    pub w: &'a mut Wlan,
    pub now: u64,
}

impl Session<'_> {
    fn ready(&self) -> Result<(), WlanError> {
        if !self.w.radio_usable() {
            Err(WlanError::NoAdapter)
        } else if !self.w.radio_on {
            Err(WlanError::RadioOff)
        } else {
            Ok(())
        }
    }
}

impl wlan::Server for Session<'_> {
    fn status(&mut self) -> WlanStatus {
        self.w.status(self.now)
    }

    fn scan(&mut self) -> Result<(), WlanError> {
        self.ready()?;
        self.w.start_scan(true, self.now);
        Ok(())
    }

    fn networks(&mut self) -> Vec<NetworkInfo> {
        let connected = self
            .w
            .current
            .as_ref()
            .filter(|c| c.state == vproto::wlan::ConnState::Connected)
            .map(|c| c.request.ssid.clone());
        self.w.table.networks(self.now, &self.w.profiles, connected.as_deref())
    }

    fn access_points(&mut self) -> Vec<BssInfo> {
        self.w.table.access_points(self.now)
    }

    fn connect(&mut self, request: ConnectRequest) -> Result<(), WlanError> {
        self.w.connect_request(request, self.now)
    }

    fn disconnect(&mut self) -> Result<(), WlanError> {
        self.ready()?;
        self.w.log("disconnected by the user".into());
        self.w.user_disconnect(self.now);
        Ok(())
    }

    fn saved(&mut self) -> Vec<SavedNetwork> {
        self.w
            .profiles
            .iter()
            .map(|p| SavedNetwork {
                ssid: Bytes(p.ssid.clone()),
                name: ssid_display(&p.ssid),
                security: to_proto(p.security),
                auto_connect: p.auto_connect,
                hidden: p.hidden,
                last_connected: p.last_connected,
            })
            .collect()
    }

    fn forget(&mut self, ssid: Bytes) -> Result<(), WlanError> {
        self.w.forget(&ssid.0, self.now)
    }

    fn set_auto_connect(&mut self, ssid: Bytes, enabled: bool) -> Result<(), WlanError> {
        self.w.set_auto_connect(&ssid.0, enabled)
    }

    fn set_radio(&mut self, on: bool) -> Result<(), WlanError> {
        if self.w.radio.is_none() {
            return Err(WlanError::NoAdapter);
        }
        self.w.set_radio(on, self.now);
        Ok(())
    }

    fn watch(&mut self, events: Channel) -> Result<(), WlanError> {
        if self.w.add_watcher(events) { Ok(()) } else { Err(WlanError::LimitReached) }
    }

    fn diagnostics(&mut self) -> WlanDiagnostics {
        WlanDiagnostics { status: self.w.status(self.now), counters: self.w.counters.clone(), log: self.w.log_lines() }
    }
}
