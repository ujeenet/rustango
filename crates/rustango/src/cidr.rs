//! CIDR ranges shared by the IP filter, trusted proxies and the
//! outbound allowlist.

use std::net::IpAddr;

#[derive(Debug, Clone, Copy)]
pub(crate) enum CidrRange {
    V4 { addr: u32, mask: u32 },
    V6 { addr: u128, mask: u128 },
}

impl CidrRange {
    /// An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) matches IPv4 rules
    /// and any IPv6 rule covering it.
    pub(crate) fn contains(&self, ip: IpAddr) -> bool {
        match self {
            Self::V4 { addr, mask } => {
                matches!(ip.to_canonical(), IpAddr::V4(v4) if u32::from(v4) & mask == *addr & mask)
            }
            Self::V6 { addr, mask } => {
                matches!(ip, IpAddr::V6(v6) if u128::from(v6) & mask == *addr & mask)
            }
        }
    }

    /// Parse `a.b.c.d/n`, `x::/n` or a single address. `None` if bad.
    #[cfg_attr(
        not(any(
            test,
            feature = "admin",
            feature = "oauth2",
            all(feature = "notifications", feature = "http-client")
        )),
        allow(dead_code)
    )]
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let (ip_str, prefix) = match s.split_once('/') {
            Some((ip, p)) => (ip, Some(p)),
            None => (s, None),
        };
        let ip: IpAddr = ip_str.parse().ok()?;
        match ip {
            IpAddr::V4(v4) => {
                let bits: u32 = match prefix {
                    Some(p) => p.parse().ok()?,
                    None => 32,
                };
                if bits > 32 {
                    return None;
                }
                let mask = if bits == 0 {
                    0
                } else {
                    u32::MAX << (32 - bits)
                };
                Some(Self::V4 {
                    addr: u32::from(v4) & mask,
                    mask,
                })
            }
            IpAddr::V6(v6) => {
                let bits: u32 = match prefix {
                    Some(p) => p.parse().ok()?,
                    None => 128,
                };
                if bits > 128 {
                    return None;
                }
                // `::ffff:a.b.c.d/n` is an IPv4 range, matched as one.
                if let (Some(v4), true) = (v6.to_ipv4_mapped(), bits >= 96) {
                    return Self::parse(&format!("{v4}/{}", bits - 96));
                }
                let mask = if bits == 0 {
                    0u128
                } else {
                    u128::MAX << (128 - bits)
                };
                Some(Self::V6 {
                    addr: u128::from(v6) & mask,
                    mask,
                })
            }
        }
    }
}
