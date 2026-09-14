//! Real-IP extraction middleware for apps behind a trusted reverse proxy.
//!
//! `axum::extract::ConnectInfo<SocketAddr>` always reports the
//! immediate peer — useless when your app sits behind nginx /
//! Cloudflare / ELB. This middleware parses one of the common
//! forwarded-for headers and stuffs the resolved client IP into the
//! request extensions as a [`RealIp`] value.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::real_ip::{RealIpLayer, RealIpRouterExt, RealIp};
//! use axum::Extension;
//!
//! // Trust the immediate proxy; read the leftmost (= original client)
//! // entry in X-Forwarded-For.
//! let app = axum::Router::new()
//!     .route("/", axum::routing::get(home))
//!     .real_ip(RealIpLayer::default());
//!
//! async fn home(Extension(ip): Extension<RealIp>) -> String {
//!     format!("hi, {}", ip.0)
//! }
//! ```
//!
//! ## Important security note
//!
//! **Never trust forwarded-for headers from the open internet** — any
//! client can set them. Apply this layer ONLY when a proxy you
//! control terminates inbound requests and rewrites these headers,
//! and configure that proxy to scrub them on the way in.

use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::Router;

/// Resolved client IP. Stored in `request.extensions` by
/// [`RealIpLayer`]; pull it out via `axum::Extension<RealIp>` in your
/// handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealIp(pub IpAddr);

#[derive(Clone, Debug)]
pub enum HeaderStrategy {
    /// Standard `Forwarded: for=<ip>` (RFC 7239). Picks the leftmost
    /// `for=` parameter.
    ForwardedRfc7239,
    /// `X-Forwarded-For: client, proxy1, proxy2`. Picks the leftmost
    /// IP (= original client). Almost universally what reverse proxies
    /// emit.
    XForwardedFor,
    /// `X-Real-IP: <ip>`. Single value; some proxies (nginx) set this
    /// rather than X-Forwarded-For.
    XRealIp,
    /// Cloudflare's `CF-Connecting-IP`.
    CfConnectingIp,
    /// Try each strategy in order; first hit wins. Default.
    Auto,
}

/// A client IP resolved from a header sent by a **trusted** proxy — the
/// connecting socket matched [`RealIpLayer::trust_proxies`] (#1398).
///
/// [`RealIp`] says only "some header claimed this". Any client can claim
/// anything, so that value must never key a security decision. This one
/// carries the extra fact that the claim arrived over a hop the operator
/// declared trusted, which is what makes it safe for
/// [`crate::rate_limit`] to bucket on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedRealIp(pub IpAddr);

#[derive(Clone, Debug)]
pub struct RealIpLayer {
    pub strategy: HeaderStrategy,
    /// Networks whose forwarding headers are believed. `None` (the
    /// default) means none are, and only [`RealIp`] is inserted.
    trusted_proxies: Option<Vec<crate::ip_filter::CidrRange>>,
}

impl Default for RealIpLayer {
    fn default() -> Self {
        Self {
            strategy: HeaderStrategy::Auto,
            trusted_proxies: None,
        }
    }
}

impl RealIpLayer {
    #[must_use]
    pub fn new(strategy: HeaderStrategy) -> Self {
        Self {
            strategy,
            trusted_proxies: None,
        }
    }

    /// Declare which networks' forwarding headers to believe, so the
    /// resolved address can key a security decision (#1398).
    ///
    /// Without this, a forwarding header is just a claim by whoever sent
    /// it, and [`RealIp`] is only safe for logging. Name the addresses
    /// your ingress actually connects from — your load balancer, your
    /// nginx, your CDN's egress ranges — and a request arriving from one
    /// of them additionally gets a [`TrustedRealIp`], which
    /// [`crate::rate_limit::RateLimitLayer::per_ip`] will bucket on.
    ///
    /// A request from anywhere else is unaffected: it still gets
    /// `RealIp`, never `TrustedRealIp`, so a client cannot win itself a
    /// private rate-limit bucket by inventing an `X-Forwarded-For`.
    ///
    /// ```no_run
    /// # use rustango::real_ip::RealIpLayer;
    /// let layer = RealIpLayer::default()
    ///     .trust_proxies(["10.0.0.0/8", "172.16.0.0/12"])
    ///     .expect("valid CIDRs");
    /// ```
    ///
    /// # Errors
    /// [`crate::ip_filter::IpFilterError::InvalidCidr`] if an entry is
    /// not an IP or CIDR block.
    pub fn trust_proxies<I, S>(mut self, nets: I) -> Result<Self, crate::ip_filter::IpFilterError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.trusted_proxies = Some(crate::ip_filter::parse_all(nets)?);
        Ok(self)
    }

    /// Whether `peer` is a proxy whose forwarding headers we believe.
    fn trusts(&self, peer: IpAddr) -> bool {
        self.trusted_proxies
            .as_ref()
            .is_some_and(|nets| nets.iter().any(|n| n.contains(peer)))
    }
}

pub trait RealIpRouterExt {
    #[must_use]
    fn real_ip(self, layer: RealIpLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> RealIpRouterExt for Router<S> {
    fn real_ip(self, layer: RealIpLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move {
                    if let Some(ip) = extract(&req, &cfg.strategy) {
                        // The claim, for logging — unchanged, and never
                        // trusted on its own.
                        req.extensions_mut().insert(RealIp(ip));
                        // The claim plus the fact that it came over a hop
                        // the operator declared trusted (#1398).
                        let peer = req
                            .extensions()
                            .get::<ConnectInfo<std::net::SocketAddr>>()
                            .map(|ci| ci.ip());
                        if peer.is_some_and(|p| cfg.trusts(p)) {
                            req.extensions_mut().insert(TrustedRealIp(ip));
                        }
                    }
                    next.run(req).await
                }
            },
        ))
    }
}

fn extract(req: &Request<Body>, strategy: &HeaderStrategy) -> Option<IpAddr> {
    let h = req.headers();
    match strategy {
        HeaderStrategy::ForwardedRfc7239 => {
            parse_forwarded_rfc7239(h.get("forwarded").and_then(|v| v.to_str().ok())?)
        }
        HeaderStrategy::XForwardedFor => {
            parse_x_forwarded_for(h.get("x-forwarded-for").and_then(|v| v.to_str().ok())?)
        }
        HeaderStrategy::XRealIp => h
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse().ok()),
        HeaderStrategy::CfConnectingIp => h
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse().ok()),
        HeaderStrategy::Auto => extract(req, &HeaderStrategy::CfConnectingIp)
            .or_else(|| extract(req, &HeaderStrategy::ForwardedRfc7239))
            .or_else(|| extract(req, &HeaderStrategy::XForwardedFor))
            .or_else(|| extract(req, &HeaderStrategy::XRealIp))
            .or_else(|| {
                req.extensions()
                    .get::<ConnectInfo<std::net::SocketAddr>>()
                    .map(|ci| ci.ip())
            }),
    }
}

/// Parse the leftmost `for=<ip>` token from an RFC 7239 `Forwarded`
/// header. Strips IPv6 brackets and `:port` suffixes. Returns `None`
/// on any malformed value.
fn parse_forwarded_rfc7239(s: &str) -> Option<IpAddr> {
    // Comma-separated forwarded elements; pick the first one.
    let first = s.split(',').next()?.trim();
    // Each element is semicolon-separated key=value pairs.
    for kv in first.split(';') {
        let kv = kv.trim();
        let (k, v) = kv.split_once('=')?;
        if k.eq_ignore_ascii_case("for") {
            // Strip surrounding quotes if any.
            let v = v.trim().trim_matches('"');
            return parse_ip_with_optional_port(v);
        }
    }
    None
}

fn parse_x_forwarded_for(s: &str) -> Option<IpAddr> {
    // Leftmost = original client. Proxies APPEND, never prepend.
    let first = s.split(',').next()?.trim();
    parse_ip_with_optional_port(first)
}

/// Parse an IP that may be wrapped in brackets (`[::1]`) or carry a
/// `:port` suffix. The brackets-without-port case is also handled.
fn parse_ip_with_optional_port(s: &str) -> Option<IpAddr> {
    let s = s.trim();
    // [v6]:port
    if let Some(rest) = s.strip_prefix('[') {
        let close = rest.find(']')?;
        return rest[..close].parse().ok();
    }
    // bare v4:port — at most one colon; bare v6 has multiple.
    if s.matches(':').count() == 1 {
        if let Some((ip, _port)) = s.rsplit_once(':') {
            return ip.parse().ok();
        }
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn req_with_header(name: &'static str, value: &str) -> Request<Body> {
        Request::builder()
            .header(name, value)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn x_forwarded_for_picks_leftmost() {
        let r = req_with_header("x-forwarded-for", "1.2.3.4, 10.0.0.1, 172.16.0.5");
        let ip = extract(&r, &HeaderStrategy::XForwardedFor).unwrap();
        assert_eq!(ip.to_string(), "1.2.3.4");
    }

    #[test]
    fn x_forwarded_for_strips_ipv4_port() {
        let r = req_with_header("x-forwarded-for", "203.0.113.7:51234, 10.0.0.1");
        let ip = extract(&r, &HeaderStrategy::XForwardedFor).unwrap();
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn x_forwarded_for_handles_ipv6_brackets() {
        let r = req_with_header("x-forwarded-for", "[2001:db8::1]:443, 10.0.0.1");
        let ip = extract(&r, &HeaderStrategy::XForwardedFor).unwrap();
        assert_eq!(ip.to_string(), "2001:db8::1");
    }

    #[test]
    fn x_forwarded_for_bare_ipv6() {
        let r = req_with_header("x-forwarded-for", "2001:db8::1");
        let ip = extract(&r, &HeaderStrategy::XForwardedFor).unwrap();
        assert_eq!(ip.to_string(), "2001:db8::1");
    }

    #[test]
    fn x_real_ip_strategy() {
        let r = req_with_header("x-real-ip", "198.51.100.42");
        let ip = extract(&r, &HeaderStrategy::XRealIp).unwrap();
        assert_eq!(ip.to_string(), "198.51.100.42");
    }

    #[test]
    fn cf_connecting_ip_strategy() {
        let r = req_with_header("cf-connecting-ip", "2606:4700::1");
        let ip = extract(&r, &HeaderStrategy::CfConnectingIp).unwrap();
        assert_eq!(ip.to_string(), "2606:4700::1");
    }

    #[test]
    fn rfc7239_for_token_parses() {
        // RFC 7239 example: `for=192.0.2.43, for=198.51.100.17`
        let r = req_with_header("forwarded", "for=192.0.2.43, for=198.51.100.17");
        let ip = extract(&r, &HeaderStrategy::ForwardedRfc7239).unwrap();
        assert_eq!(ip.to_string(), "192.0.2.43");
    }

    #[test]
    fn rfc7239_with_quoted_ipv6_port() {
        let r = req_with_header("forwarded", r#"for="[2001:db8:cafe::17]:4711""#);
        let ip = extract(&r, &HeaderStrategy::ForwardedRfc7239).unwrap();
        assert_eq!(ip.to_string(), "2001:db8:cafe::17");
    }

    #[test]
    fn rfc7239_ignores_other_keys() {
        let r = req_with_header("forwarded", "by=10.0.0.1;for=203.0.113.7;proto=https");
        let ip = extract(&r, &HeaderStrategy::ForwardedRfc7239).unwrap();
        assert_eq!(ip.to_string(), "203.0.113.7");
    }

    #[test]
    fn auto_picks_cloudflare_first_when_present() {
        let r = Request::builder()
            .header("x-forwarded-for", "1.1.1.1")
            .header("cf-connecting-ip", "9.9.9.9")
            .body(Body::empty())
            .unwrap();
        let ip = extract(&r, &HeaderStrategy::Auto).unwrap();
        assert_eq!(ip.to_string(), "9.9.9.9");
    }

    #[test]
    fn auto_falls_through_to_xff_when_no_cf_or_forwarded() {
        let r = req_with_header("x-forwarded-for", "1.1.1.1");
        let ip = extract(&r, &HeaderStrategy::Auto).unwrap();
        assert_eq!(ip.to_string(), "1.1.1.1");
    }

    #[test]
    fn no_headers_returns_none_when_no_connect_info() {
        let r = Request::builder().body(Body::empty()).unwrap();
        assert!(extract(&r, &HeaderStrategy::Auto).is_none());
    }

    #[test]
    fn malformed_header_returns_none() {
        let r = req_with_header("x-real-ip", "not-an-ip");
        assert!(extract(&r, &HeaderStrategy::XRealIp).is_none());
    }

    #[tokio::test]
    async fn middleware_inserts_realip_into_extensions() {
        use axum::routing::get;
        use axum::Extension;
        use tower::ServiceExt;

        async fn handler(Extension(RealIp(ip)): Extension<RealIp>) -> String {
            ip.to_string()
        }

        let app = Router::new()
            .route("/", get(handler))
            .real_ip(RealIpLayer::default());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("x-forwarded-for", "192.0.2.1, 10.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "192.0.2.1");
    }
}
