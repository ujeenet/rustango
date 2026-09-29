//! Outbound HTTP to URLs that config can set: tenant SSO issuers,
//! Slack hooks, webhook subscribers (#1670, #1716).
//!
//! [`CheckedTarget`] is the only way to get a client here: it refuses
//! non-public addresses, pins the connection to the checked ones and
//! never follows redirects. List private hosts and CIDRs in
//! `RUSTANGO_OUTBOUND_ALLOW` to reach them, such as an IdP on your own
//! network. Webhook delivery ignores that list.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::cidr::CidrRange;

/// Env var: comma-separated hosts and CIDRs that SSO and Slack calls may
/// reach even when private, e.g. `10.0.5.0/24,idp.internal`.
#[cfg(any(
    test,
    feature = "oauth2",
    all(feature = "notifications", feature = "http-client")
))]
pub(crate) const ALLOW_ENV: &str = "RUSTANGO_OUTBOUND_ALLOW";

/// Private targets an operator allowed. A host entry matches the URL
/// host; a CIDR entry must cover every address the host resolves to.
#[derive(Debug, Clone, Default)]
pub(crate) struct Allowlist {
    hosts: Vec<String>,
    nets: Vec<CidrRange>,
}

impl Allowlist {
    /// Parse a comma-separated list. Bad entries are skipped, so they
    /// allow nothing.
    #[cfg(any(
        test,
        feature = "oauth2",
        all(feature = "notifications", feature = "http-client")
    ))]
    pub(crate) fn parse(spec: &str) -> Self {
        let mut list = Self::default();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let bare = entry.trim_start_matches('[').trim_end_matches(']');
            if let Some(net) = CidrRange::parse(bare) {
                list.nets.push(net);
            } else if entry.contains(['/', ':', '[', ']', ' ']) {
                tracing::warn!(entry, "{ALLOW_ENV}: skipping bad entry");
            } else {
                list.hosts.push(normalize_host(entry));
            }
        }
        list
    }

    fn allows_host(&self, host: &str) -> bool {
        let host = normalize_host(host);
        self.hosts.iter().any(|h| *h == host)
    }

    fn allows_ip(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|n| n.contains(ip))
    }
}

fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Held by tests that set or read [`ALLOW_ENV`].
#[cfg(test)]
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Which addresses a target may resolve to.
#[derive(Debug, Clone)]
pub(crate) enum TargetPolicy {
    /// Public addresses, plus what the allowlist names.
    Public(Allowlist),
    /// No address check.
    #[cfg_attr(not(any(test, feature = "webhook-delivery")), allow(dead_code))]
    AllowPrivate,
}

impl TargetPolicy {
    /// Public addresses only.
    #[cfg_attr(not(any(test, feature = "webhook-delivery")), allow(dead_code))]
    pub(crate) fn public_only() -> Self {
        Self::Public(Allowlist::default())
    }

    /// Public addresses plus the operator's [`ALLOW_ENV`] list.
    #[cfg(any(
        feature = "oauth2",
        all(feature = "notifications", feature = "http-client")
    ))]
    pub(crate) fn from_env() -> Self {
        Self::Public(Allowlist::parse(
            &std::env::var(ALLOW_ENV).unwrap_or_default(),
        ))
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
    /// Host name pinned to its checked (or allowlisted) addresses.
    Pinned(String, Vec<SocketAddr>),
}

/// A URL whose target passed [`TargetPolicy`].
pub(crate) struct CheckedTarget {
    url: reqwest::Url,
    route: Route,
}

impl CheckedTarget {
    /// Parse `url` and check every address it resolves to.
    pub(crate) async fn check(url: &str, policy: &TargetPolicy) -> Result<Self, TargetError> {
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
            TargetPolicy::Public(allow) => checked_route(&url, allow).await?,
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

async fn checked_route(url: &reqwest::Url, allow: &Allowlist) -> Result<Route, TargetError> {
    let host = url
        .host_str()
        .ok_or_else(|| TargetError::Invalid("target url has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        if refused_ip(ip, allow) {
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
    pinned_route(host, addrs, allow)
}

/// Check a host's resolved addresses. An allowlisted host skips the
/// private-range check but never the cloud-metadata one (#1796).
fn pinned_route(
    host: &str,
    addrs: Vec<SocketAddr>,
    allow: &Allowlist,
) -> Result<Route, TargetError> {
    let host_allowed = allow.allows_host(host);
    let refused = |ip| is_metadata_ip(ip) || (!host_allowed && refused_ip(ip, allow));
    if addrs.iter().any(|a| refused(a.ip())) {
        return Err(TargetError::Blocked);
    }
    Ok(Route::Pinned(host.to_owned(), addrs))
}

fn refused_ip(ip: IpAddr, allow: &Allowlist) -> bool {
    is_metadata_ip(ip) || (is_blocked_ip(ip) && !allow.allows_ip(ip))
}

/// Cloud instance-metadata endpoints (AWS, ECS, GCP, Azure, Oracle, Alibaba).
/// No allowlist entry opens them: they hand out credentials.
const METADATA_V4: [Ipv4Addr; 3] = [
    Ipv4Addr::new(169, 254, 169, 254),
    Ipv4Addr::new(169, 254, 170, 2),
    Ipv4Addr::new(100, 100, 100, 200),
];
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

fn is_metadata_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => METADATA_V4.contains(&v4),
        IpAddr::V6(v6) => {
            v6 == METADATA_V6 || embedded_v4(v6).iter().any(|v4| METADATA_V4.contains(v4))
        }
    }
}

/// IPv4 addresses an IPv6 address carries: mapped, NAT64, translated,
/// 6to4 and Teredo (server and client).
fn embedded_v4(v6: Ipv6Addr) -> Vec<Ipv4Addr> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return vec![v4];
    }
    let seg = v6.segments();
    let v4 = |hi: u16, lo: u16| {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    // NAT64 64:ff9b::/96 and IPv4-translated ::ffff:0:0:0/96.
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || seg[..6] == [0, 0, 0, 0, 0xffff, 0] {
        return vec![v4(seg[6], seg[7])];
    }
    // 6to4 2002::/16 carries the IPv4 in bits 16..48.
    if seg[0] == 0x2002 {
        return vec![v4(seg[1], seg[2])];
    }
    // Teredo 2001::/32: server IPv4, then the client IPv4 XOR'd.
    if seg[0] == 0x2001 && seg[1] == 0 {
        return vec![v4(seg[2], seg[3]), v4(!seg[6], !seg[7])];
    }
    Vec::new()
}

/// Loopback, private, link-local, CGNAT, multicast, unspecified and
/// other non-public ranges. IPv4 embedded in IPv6 is checked as IPv4.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            let inner = embedded_v4(v6);
            if !inner.is_empty() {
                return inner.into_iter().any(is_blocked_v4);
            }
            let seg = v6.segments();
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
            let err = CheckedTarget::check(url, &TargetPolicy::public_only())
                .await
                .err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
        assert!(
            CheckedTarget::check("http://10.0.0.1/", &TargetPolicy::AllowPrivate)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn allowlist_opens_only_what_it_names() {
        let policy = TargetPolicy::Public(Allowlist::parse(
            " 10.0.5.0/24, IdP.Internal., localhost, bad/entry:1 ",
        ));
        for url in [
            "http://10.0.5.7/token",
            "http://[::ffff:10.0.5.7]/",
            "http://localhost:9/",
        ] {
            assert!(CheckedTarget::check(url, &policy).await.is_ok(), "{url}");
        }
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.6.1/",
            "http://127.0.0.1/",
        ] {
            let err = CheckedTarget::check(url, &policy).await.err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
    }

    /// Metadata addresses stay refused whatever the allowlist says (#1796).
    #[tokio::test]
    async fn allowlist_never_opens_cloud_metadata() {
        let allow =
            Allowlist::parse("metadata.google.internal,169.254.0.0/16,100.64.0.0/10,fd00::/8");
        let at = |ip: &str| vec![SocketAddr::new(ip.parse().unwrap(), 80)];
        for ip in [
            "169.254.169.254",
            "fd00:ec2::254",
            "100.100.100.200",
            "::ffff:169.254.169.254",
        ] {
            let route = pinned_route("metadata.google.internal", at(ip), &allow);
            assert!(matches!(route, Err(TargetError::Blocked)), "{ip}");
        }
        assert!(pinned_route("metadata.google.internal", at("169.254.1.1"), &allow).is_ok());
        let policy = TargetPolicy::Public(allow);
        for url in [
            "http://169.254.169.254/",
            "http://[fd00:ec2::254]/",
            "http://100.100.100.200/",
        ] {
            let err = CheckedTarget::check(url, &policy).await.err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
        assert!(CheckedTarget::check("http://169.254.1.1/", &policy)
            .await
            .is_ok());
    }

    #[test]
    fn allowlist_host_match_is_exact() {
        let list = Allowlist::parse("idp.internal,[fd00::1],idp.internal:8443");
        assert!(list.allows_host("IDP.internal"));
        assert!(!list.allows_host("evil.idp.internal"));
        assert!(!list.allows_host("idp.internal.evil.com"));
        assert!(list.allows_ip("fd00::1".parse().unwrap()));
        assert_eq!(list.hosts, ["idp.internal"]);
    }
}
