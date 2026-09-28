//! Outbound HTTP to URLs that config can set: tenant SSO issuers,
//! Slack hooks, webhook subscribers (#1670, #1716).
//!
//! [`CheckedTarget`] is the only way to get a client here: it refuses
//! non-public addresses, pins the connection to the checked ones and
//! never follows redirects. Set `RUSTANGO_OUTBOUND_ALLOW_PRIVATE=1` to
//! reach private hosts, such as an IdP on your own network.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Env var that lets outbound calls reach private addresses.
pub(crate) const ALLOW_PRIVATE_ENV: &str = "RUSTANGO_OUTBOUND_ALLOW_PRIVATE";

/// Whether a target may resolve to a non-public address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetPolicy {
    PublicOnly,
    AllowPrivate,
}

impl TargetPolicy {
    /// `AllowPrivate` only when the operator set [`ALLOW_PRIVATE_ENV`].
    pub(crate) fn from_env() -> Self {
        match std::env::var(ALLOW_PRIVATE_ENV).as_deref() {
            Ok("1" | "true") => Self::AllowPrivate,
            _ => Self::PublicOnly,
        }
    }
}

/// Why a target was refused. No variant names the address: it may be
/// an internal one.
#[derive(Debug)]
pub(crate) enum TargetError {
    /// Bad URL, scheme or host; retrying will not help.
    Invalid(String),
    /// Resolves to a non-public address.
    Blocked,
    /// Name lookup failed; may be transient.
    Dns(String),
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => f.write_str(m),
            Self::Blocked => f.write_str("target resolves to a blocked address"),
            Self::Dns(m) => write!(f, "dns: {m}"),
        }
    }
}

enum Route {
    /// `AllowPrivate`: no address check.
    Unchecked,
    /// IP literal that passed the check.
    Direct,
    /// Host name pinned to its checked addresses.
    Pinned(String, Vec<SocketAddr>),
}

/// A URL whose target passed [`TargetPolicy`].
pub(crate) struct CheckedTarget {
    url: reqwest::Url,
    route: Route,
}

impl CheckedTarget {
    /// Parse `url` and check every address it resolves to.
    pub(crate) async fn check(url: &str, policy: TargetPolicy) -> Result<Self, TargetError> {
        let url = reqwest::Url::parse(url)
            .map_err(|e| TargetError::Invalid(format!("bad target url: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(TargetError::Invalid(format!(
                "scheme not allowed: {}",
                url.scheme()
            )));
        }
        let route = match policy {
            TargetPolicy::AllowPrivate => Route::Unchecked,
            TargetPolicy::PublicOnly => checked_route(&url).await?,
        };
        Ok(Self { url, route })
    }

    pub(crate) fn url(&self) -> &reqwest::Url {
        &self.url
    }

    /// Finish `builder`: no redirects and, when checked, no proxy and
    /// only the checked addresses.
    pub(crate) fn client(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> reqwest::Result<reqwest::Client> {
        let mut builder = builder.redirect(reqwest::redirect::Policy::none());
        match &self.route {
            Route::Unchecked => {}
            // A proxy would re-resolve the host.
            Route::Direct => builder = builder.no_proxy(),
            Route::Pinned(host, addrs) => {
                builder = builder.no_proxy().resolve_to_addrs(host, addrs)
            }
        }
        builder.build()
    }
}

/// Read at most `max` bytes of `resp`'s body, for an error message.
#[cfg(any(
    feature = "oauth2",
    all(feature = "notifications", feature = "http-client")
))]
pub(crate) async fn bounded_text(mut resp: reqwest::Response, max: usize) -> String {
    let mut buf = Vec::new();
    while buf.len() < max {
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    buf.truncate(max);
    String::from_utf8_lossy(&buf).into_owned()
}

async fn checked_route(url: &reqwest::Url) -> Result<Route, TargetError> {
    let host = url
        .host_str()
        .ok_or_else(|| TargetError::Invalid("target url has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(TargetError::Blocked);
        }
        return Ok(Route::Direct);
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| TargetError::Dns(e.to_string()))?
        .collect();
    if addrs.is_empty() {
        return Err(TargetError::Dns(format!("no addresses for {host}")));
    }
    if addrs.iter().any(|a| is_blocked_ip(a.ip())) {
        return Err(TargetError::Blocked);
    }
    Ok(Route::Pinned(host.to_owned(), addrs))
}

/// Loopback, private, link-local, CGNAT, multicast, unspecified and
/// other non-public ranges. IPv4 embedded in IPv6 is checked as IPv4.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_v4(v4);
            }
            let seg = v6.segments();
            let v4 = |hi: u16, lo: u16| {
                let [a, b] = hi.to_be_bytes();
                let [c, d] = lo.to_be_bytes();
                Ipv4Addr::new(a, b, c, d)
            };
            // NAT64 64:ff9b::/96 and IPv4-translated ::ffff:0:0:0/96.
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || seg[..6] == [0, 0, 0, 0, 0xffff, 0] {
                return is_blocked_v4(v4(seg[6], seg[7]));
            }
            // 6to4 2002::/16 carries the IPv4 in bits 16..48.
            if seg[0] == 0x2002 {
                return is_blocked_v4(v4(seg[1], seg[2]));
            }
            // Teredo 2001::/32: server IPv4, then the client IPv4 XOR'd.
            if seg[0] == 0x2001 && seg[1] == 0 {
                return is_blocked_v4(v4(seg[2], seg[3])) || is_blocked_v4(v4(!seg[6], !seg[7]));
            }
            v6.is_loopback()
                || seg[..3] == [0x64, 0xff9b, 1] // local-use NAT64 64:ff9b:1::/48
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (seg[0] & 0xffc0) == 0xfec0 // site-local fec0::/10
                || seg[..6] == [0, 0, 0, 0, 0, 0] // IPv4-compatible ::/96
        }
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0 // this network 0.0.0.0/8
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || (a == 100 && (b & 0xc0) == 64) // CGNAT 100.64.0.0/10
        || (a == 192 && b == 0 && c == 0) // 192.0.0.0/24
        || (a == 198 && (b & 0xfe) == 18) // benchmarking 198.18.0.0/15
        || a >= 240 // reserved 240.0.0.0/4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_ip_ranges() {
        for ip in [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "::ffff:0:7f00:1",              // IPv4-translated 127.0.0.1
            "64:ff9b:1::808:808",           // local-use NAT64
            "2002:7f00:1::1",               // 6to4 of 127.0.0.1
            "2002:a9fe:a9fe::1",            // 6to4 of 169.254.169.254
            "2001:0:808:808:0:0:80ff:fffe", // Teredo, client 127.0.0.1
            "2001:0:a00:1:0:0:f7f7:f7f7",   // Teredo, server 10.0.0.1
            "fec0::1",
            "::7f00:1",
            "100.127.255.255",
            "192.0.0.8",
            "198.18.0.1",
            "240.0.0.1",
        ] {
            assert!(is_blocked_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in [
            "93.184.216.34",
            "8.8.8.8",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "::ffff:0:808:808",
            "2002:808:808::1",
            "2001:0:808:808:0:0:f7f7:f7f7", // Teredo, client 8.8.8.8
        ] {
            assert!(
                !is_blocked_ip(ip.parse().unwrap()),
                "{ip} should be allowed"
            );
        }
    }

    #[tokio::test]
    async fn metadata_and_private_targets_are_refused() {
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://localhost/",
            "http://[::1]/",
        ] {
            let err = CheckedTarget::check(url, TargetPolicy::PublicOnly)
                .await
                .err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
        assert!(
            CheckedTarget::check("http://10.0.0.1/", TargetPolicy::AllowPrivate)
                .await
                .is_ok()
        );
    }
}
