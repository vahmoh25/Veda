//! Route selection: which interface carries traffic to a destination.
//!
//! 1. Loopback addresses go to `lo`.
//! 2. A destination inside a connected network (an interface's IPv4
//!    subnet or IPv6 prefix, including link-local) goes to that interface.
//! 3. Anything else follows the default route of the best interface: the
//!    lowest metric among interfaces whose link is up and that have a
//!    gateway and an address of the destination's family (Ethernet beats
//!    Wi-Fi when both are connected).

use core::net::IpAddr;

use smoltcp::wire::IpCidr;
use vproto::net::NetError;

use crate::{IfaceId, LOOPBACK, Stack};

impl Stack {
    /// Picks the interface for traffic to `dest`.
    pub(crate) fn route(&self, dest: IpAddr) -> Result<IfaceId, NetError> {
        if dest.is_loopback() {
            return Ok(LOOPBACK);
        }
        if dest.is_unspecified() || dest.is_multicast() || matches!(dest, IpAddr::V4(a) if a.is_broadcast()) {
            return Err(NetError::InvalidArgument);
        }
        let mut connected: Option<(u32, IfaceId)> = None;
        let mut default: Option<(u32, IfaceId)> = None;
        let mut any_up = false;
        for id in self.iface_ids() {
            if id == LOOPBACK {
                continue;
            }
            let i = self.iface(id).unwrap();
            if !i.link_up {
                continue;
            }
            any_up = true;
            let on_link = match dest {
                IpAddr::V4(d) => i.v4.as_ref().is_some_and(|v| v.address.contains_addr(&d)),
                IpAddr::V6(d) => i.iface.ip_addrs().iter().any(|c| match c {
                    IpCidr::Ipv6(c) => c.contains_addr(&d),
                    _ => false,
                }),
            };
            if on_link && connected.is_none_or(|(m, _)| i.metric < m) {
                connected = Some((i.metric, id));
            }
            let has_default = match dest {
                IpAddr::V4(_) => i.v4.as_ref().is_some_and(|v| v.gateway.is_some()),
                IpAddr::V6(_) => i.v6_gateway().is_some() && i.has_global_v6(),
            };
            if has_default && default.is_none_or(|(m, _)| i.metric < m) {
                default = Some((i.metric, id));
            }
        }
        if let Some((_, id)) = connected.or(default) {
            return Ok(id);
        }
        Err(if any_up { NetError::NoRoute } else { NetError::NetworkDown })
    }

    /// The interface of the default route and its gateway (IPv4 preferred).
    pub(crate) fn default_interface(&self) -> Option<(IfaceId, IpAddr)> {
        let mut best: Option<(u32, bool, IfaceId, IpAddr)> = None;
        for id in self.iface_ids() {
            let i = self.iface(id).unwrap();
            if id == LOOPBACK || !i.link_up {
                continue;
            }
            let gw =
                i.v4.as_ref()
                    .and_then(|v| v.gateway)
                    .map(|g| (true, IpAddr::V4(g)))
                    .or_else(|| i.v6_gateway().filter(|_| i.has_global_v6()).map(|g| (false, IpAddr::V6(g))));
            if let Some((v4, gw)) = gw {
                // Lower metric wins; at equal metric IPv4 wins.
                let better = match best {
                    None => true,
                    Some((m, b4, _, _)) => i.metric < m || (i.metric == m && v4 && !b4),
                };
                if better {
                    best = Some((i.metric, v4, id, gw));
                }
            }
        }
        best.map(|(_, _, id, gw)| (id, gw))
    }

    /// Whether IPv6 destinations are reachable beyond the local link.
    pub(crate) fn has_ipv6_route(&self) -> bool {
        self.ifaces.iter().flatten().any(|i| i.link_up && i.v6_gateway().is_some() && i.has_global_v6())
    }
}
