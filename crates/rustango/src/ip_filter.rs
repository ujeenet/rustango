//! IP allowlist / blocklist middleware — gate routes by client IP.
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
//! Requires `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())`
//! to populate `ConnectInfo` — without that, every request matches the
//! `<no-ip>` fallback (see `default_decision`).

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

/// Configuration for the IP filter.
#[derive(Clone)]
pub struct IpFilterLayer {
    mode: Mode,
    /// What to do when the request has no `ConnectInfo`. Default `false`
    /// (deny — fail closed for AllowOnly, allow for Block).
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
            allow_no_ip: true,
        })
    }

    /// When `true`, requests without `ConnectInfo` are allowed through.
    /// When `false`, they're rejected for AllowOnly and allowed for Block.
    /// Default `false` (fail-closed).
    #[must_use]
    pub fn allow_no_ip(mut self, yes: bool) -> Self {
        self.allow_no_ip = yes;
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
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.ip());
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

#[derive(Debug, Clone, Copy)]
enum CidrRange {
    V4 { addr: u32, mask: u32 },
    V6 { addr: u128, mask: u128 },
}

impl CidrRange {
    fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Self::V4 { addr, mask }, IpAddr::V4(v4)) => u32::from(v4) & mask == *addr & mask,
            (Self::V6 { addr, mask }, IpAddr::V6(v6)) => u128::from(v6) & mask == *addr & mask,
            _ => false, // address family mismatch
        }
    }
}

fn parse_all<I, S>(nets: I) -> Result<Vec<CidrRange>, IpFilterError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    nets.into_iter().map(|s| parse_cidr(s.as_ref())).collect()
}

fn parse_cidr(s: &str) -> Result<CidrRange, IpFilterError> {
    let (ip_str, prefix) = match s.split_once('/') {
        Some((ip, p)) => (ip, Some(p)),
        None => (s, None),
    };
    let ip: IpAddr = ip_str
        .parse()
        .map_err(|_| IpFilterError::InvalidCidr(s.to_owned()))?;

    match ip {
        IpAddr::V4(v4) => {
            let bits: u32 = match prefix {
                Some(p) => p
                    .parse()
                    .map_err(|_| IpFilterError::InvalidCidr(s.to_owned()))?,
                None => 32,
            };
            if bits > 32 {
                return Err(IpFilterError::InvalidCidr(s.to_owned()));
            }
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            Ok(CidrRange::V4 {
                addr: u32::from(v4) & mask,
                mask,
            })
        }
        IpAddr::V6(v6) => {
            let bits: u32 = match prefix {
                Some(p) => p
                    .parse()
                    .map_err(|_| IpFilterError::InvalidCidr(s.to_owned()))?,
                None => 128,
            };
            if bits > 128 {
                return Err(IpFilterError::InvalidCidr(s.to_owned()));
            }
            let mask = if bits == 0 {
                0u128
            } else {
                u128::MAX << (128 - bits)
            };
            Ok(CidrRange::V6 {
                addr: u128::from(v6) & mask,
                mask,
            })
        }
    }
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
}
