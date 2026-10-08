//! IP allowlist / blocklist middleware — gate routes by client IP.
//!
//! Prefer `allow_only` for anything sensitive. An allowlist refuses
//! every address you did not name; a blocklist only stops the ones you
//! already know about, so a new address walks straight in.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::ip_filter::{IpFilterLayer, IpFilterRouterExt};
//!
//! // Allow only internal admin network
//! let admin_router = Router::new()
//!     .route("/__admin", get(admin_index))
//!     .ip_filter(IpFilterLayer::allow_only(vec!["10.0.0.0/8", "192.168.0.0/16"])?);
//!
//! // Block known abusers
//! let public_router = Router::new()
//!     .route("/api/posts", get(list_posts))
//!     .ip_filter(IpFilterLayer::block(vec!["203.0.113.42"])?);
//! ```
//!
//! The address comes from `ConnectInfo`, the real TCP peer. Serve with
//! `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())`.
//! Without it there is no address at all, and every request takes the
//! `allow_no_ip` path.
//!
//! Behind a proxy the peer is the proxy, not the client. To gate the
//! client instead, call [`IpFilterLayer::behind_trusted_proxy`]. This
//! filter never reads a forwarded header itself: any client can forge one.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

#[derive(Debug, thiserror::Error)]
pub enum IpFilterError {
    #[error("invalid CIDR or IP: {0}")]
    InvalidCidr(String),
}

/// Filter mode.
#[derive(Clone, Debug)]
enum Mode {
    /// Allow only IPs in `nets`. Reject everything else.
    AllowOnly(Vec<CidrRange>),
    /// Block IPs in `nets`. Allow everything else.
    Block(Vec<CidrRange>),
}

/// Which address the filter checks.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// The socket peer from `ConnectInfo`. The default.
    Peer,
    /// [`crate::rate_limit::client_ip`]: a `TrustedRealIp`, else the peer.
    ClientIp,
}

/// Configuration for the IP filter.
#[derive(Clone)]
pub struct IpFilterLayer {
    mode: Mode,
    source: Source,
    /// What to do when the request has no `ConnectInfo`. `false`
    /// (the default for `allow_only`) denies; `block` sets it `true`.
    pub allow_no_ip: bool,
}

impl IpFilterLayer {
    /// Allow ONLY these CIDR ranges or single IPs. All others get 403.
    ///
    /// # Errors
    /// [`IpFilterError::InvalidCidr`] if any entry doesn't parse.
    pub fn allow_only<I, S>(nets: I) -> Result<Self, IpFilterError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let nets = parse_all(nets)?;
        Ok(Self {
            mode: Mode::AllowOnly(nets),
            source: Source::Peer,
            allow_no_ip: false,
        })
    }

    /// BLOCK these CIDR ranges or single IPs. All others pass.
    ///
    /// # Errors
    /// [`IpFilterError::InvalidCidr`] if any entry doesn't parse.
    pub fn block<I, S>(nets: I) -> Result<Self, IpFilterError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let nets = parse_all(nets)?;
        Ok(Self {
            mode: Mode::Block(nets),
            source: Source::Peer,
            allow_no_ip: true,
        })
    }

    /// When `true`, requests with no `ConnectInfo` pass. When `false`
    /// they are rejected. `allow_only` defaults to `false`, so it
    /// fails closed.
    #[must_use]
    pub fn allow_no_ip(mut self, yes: bool) -> Self {
        self.allow_no_ip = yes;
        self
    }

    /// Check the client IP a `RealIpLayer` with `trust_proxies` resolved,
    /// not the socket peer; requests from other peers still use the peer.
    ///
    /// A peer inside a trusted range can forge `X-Forwarded-For`, so list
    /// only proxy egress addresses there; `X-Real-IP` and `CF-Connecting-IP`
    /// are taken as sent. The filter must sit inside the `RealIpLayer`, or
    /// it checks the proxy address and a block-list fails open.
    #[must_use]
    pub fn behind_trusted_proxy(mut self) -> Self {
        self.source = Source::ClientIp;
        self
    }

    /// Decide whether to allow a request from `ip`.
    fn allow(&self, ip: Option<IpAddr>) -> bool {
        let Some(ip) = ip else {
            return self.allow_no_ip;
        };
        match &self.mode {
            Mode::AllowOnly(nets) => nets.iter().any(|n| n.contains(ip)),
            Mode::Block(nets) => !nets.iter().any(|n| n.contains(ip)),
        }
    }
}

/// Extension trait — `.ip_filter(layer)` on Router.
pub trait IpFilterRouterExt {
    #[must_use]
    fn ip_filter(self, layer: IpFilterLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> IpFilterRouterExt for Router<S> {
    fn ip_filter(self, layer: IpFilterLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<IpFilterLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    let ip = match cfg.source {
        Source::Peer => req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.ip()),
        Source::ClientIp => crate::rate_limit::client_ip(req.extensions(), req.headers()),
    };
    if cfg.allow(ip) {
        next.run(req).await
    } else {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("forbidden"))
            .unwrap()
    }
}

// ------------------------------------------------------------------ CIDR parsing

pub(crate) use crate::cidr::CidrRange;

pub(crate) fn parse_all<I, S>(nets: I) -> Result<Vec<CidrRange>, IpFilterError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    nets.into_iter().map(|s| parse_cidr(s.as_ref())).collect()
}

fn parse_cidr(s: &str) -> Result<CidrRange, IpFilterError> {
    CidrRange::parse(s).ok_or_else(|| IpFilterError::InvalidCidr(s.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ip4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }

    fn ip6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    #[test]
    fn parse_single_ipv4() {
        let r = parse_cidr("192.168.1.1").unwrap();
        assert!(r.contains(ip4("192.168.1.1")));
        assert!(!r.contains(ip4("192.168.1.2")));
    }

    #[test]
    fn parse_ipv4_cidr() {
        let r = parse_cidr("10.0.0.0/8").unwrap();
        assert!(r.contains(ip4("10.0.0.1")));
        assert!(r.contains(ip4("10.255.255.255")));
        assert!(!r.contains(ip4("11.0.0.0")));
    }

    #[test]
    fn parse_ipv6_cidr() {
        let r = parse_cidr("fe80::/10").unwrap();
        assert!(r.contains(ip6("fe80::1")));
        assert!(!r.contains(ip6("2001::1")));
    }

    #[test]
    fn parse_zero_prefix_matches_all() {
        let r = parse_cidr("0.0.0.0/0").unwrap();
        assert!(r.contains(ip4("1.2.3.4")));
        assert!(r.contains(ip4("255.255.255.255")));
    }

    #[test]
    fn parse_invalid_returns_error() {
        assert!(parse_cidr("not-an-ip").is_err());
        assert!(parse_cidr("192.168.1.1/33").is_err());
        assert!(parse_cidr("::/129").is_err());
    }

    #[test]
    fn allow_only_passes_listed_ips() {
        let l = IpFilterLayer::allow_only(vec!["10.0.0.0/8"]).unwrap();
        assert!(l.allow(Some(ip4("10.1.2.3"))));
        assert!(!l.allow(Some(ip4("11.0.0.1"))));
    }

    #[test]
    fn allow_only_rejects_unlisted_ips() {
        let l = IpFilterLayer::allow_only(vec!["192.168.0.0/16"]).unwrap();
        assert!(!l.allow(Some(ip4("8.8.8.8"))));
    }

    #[test]
    fn block_rejects_listed_ips() {
        let l = IpFilterLayer::block(vec!["203.0.113.42"]).unwrap();
        assert!(!l.allow(Some(ip4("203.0.113.42"))));
        assert!(l.allow(Some(ip4("203.0.113.43"))));
    }

    #[test]
    fn block_passes_unlisted_ips() {
        let l = IpFilterLayer::block(vec!["10.0.0.0/8"]).unwrap();
        assert!(l.allow(Some(ip4("8.8.8.8"))));
    }

    #[test]
    fn allow_only_no_ip_fails_closed_by_default() {
        let l = IpFilterLayer::allow_only(vec!["10.0.0.0/8"]).unwrap();
        assert!(!l.allow(None));
    }

    #[test]
    fn block_no_ip_fails_open_by_default() {
        let l = IpFilterLayer::block(vec!["10.0.0.0/8"]).unwrap();
        assert!(l.allow(None));
    }

    #[test]
    fn allow_no_ip_override() {
        let l = IpFilterLayer::allow_only(vec!["10.0.0.0/8"])
            .unwrap()
            .allow_no_ip(true);
        assert!(l.allow(None));
    }

    #[test]
    fn cross_family_does_not_match() {
        // IPv4 CIDR shouldn't match IPv6 addresses
        let l = IpFilterLayer::allow_only(vec!["10.0.0.0/8"]).unwrap();
        assert!(!l.allow(Some(ip6("::1"))));
    }

    /// A dual-stack listener reports IPv4 peers as `::ffff:a.b.c.d`.
    #[test]
    fn ipv4_mapped_peer_matches_ipv4_blocklist() {
        let l = IpFilterLayer::block(vec!["203.0.113.42"]).unwrap();
        assert!(!l.allow(Some(ip6("::ffff:203.0.113.42"))));
    }

    #[test]
    fn ipv4_mapped_peer_matches_ipv4_allowlist() {
        let l = IpFilterLayer::allow_only(vec!["10.0.0.0/8"]).unwrap();
        assert!(l.allow(Some(ip6("::ffff:10.1.2.3"))));
        assert!(!l.allow(Some(ip6("::ffff:11.1.2.3"))));
    }

    #[test]
    fn ipv4_mapped_cidr_matches_plain_ipv4() {
        let l = IpFilterLayer::block(vec!["::ffff:10.0.0.0/104"]).unwrap();
        assert!(!l.allow(Some(ip4("10.200.0.1"))));
        assert!(l.allow(Some(ip4("11.0.0.1"))));
    }

    #[test]
    fn wide_ipv6_rule_matches_mapped_peer() {
        let l = IpFilterLayer::block(vec!["::/0"]).unwrap();
        assert!(!l.allow(Some(ip6("::ffff:203.0.113.7"))));
        assert!(l.allow(Some(ip4("203.0.113.7"))));
    }

    /// Status for a request from `peer` claiming `xff`, through `app`.
    async fn status(app: Router, peer: &str, xff: &str) -> StatusCode {
        use tower::ServiceExt;
        let mut req = Request::builder()
            .uri("/")
            .header("x-forwarded-for", xff)
            .body(Body::empty())
            .unwrap();
        let addr: std::net::SocketAddr = format!("{peer}:4000").parse().unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(addr));
        app.oneshot(req).await.unwrap().status()
    }

    fn filtered(layer: IpFilterLayer) -> Router {
        Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .ip_filter(layer)
    }

    /// `filtered(layer)` inside a `RealIpLayer` trusting 10.0.0.0/8.
    fn proxied(layer: IpFilterLayer) -> Router {
        use crate::real_ip::{RealIpLayer, RealIpRouterExt as _};
        let real_ip = RealIpLayer::default()
            .trust_proxies(["10.0.0.0/8"])
            .unwrap();
        filtered(layer).real_ip(real_ip)
    }

    fn client_only() -> IpFilterLayer {
        IpFilterLayer::allow_only(vec!["203.0.113.7"]).unwrap()
    }

    /// #2278: opted in, the filter gates the trusted client, not the proxy.
    #[tokio::test]
    async fn behind_trusted_proxy_filters_the_client_ip() {
        let app = || proxied(client_only().behind_trusted_proxy());
        assert_eq!(
            status(app(), "10.0.0.2", "203.0.113.7").await,
            StatusCode::OK
        );
        let denied = status(app(), "10.0.0.2", "198.51.100.1").await;
        assert_eq!(denied, StatusCode::FORBIDDEN);
        // A peer that is not a named proxy cannot claim the address.
        let forged = status(app(), "198.51.100.1", "203.0.113.7").await;
        assert_eq!(forged, StatusCode::FORBIDDEN);
    }

    /// By default the peer is checked, so an allow-list of the proxy keeps working.
    #[tokio::test]
    async fn by_default_the_peer_is_filtered() {
        let lb = || IpFilterLayer::allow_only(vec!["10.0.0.0/8"]).unwrap();
        assert_eq!(
            status(proxied(lb()), "10.0.0.2", "198.51.100.1").await,
            StatusCode::OK
        );
        let forged = status(proxied(client_only()), "10.0.0.2", "203.0.113.7").await;
        assert_eq!(forged, StatusCode::FORBIDDEN);
        let direct = status(filtered(client_only()), "203.0.113.7", "1.1.1.1").await;
        assert_eq!(direct, StatusCode::OK);
    }
}
