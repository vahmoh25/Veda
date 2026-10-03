//! Just enough of Ethernet, IPv4, IPv6 and UDP to recognise DHCP and DNS
//! traffic on the wired side, and to answer a DNS query with SERVFAIL (used
//! by the `dns servfail` condition).

/// What a frame on the wired side carries, as far as the simulated network
/// conditions care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// DHCP or DHCPv6.
    Dhcp,
    /// A DNS query (UDP to port 53).
    DnsQuery,
    Other,
}

const ETH_HDR: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_IPV6: u16 = 0x86DD;
const PROTO_UDP: u8 = 17;
const DNS_PORT: u16 = 53;

/// The UDP source and destination ports of an IPv4 or IPv6 packet in an
/// Ethernet frame, and the offset of the UDP header.
fn udp_ports(eth: &[u8]) -> Option<(u16, u16, usize)> {
    let ethertype = u16::from_be_bytes([*eth.get(12)?, *eth.get(13)?]);
    let ip = &eth[ETH_HDR..];
    let udp_at = match ethertype {
        ETHERTYPE_IPV4 => {
            if ip.len() < 20 || ip[0] >> 4 != 4 {
                return None;
            }
            let ihl = (ip[0] & 0xF) as usize * 4;
            // Only a packet's first fragment carries the UDP header.
            let fragment_offset = u16::from_be_bytes([ip[6], ip[7]]) & 0x1FFF;
            if ihl < 20 || ip[9] != PROTO_UDP || fragment_offset != 0 {
                return None;
            }
            ETH_HDR + ihl
        }
        ETHERTYPE_IPV6 => {
            if ip.len() < 40 || ip[0] >> 4 != 6 || ip[6] != PROTO_UDP {
                return None;
            }
            ETH_HDR + 40
        }
        _ => return None,
    };
    let udp = eth.get(udp_at..udp_at + 8)?;
    Some((u16::from_be_bytes([udp[0], udp[1]]), u16::from_be_bytes([udp[2], udp[3]]), udp_at))
}

pub fn classify(eth: &[u8]) -> Kind {
    const DHCP_PORTS: [u16; 4] = [67, 68, 546, 547];
    match udp_ports(eth) {
        Some((s, d, _)) if DHCP_PORTS.contains(&s) && DHCP_PORTS.contains(&d) => Kind::Dhcp,
        Some((_, DNS_PORT, _)) => Kind::DnsQuery,
        _ => Kind::Other,
    }
}

/// The length of a DNS question (name, type and class), if well formed.
fn question_len(q: &[u8]) -> Option<usize> {
    let mut i = 0;
    loop {
        let label = *q.get(i)? as usize;
        i += 1;
        if label == 0 {
            break;
        }
        // Questions from a stub resolver are never compressed.
        if label & 0xC0 != 0 || i + label > 255 {
            return None;
        }
        i += label;
    }
    (q.len() >= i + 4).then_some(i + 4)
}

/// The Internet checksum (RFC 1071) over `parts`.
fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    let mut odd: Option<u8> = None;
    for p in parts {
        for &b in p.iter() {
            match odd.take() {
                Some(hi) => sum += u16::from_be_bytes([hi, b]) as u32,
                None => odd = Some(b),
            }
        }
    }
    if let Some(hi) = odd {
        sum += u16::from_be_bytes([hi, 0]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// A SERVFAIL answer to a DNS query carried over IPv4 (`None` for anything
/// else). The answer comes from the address the query was sent to and
/// repeats the question.
pub fn dns_servfail(eth: &[u8]) -> Option<Vec<u8>> {
    let (src_port, dst_port, udp_at) = udp_ports(eth)?;
    if dst_port != DNS_PORT || u16::from_be_bytes([eth[12], eth[13]]) != ETHERTYPE_IPV4 {
        return None;
    }
    let ip = &eth[ETH_HDR..udp_at];
    let ip_total = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    let end = (ETH_HDR + ip_total).min(eth.len());
    let dns = eth.get(udp_at + 8..end)?;
    // A query (QR clear) with at most one question.
    if dns.len() < 12 || dns[2] & 0x80 != 0 {
        return None;
    }
    let questions = u16::from_be_bytes([dns[4], dns[5]]);
    let header_and_question = match questions {
        0 => 12,
        1 => 12 + question_len(&dns[12..])?,
        _ => return None,
    };
    let mut answer = dns[..header_and_question].to_vec();
    answer[2] = 0x80 | (dns[2] & 0x79); // QR, with the query's opcode and RD
    answer[3] = 0x80 | 2; // RA, RCODE = SERVFAIL
    answer[6..12].fill(0); // no answer, authority or additional records

    let udp_len = (8 + answer.len()) as u16;
    let mut udp = [0u8; 8];
    udp[0..2].copy_from_slice(&dst_port.to_be_bytes());
    udp[2..4].copy_from_slice(&src_port.to_be_bytes());
    udp[4..6].copy_from_slice(&udp_len.to_be_bytes());
    let (server, client) = (&ip[16..20], &ip[12..16]);
    let pseudo = [&[0, PROTO_UDP][..], &udp_len.to_be_bytes()];
    let sum = match checksum(&[server, client, pseudo[0], pseudo[1], &udp, &answer]) {
        0 => 0xFFFF,
        s => s,
    };
    udp[6..8].copy_from_slice(&sum.to_be_bytes());

    let mut iph = [0u8; 20];
    iph[0] = 0x45;
    iph[2..4].copy_from_slice(&(20 + udp_len).to_be_bytes());
    iph[4..6].copy_from_slice(&ip[4..6]);
    iph[6] = 0x40; // don't fragment
    iph[8] = 64;
    iph[9] = PROTO_UDP;
    iph[12..16].copy_from_slice(server);
    iph[16..20].copy_from_slice(client);
    let sum = checksum(&[&iph]);
    iph[10..12].copy_from_slice(&sum.to_be_bytes());

    let mut out = Vec::with_capacity(ETH_HDR + 20 + udp_len as usize);
    out.extend_from_slice(&eth[6..12]);
    out.extend_from_slice(&eth[0..6]);
    out.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    out.extend_from_slice(&iph);
    out.extend_from_slice(&udp);
    out.extend_from_slice(&answer);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Ethernet frame with an IPv4/UDP packet (header checksums left
    /// zero; nothing here checks them).
    fn udp4(src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0x52, 0x55, 10, 0, 2, 2, 0x02, 0x56, 0x57, 0xAA, 0, 1, 0x08, 0x00];
        let total = (20 + 8 + payload.len()) as u16;
        f.extend_from_slice(&[0x45, 0, (total >> 8) as u8, total as u8, 0x12, 0x34, 0, 0, 64, 17, 0, 0]);
        f.extend_from_slice(&[10, 0, 2, 15, 10, 0, 2, 3]);
        f.extend_from_slice(&src_port.to_be_bytes());
        f.extend_from_slice(&dst_port.to_be_bytes());
        f.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(payload);
        f
    }

    fn query(name: &[&str]) -> Vec<u8> {
        let mut q = vec![0xAB, 0xCD, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 1];
        for l in name {
            q.push(l.len() as u8);
            q.extend_from_slice(l.as_bytes());
        }
        q.extend_from_slice(&[0, 0, 1, 0, 1]);
        // An EDNS OPT record in the additional section.
        q.extend_from_slice(&[0, 0, 41, 0x04, 0xD0, 0, 0, 0, 0, 0, 0]);
        q
    }

    #[test]
    fn recognises_dhcp_and_dns() {
        assert_eq!(classify(&udp4(68, 67, &[1; 240])), Kind::Dhcp);
        assert_eq!(classify(&udp4(67, 68, &[2; 240])), Kind::Dhcp);
        assert_eq!(classify(&udp4(49152, 53, &query(&["example", "com"]))), Kind::DnsQuery);
        assert_eq!(classify(&udp4(53, 49152, &[0; 12])), Kind::Other);
        assert_eq!(classify(&udp4(49152, 443, &[0; 12])), Kind::Other);
        // DHCPv6 in IPv6.
        let mut v6 = vec![0x33, 0x33, 0, 1, 0, 2, 0x02, 0x56, 0x57, 0xAA, 0, 1, 0x86, 0xDD];
        v6.extend_from_slice(&[0x60, 0, 0, 0, 0, 12, 17, 1]);
        v6.extend_from_slice(&[0; 32]);
        v6.extend_from_slice(&[0x02, 0x22, 0x02, 0x23, 0, 12, 0, 0, 1, 2, 3, 4]);
        assert_eq!(classify(&v6), Kind::Dhcp);
        // Truncated and non-IP frames.
        assert_eq!(classify(&[0; 10]), Kind::Other);
        assert_eq!(classify(&udp4(68, 67, &[])[..30]), Kind::Other);
    }

    #[test]
    fn servfail_answer_is_well_formed() {
        let q = udp4(50000, 53, &query(&["www", "example", "com"]));
        let a = dns_servfail(&q).unwrap();
        // Addressed back to the querier.
        assert_eq!(a[0..6], q[6..12]);
        assert_eq!(a[6..12], q[0..6]);
        assert_eq!(a[26..30], q[30..34]);
        assert_eq!(a[30..34], q[26..30]);
        assert_eq!(u16::from_be_bytes([a[34], a[35]]), 53);
        assert_eq!(u16::from_be_bytes([a[36], a[37]]), 50000);
        // Both checksums verify (summing a correct header gives zero).
        assert_eq!(checksum(&[&a[14..34]]), 0);
        let udp_len = u16::from_be_bytes([a[38], a[39]]);
        assert_eq!(udp_len as usize, a.len() - 34);
        assert_eq!(checksum(&[&a[26..34], &[0, 17], &udp_len.to_be_bytes(), &a[34..]]), 0);
        // Same ID and question, SERVFAIL, no other records.
        let dns = &a[42..];
        assert_eq!(dns[0..2], [0xAB, 0xCD]);
        assert_eq!(dns[2] & 0x80, 0x80);
        assert_eq!(dns[3] & 0x0F, 2);
        assert_eq!(dns[4..12], [0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(dns.len(), 12 + 17 + 4);
    }

    #[test]
    fn servfail_is_only_built_for_queries() {
        let mut answer = query(&["example", "com"]);
        answer[2] |= 0x80;
        assert!(dns_servfail(&udp4(50000, 53, &answer)).is_none());
        assert!(dns_servfail(&udp4(50000, 80, &query(&["a"]))).is_none());
        assert!(dns_servfail(&udp4(50000, 53, &[0; 5])).is_none());
        // A question running past the end of the packet.
        assert!(dns_servfail(&udp4(50000, 53, &[0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 9, b'a'])).is_none());
        // Random bytes never panic.
        let mut s = 0x9E37_79B9u32;
        for _ in 0..2000 {
            let len = (s % 80) as usize;
            let mut p = Vec::with_capacity(len);
            for _ in 0..len {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                p.push(s as u8);
            }
            let f = udp4(50000, 53, &p);
            let _ = dns_servfail(&f);
            let _ = classify(&f[..f.len().min((s % 64) as usize)]);
        }
    }
}
