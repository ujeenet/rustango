//! Outbound HTTP to URLs that config can set: tenant SSO issuers,
//! Slack hooks, webhook subscribers (#1670, #1716).
//!
//! [`Egress::check`] is the only way to send here: it refuses
//! non-public addresses and never follows redirects. Without a proxy the
//! client checks the addresses again at connect time. List private hosts and CIDRs in
//! `RUSTANGO_OUTBOUND_ALLOW` to reach them, such as an IdP on your own
//! network. Webhook delivery ignores that list. Set
//! `RUSTANGO_OUTBOUND_PROXY` to send every call through an egress proxy.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex};

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
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
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

    /// Every address; metadata endpoints stay refused (#1821).
    fn any_address() -> Self {
        let nets = ["0.0.0.0/0", "::/0"].map(|n| CidrRange::parse(n).expect("valid CIDR"));
        Self {
            hosts: Vec::new(),
            nets: nets.into(),
        }
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum TargetPolicy {
    /// Public addresses, plus what the allowlist names.
    Public(Allowlist),
    /// Private addresses too; cloud-metadata ones are still refused.
    #[cfg_attr(not(any(test, feature = "webhook-delivery")), allow(dead_code))]
    AllowPrivate,
}

impl TargetPolicy {
    /// The allowlist this policy checks addresses against.
    fn allowlist(&self) -> std::borrow::Cow<'_, Allowlist> {
        match self {
            Self::Public(allow) => std::borrow::Cow::Borrowed(allow),
            Self::AllowPrivate => std::borrow::Cow::Owned(Allowlist::any_address()),
        }
    }

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

impl std::error::Error for TargetError {}

/// Env var: the egress proxy every checked call goes through, e.g.
/// `http://proxy.internal:3128`. `HTTPS_PROXY` is never read.
pub(crate) const PROXY_ENV: &str = "RUSTANGO_OUTBOUND_PROXY";

fn proxy_from_env() -> Option<String> {
    parse_proxy(std::env::var(PROXY_ENV).ok().as_deref())
}

/// A [`PROXY_ENV`] value; blank means none.
fn parse_proxy(value: Option<&str>) -> Option<String> {
    let spec = value?.trim();
    (!spec.is_empty()).then(|| spec.to_owned())
}

type EgressKey = (TargetPolicy, Option<String>);

/// Clients by policy and proxy, so calls reuse connections and TLS setup.
#[derive(Default)]
pub(crate) struct EgressCache(Mutex<HashMap<EgressKey, reqwest::Client>>);

/// Clients built from a bare builder. They hold no tenant data.
static SHARED: LazyLock<EgressCache> = LazyLock::new(EgressCache::default);

/// The process-wide [`Egress`] for `policy`.
pub(crate) fn shared(policy: TargetPolicy) -> reqwest::Result<Egress> {
    SHARED.get(policy, reqwest::Client::builder)
}

impl EgressCache {
    /// The client for `policy` and the current [`PROXY_ENV`], built from
    /// `builder` on first use. The only way to get an [`Egress`].
    pub(crate) fn get(
        &self,
        policy: TargetPolicy,
        builder: impl FnOnce() -> reqwest::ClientBuilder,
    ) -> reqwest::Result<Egress> {
        self.get_via(policy, proxy_from_env(), builder)
    }

    fn get_via(
        &self,
        policy: TargetPolicy,
        proxy: Option<String>,
        builder: impl FnOnce() -> reqwest::ClientBuilder,
    ) -> reqwest::Result<Egress> {
        let key = (policy, proxy);
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let client = if let Some(c) = map.get(&key) {
            c.clone()
        } else {
            let client = build_client(&key.0, key.1.as_deref(), builder())?;
            // Keys change only with the env; stay bounded anyway.
            if map.len() >= 16 {
                map.clear();
            }
            map.insert(key.clone(), client.clone());
            client
        };
        Ok(Egress {
            proxied: key.1.is_some(),
            policy: key.0,
            client,
        })
    }
}

/// No redirects. Direct and checked, names resolve through
/// [`CheckingResolver`]. Proxied, the proxy is operator config and is
/// not checked.
fn build_client(
    policy: &TargetPolicy,
    proxy: Option<&str>,
    builder: reqwest::ClientBuilder,
) -> reqwest::Result<reqwest::Client> {
    // `no_proxy` also drops any proxy a `ClientConfig` hook added.
    let builder = builder
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(std::time::Duration::from_secs(30));
    let builder = match (proxy, policy) {
        (Some(url), _) => {
            warn_proxied_once();
            builder.proxy(reqwest::Proxy::all(url)?)
        }
        (None, policy) => {
            builder.dns_resolver(Arc::new(CheckingResolver(policy.allowlist().into_owned())))
        }
    };
    builder.build()
}

/// The proxy resolves targets again, so the app's check can be raced.
fn warn_proxied_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            target: "rustango::outbound",
            "outbound calls go through {PROXY_ENV}; the proxy must refuse private and \
             metadata addresses (169.254.0.0/16, 100.100.100.200, fd00:ec2::/32) itself"
        );
    });
}

/// Checks the addresses at connect time, so a pooled client never
/// reaches one the check did not see (DNS rebinding).
struct CheckingResolver(Allowlist);

impl reqwest::dns::Resolve for CheckingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow = self.0.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = checked_addrs(&host, lookup(&host, 0).await?, &allow)?;
            let addrs: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

/// A client that only sends to targets that pass its policy.
pub(crate) struct Egress {
    policy: TargetPolicy,
    client: reqwest::Client,
    proxied: bool,
}

impl Egress {
    /// Parse `url` and check it. Behind a proxy this is the only check:
    /// the proxy resolves the name itself.
    pub(crate) async fn check(&self, url: &str) -> Result<CheckedTarget, TargetError> {
        let url = check_url(url, &self.policy).await.map_err(|e| match e {
            // Fail closed: without local DNS nothing checks the target.
            TargetError::Dns(m) if self.proxied => TargetError::Dns(format!(
                "{m} (the target check needs local DNS even with {PROXY_ENV} set)"
            )),
            e => e,
        })?;
        Ok(CheckedTarget {
            url,
            client: self.client.clone(),
        })
    }
}

/// A URL that passed its [`Egress`] policy, with the client to send it.
pub(crate) struct CheckedTarget {
    url: reqwest::Url,
    client: reqwest::Client,
}

impl CheckedTarget {
    /// A request to the checked URL.
    pub(crate) fn request(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
        self.client.request(method, self.url.clone())
    }
}

async fn check_url(url: &str, policy: &TargetPolicy) -> Result<reqwest::Url, TargetError> {
    let url = reqwest::Url::parse(url)
        .map_err(|e| TargetError::Invalid(format!("bad target url: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(TargetError::Invalid(format!(
            "scheme not allowed: {}",
            url.scheme()
        )));
    }
    check_host(&url, &policy.allowlist()).await?;
    Ok(url)
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

/// Read `resp`'s whole body, or fail once it passes `max` bytes, so a
/// hostile server cannot make us buffer an unbounded answer.
#[cfg(feature = "oauth2")]
pub(crate) async fn capped_body(
    mut resp: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, BodyError> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(BodyError::Read)? {
        if buf.len() + chunk.len() > max {
            return Err(BodyError::TooLarge { max });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Why [`capped_body`] failed.
#[cfg(feature = "oauth2")]
#[derive(Debug)]
pub(crate) enum BodyError {
    TooLarge { max: usize },
    Read(reqwest::Error),
}

#[cfg(feature = "oauth2")]
impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { max } => write!(f, "response body exceeds {max} bytes"),
            Self::Read(e) => write!(f, "read body: {e}"),
        }
    }
}

async fn check_host(url: &reqwest::Url, allow: &Allowlist) -> Result<(), TargetError> {
    let host = url
        .host_str()
        .ok_or_else(|| TargetError::Invalid("target url has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return if refused_ip(ip, allow) {
            Err(TargetError::Blocked)
        } else {
            Ok(())
        };
    }
    checked_addrs(host, lookup(host, port).await?, allow).map(drop)
}

async fn lookup(host: &str, port: u16) -> Result<Vec<SocketAddr>, TargetError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| TargetError::Dns(e.to_string()))?
        .collect();
    if addrs.is_empty() {
        return Err(TargetError::Dns(format!("no addresses for {host}")));
    }
    Ok(addrs)
}

/// Check a host's resolved addresses. An allowlisted host skips the
/// private-range check but never the cloud-metadata one (#1796).
fn checked_addrs(
    host: &str,
    addrs: Vec<SocketAddr>,
    allow: &Allowlist,
) -> Result<Vec<SocketAddr>, TargetError> {
    let host_allowed = allow.allows_host(host);
    let refused = |ip| is_metadata_ip(ip) || (!host_allowed && refused_ip(ip, allow));
    if addrs.iter().any(|a| refused(a.ip())) {
        return Err(TargetError::Blocked);
    }
    Ok(addrs)
}

fn refused_ip(ip: IpAddr, allow: &Allowlist) -> bool {
    is_metadata_ip(ip) || (is_blocked_ip(ip) && !allow.allows_ip(ip))
}

/// Cloud instance-metadata endpoints (AWS, ECS, EKS Pod Identity, GCP, Azure,
/// Oracle, Alibaba). No allowlist entry opens them: they hand out credentials.
const METADATA_V4: [Ipv4Addr; 4] = [
    Ipv4Addr::new(169, 254, 169, 254),
    Ipv4Addr::new(169, 254, 170, 2),
    Ipv4Addr::new(169, 254, 170, 23),
    Ipv4Addr::new(100, 100, 100, 200),
];
const METADATA_V6: [Ipv6Addr; 2] = [
    Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254),
    Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x23),
];

fn is_metadata_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => METADATA_V4.contains(&v4),
        IpAddr::V6(v6) => {
            METADATA_V6.contains(&v6) || embedded_v4(v6).iter().any(|v4| METADATA_V4.contains(v4))
        }
    }
}

/// IPv4 addresses an IPv6 address carries: mapped, compatible, NAT64
/// (well-known and local-use), translated, 6to4 and Teredo (server and client).
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
    // NAT64 64:ff9b::/96, IPv4-translated ::ffff:0:0:0/96, and
    // IPv4-compatible ::/96 (not `::` or `::1`).
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
        || seg[..6] == [0, 0, 0, 0, 0xffff, 0]
        || (seg[..6] == [0; 6] && !v6.is_unspecified() && !v6.is_loopback())
    {
        return vec![v4(seg[6], seg[7])];
    }
    // Local-use NAT64 64:ff9b:1::/48, as the usual /96 prefix: IPv4 in the low 32 bits.
    if seg[..3] == [0x64, 0xff9b, 1] {
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
            let seg = v6.segments();
            // Local-use NAT64 64:ff9b:1::/48 and IPv4-compatible ::/96.
            if seg[..3] == [0x64, 0xff9b, 1] || seg[..6] == [0; 6] {
                return true;
            }
            let inner = embedded_v4(v6);
            if !inner.is_empty() {
                return inner.into_iter().any(is_blocked_v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (seg[0] & 0xffc0) == 0xfec0 // site-local fec0::/10
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
            let err = check_url(url, &TargetPolicy::public_only()).await.err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
        assert!(check_url("http://10.0.0.1/", &TargetPolicy::AllowPrivate)
            .await
            .is_ok());
        // Private targets never open cloud metadata (#1821).
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[::ffff:169.254.169.254]/",
            "http://100.100.100.200/",
        ] {
            let err = check_url(url, &TargetPolicy::AllowPrivate).await.err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
        let at = vec![SocketAddr::new("169.254.170.2".parse().unwrap(), 80)];
        let route = checked_addrs("x", at, &TargetPolicy::AllowPrivate.allowlist());
        assert!(matches!(route, Err(TargetError::Blocked)));
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
            assert!(check_url(url, &policy).await.is_ok(), "{url}");
        }
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.6.1/",
            "http://127.0.0.1/",
        ] {
            let err = check_url(url, &policy).await.err();
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
            let route = checked_addrs("metadata.google.internal", at(ip), &allow);
            assert!(matches!(route, Err(TargetError::Blocked)), "{ip}");
        }
        assert!(checked_addrs("metadata.google.internal", at("169.254.1.1"), &allow).is_ok());
        let policy = TargetPolicy::Public(allow);
        for url in [
            "http://169.254.169.254/",
            "http://[fd00:ec2::254]/",
            "http://100.100.100.200/",
        ] {
            let err = check_url(url, &policy).await.err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
        assert!(check_url("http://169.254.1.1/", &policy).await.is_ok());
    }

    /// Every IPv6 form carrying a metadata IPv4 is refused, even with its
    /// prefix allowlisted so the private-range check can't decide it.
    #[tokio::test]
    async fn allowlisted_prefixes_never_open_embedded_metadata() {
        let allow = Allowlist::parse(
            "meta.internal,64:ff9b::/96,64:ff9b:1::/48,2002::/16,2001::/32,::/96,fd00::/8",
        );
        let embedded = [
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::a9fe:a9fe",
            "2002:a9fe:a9fe::1",
            "2001:0:a9fe:a9fe::1",
            "2001:0:101:101::5601:5601",
            "::a9fe:a9fe",
            "::a9fe:aa17",
            "fd00:ec2::23",
        ];
        let at = |ip: &str| vec![SocketAddr::new(ip.parse().unwrap(), 80)];
        let policy = TargetPolicy::Public(allow.clone());
        for ip in embedded {
            let route = checked_addrs("meta.internal", at(ip), &allow);
            assert!(matches!(route, Err(TargetError::Blocked)), "{ip}");
            let url = format!("http://[{ip}]/");
            let err = check_url(&url, &policy).await.err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
        for ok in ["64:ff9b::a9fe:101", "::a9fe:101", "2002:a9fe:101::1"] {
            let url = format!("http://[{ok}]/");
            assert!(check_url(&url, &policy).await.is_ok(), "{url}");
        }
    }

    #[test]
    fn eks_pod_identity_is_metadata() {
        assert!(is_metadata_ip("169.254.170.23".parse().unwrap()));
        assert!(is_metadata_ip("fd00:ec2::23".parse().unwrap()));
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

    /// A proxy that answers every request itself and reports its request line.
    async fn fake_proxy() -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut r = BufReader::new(sock);
                    let mut line = String::new();
                    r.read_line(&mut line).await.unwrap();
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).await.unwrap() == 0 || h == "\r\n" {
                            break;
                        }
                    }
                    let status = if line.starts_with("CONNECT") {
                        "403 Forbidden"
                    } else {
                        "200 OK"
                    };
                    tx.send(line.trim_end().to_owned()).unwrap();
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    r.get_mut().write_all(resp.as_bytes()).await.ok();
                });
            }
        });
        (url, rx)
    }

    /// The next request line, failing fast instead of hanging.
    async fn next_line(lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        let line = tokio::time::timeout(std::time::Duration::from_secs(3), lines.recv()).await;
        line.expect("no request arrived").unwrap()
    }

    async fn get(egress: &Egress, url: &str) -> reqwest::Result<reqwest::Response> {
        let target = egress.check(url).await.unwrap();
        let req = target.request(reqwest::Method::GET);
        req.timeout(std::time::Duration::from_secs(3)).send().await
    }

    /// The proxy itself may be private; targets are still checked (#1792).
    #[tokio::test]
    async fn proxied_calls_check_the_target_and_go_to_the_proxy() {
        let (proxy, mut lines) = fake_proxy().await;
        let egress = EgressCache::default()
            .get_via(
                TargetPolicy::public_only(),
                Some(proxy),
                reqwest::Client::builder,
            )
            .unwrap();
        let resp = get(&egress, "http://93.184.216.34/hook").await.unwrap();
        assert_eq!(resp.status(), 200);
        let line = next_line(&mut lines).await;
        assert_eq!(line, "GET http://93.184.216.34/hook HTTP/1.1");
        assert!(get(&egress, "https://93.184.216.34/").await.is_err());
        let line = next_line(&mut lines).await;
        assert_eq!(line, "CONNECT 93.184.216.34:443 HTTP/1.1");
        for url in [
            "http://169.254.169.254/",
            "http://10.0.0.1/",
            "http://localhost/",
        ] {
            let err = egress.check(url).await.err();
            assert!(matches!(err, Some(TargetError::Blocked)), "{url}: {err:?}");
        }
        assert!(lines.try_recv().is_err());
    }

    #[test]
    fn proxy_value_parsing() {
        assert_eq!(parse_proxy(None), None);
        assert_eq!(parse_proxy(Some("  ")), None);
        assert_eq!(
            parse_proxy(Some(" http://p.internal:3128 ")).as_deref(),
            Some("http://p.internal:3128")
        );
    }

    /// Behind a proxy a local DNS failure says why it is fatal.
    #[tokio::test]
    async fn proxied_dns_failure_names_the_local_dns_need() {
        let egress = EgressCache::default()
            .get_via(
                TargetPolicy::public_only(),
                Some("http://127.0.0.1:9".into()),
                reqwest::Client::builder,
            )
            .unwrap();
        let err = egress.check("http://no-such-host.invalid/").await.err();
        assert!(
            matches!(&err, Some(TargetError::Dns(m)) if m.contains("needs local DNS")),
            "{err:?}"
        );
    }

    /// A named, allowlisted host connects through the checking resolver.
    #[tokio::test]
    async fn named_host_connects_through_the_checking_resolver() {
        let (server, mut lines) = fake_proxy().await;
        let port = server.rsplit(':').next().unwrap();
        let egress = EgressCache::default()
            .get_via(
                TargetPolicy::Public(Allowlist::parse("localhost")),
                None,
                reqwest::Client::builder,
            )
            .unwrap();
        let resp = get(&egress, &format!("http://localhost:{port}/named")).await;
        assert_eq!(resp.unwrap().status(), 200);
        assert_eq!(next_line(&mut lines).await, "GET /named HTTP/1.1");
    }

    /// A proxy a `ClientConfig` hook adds is dropped.
    #[tokio::test]
    async fn hook_proxy_is_ignored() {
        let (proxy, mut proxy_lines) = fake_proxy().await;
        let (server, mut lines) = fake_proxy().await;
        let egress = EgressCache::default()
            .get_via(TargetPolicy::AllowPrivate, None, || {
                reqwest::Client::builder().proxy(reqwest::Proxy::all(&proxy).unwrap())
            })
            .unwrap();
        let resp = get(&egress, &format!("{server}/direct")).await;
        assert_eq!(resp.unwrap().status(), 200);
        assert_eq!(next_line(&mut lines).await, "GET /direct HTTP/1.1");
        assert!(proxy_lines.try_recv().is_err());
    }

    const CHILD_ENV: &str = "RUSTANGO_TEST_SYSTEM_PROXY_CHILD";

    /// `HTTP(S)_PROXY` is ignored. The env is set only in a child test
    /// process, so no other test sees it.
    #[tokio::test]
    async fn system_proxy_env_is_ignored() {
        let (proxy, mut proxy_lines) = fake_proxy().await;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--ignored",
                "--exact",
                "outbound::tests::system_proxy_child",
            ])
            .env(CHILD_ENV, "1")
            // An inherited NO_PROXY=127.0.0.1 would bypass the proxy anyway.
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
        for var in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy"] {
            child.env(var, &proxy);
        }
        let out = tokio::task::spawn_blocking(move || child.output().unwrap())
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}");
        // The filter must have matched, or the child proved nothing.
        assert!(stdout.contains("1 passed"), "{stdout}");
        assert!(proxy_lines.try_recv().is_err());
    }

    #[tokio::test]
    #[ignore = "run by system_proxy_env_is_ignored"]
    async fn system_proxy_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let (server, mut lines) = fake_proxy().await;
        for policy in [
            TargetPolicy::AllowPrivate,
            TargetPolicy::Public(Allowlist::parse("127.0.0.1")),
        ] {
            let egress = EgressCache::default()
                .get_via(policy, None, reqwest::Client::builder)
                .unwrap();
            let resp = get(&egress, &format!("{server}/direct")).await;
            assert_eq!(resp.unwrap().status(), 200);
            assert_eq!(next_line(&mut lines).await, "GET /direct HTTP/1.1");
        }
    }

    /// A pooled client re-checks a name when it connects, so a name that
    /// re-resolved to a private address after `check` is still refused.
    #[tokio::test]
    async fn pooled_client_rechecks_names_at_connect() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let a = accepts.clone();
        tokio::spawn(async move {
            while listener.accept().await.is_ok() {
                a.fetch_add(1, Ordering::SeqCst);
            }
        });
        let egress = EgressCache::default()
            .get_via(TargetPolicy::public_only(), None, reqwest::Client::builder)
            .unwrap();
        // Skip `check`, as if DNS had changed since.
        let target = CheckedTarget {
            url: format!("http://localhost:{port}/").parse().unwrap(),
            client: egress.client.clone(),
        };
        let req = target.request(reqwest::Method::GET);
        let err = req
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_connect(), "{err}");
        assert_eq!(accepts.load(Ordering::SeqCst), 0);
    }
}
