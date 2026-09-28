//! Real-IP extraction middleware for apps behind a trusted reverse proxy.
//!
//! `axum::extract::ConnectInfo<SocketAddr>` reports the immediate
//! peer, which behind nginx, Cloudflare or an ELB is the proxy. This
//! middleware reads a forwarded-for header instead and puts the
//! result in the request extensions as a [`RealIp`].
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::real_ip::{RealIpLayer, RealIpRouterExt, RealIp};
//! use axum::Extension;
//!
//! // No proxies trusted: RealIp is the header's claim, for logs only.
//! let app = axum::Router::new()
//!     .route("/", axum::routing::get(home))
//!     .real_ip(RealIpLayer::default());
//!
//! async fn home(Extension(ip): Extension<RealIp>) -> String {
//!     format!("hi, {}", ip.0)
//! }
//! ```
//!
//! ## Security note
//!
//! **A forwarded-for header is only a claim.** Any client can send
//! one. Use this layer only when a proxy you control receives every
//! request and rewrites the header, and set that proxy to strip any
//! header the client sent.
//!
//! [`RealIp`]: crate::real_ip::RealIp

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
    /// Standard `Forwarded: for=<ip>` (RFC 7239). Leftmost `for=` for
    /// the claim; walked from the right behind a trusted proxy.
    ForwardedRfc7239,
    /// `X-Forwarded-For: client, proxy1, proxy2`. Leftmost hop for the
    /// claim; walked from the right behind a trusted proxy.
    XForwardedFor,
    /// `X-Real-IP: <ip>`. Single value; some proxies (nginx) set this
    /// rather than X-Forwarded-For.
    XRealIp,
    /// Cloudflare's `CF-Connecting-IP`.
    CfConnectingIp,
    /// Try each strategy in order; first hit wins. Default. Behind a
    /// trusted proxy only `X-Forwarded-For` is read.
    Auto,
}

/// A client IP from a header sent by a **trusted** proxy: the
/// connecting socket matched [`RealIpLayer::trust_proxies`].
///
/// A [`RealIp`] only means "a header claimed this", and any client can
/// claim anything, so never base a security decision on it. This type
/// adds the fact that the claim came over a hop the operator trusts,
/// which is what makes it safe for [`crate::rate_limit`] to key on.
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

    /// Name the networks whose forwarding headers you believe, so the
    /// resolved address can be used for security decisions.
    ///
    /// Without this, a forwarding header is only a claim and [`RealIp`]
    /// is safe for logging alone. List the addresses your ingress
    /// connects from: the load balancer, nginx, your CDN egress ranges.
    /// A request from one of those also gets a [`TrustedRealIp`], which
    /// [`crate::rate_limit::RateLimitLayer::per_ip`] keys on.
    ///
    /// The client is the rightmost hop not in these networks, since
    /// proxies append; hops a client wrote further left are ignored.
    /// `X-Real-IP` and `CF-Connecting-IP` are taken as sent, so pick
    /// those only when your proxy overwrites them. Other peers get
    /// `RealIp` but never `TrustedRealIp`.
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

    /// The client behind a trusted peer. A hop chain is read right to
    /// left and the first untrusted hop wins, since proxies append.
    fn resolve_trusted(&self, req: &Request<Body>) -> Option<IpAddr> {
        let chain: Vec<Option<IpAddr>> = match self.strategy {
            // Proxies append to XFF; other headers may be client-sent.
            HeaderStrategy::XForwardedFor | HeaderStrategy::Auto => {
                header_hops(req, "x-forwarded-for")?
                    .map(|h| h.and_then(parse_ip_with_optional_port))
                    .collect()
            }
            HeaderStrategy::ForwardedRfc7239 => header_hops(req, "forwarded")?
                .map(|h| h.and_then(rfc7239_for))
                .collect(),
            HeaderStrategy::XRealIp | HeaderStrategy::CfConnectingIp => {
                return extract(req, &self.strategy);
            }
        };
        let mut rightmost = None;
        for hop in chain.iter().rev() {
            let ip = (*hop)?;
            if !self.trusts(ip) {
                return Some(ip);
            }
            rightmost = rightmost.or(Some(ip));
        }
        rightmost
    }
}

/// Every comma-separated hop of every `name` line, in order. Each hop
/// is decoded alone, so one bad hop does not void the others.
fn header_hops<'a>(
    req: &'a Request<Body>,
    name: &str,
) -> Option<impl Iterator<Item = Option<&'a str>>> {
    let mut lines = req.headers().get_all(name).iter().peekable();
    lines.peek()?;
    Some(lines.flat_map(|v| {
        v.as_bytes()
            .split(|b| *b == b',')
            .map(|hop| std::str::from_utf8(hop).ok())
    }))
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
                    let peer = req
                        .extensions()
                        .get::<ConnectInfo<std::net::SocketAddr>>()
                        .map(|ci| ci.ip());
                    // Trusted form, only if the peer is a named proxy.
                    let trusted = peer
                        .filter(|p| cfg.trusts(*p))
                        .and_then(|_| cfg.resolve_trusted(&req));
                    if let Some(ip) = trusted {
                        req.extensions_mut().insert(RealIp(ip));
                        req.extensions_mut().insert(TrustedRealIp(ip));
                    } else if let Some(ip) = extract(&req, &cfg.strategy) {
                        // The bare claim: fine for logs, never trusted.
                        req.extensions_mut().insert(RealIp(ip));
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
            .and_then(parse_ip_with_optional_port),
        HeaderStrategy::CfConnectingIp => h
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_ip_with_optional_port),
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
    rfc7239_chain(s).first().copied().flatten()
}

/// The `for=` address of each `Forwarded` element, `None` where absent
/// or unparseable.
fn rfc7239_chain(s: &str) -> Vec<Option<IpAddr>> {
    s.split(',').map(rfc7239_for).collect()
}

fn rfc7239_for(element: &str) -> Option<IpAddr> {
    // Each element is semicolon-separated key=value pairs.
    for kv in element.trim().split(';') {
        let (k, v) = kv.trim().split_once('=')?;
        if k.eq_ignore_ascii_case("for") {
            // Strip surrounding quotes if any.
            return parse_ip_with_optional_port(v.trim().trim_matches('"'));
        }
    }
    None
}

fn parse_x_forwarded_for(s: &str) -> Option<IpAddr> {
    forwarded_for_chain(s).first().copied().flatten()
}

/// Each `X-Forwarded-For` hop, left to right; `None` where unparseable.
fn forwarded_for_chain(s: &str) -> Vec<Option<IpAddr>> {
    s.split(',').map(parse_ip_with_optional_port).collect()
}

/// Parse an IP that may be wrapped in brackets (`[::1]`) or carry a
/// `:port` suffix. The brackets-without-port case is also handled.
fn parse_ip_with_optional_port(s: &str) -> Option<IpAddr> {
    parse_ip_raw(s).map(|ip| ip.to_canonical())
}

fn parse_ip_raw(s: &str) -> Option<IpAddr> {
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

    fn trusting(strategy: HeaderStrategy) -> RealIpLayer {
        RealIpLayer::new(strategy)
            .trust_proxies(["10.0.0.0/8"])
            .unwrap()
    }

    #[test]
    fn rfc7239_trusted_walks_from_the_right() {
        let r = req_with_header("forwarded", "for=1.1.1.1, for=203.0.113.7, for=10.0.0.2");
        let ip = trusting(HeaderStrategy::ForwardedRfc7239).resolve_trusted(&r);
        assert_eq!(ip.unwrap().to_string(), "203.0.113.7");
    }

    #[test]
    fn trusted_chain_stops_at_an_unparseable_hop() {
        let r = req_with_header("x-forwarded-for", "203.0.113.7, unknown, 10.0.0.2");
        assert!(trusting(HeaderStrategy::XForwardedFor)
            .resolve_trusted(&r)
            .is_none());
    }

    #[test]
    fn all_trusted_chain_yields_rightmost_hop() {
        let r = req_with_header("x-forwarded-for", "10.0.0.3, 10.0.0.2");
        let ip = trusting(HeaderStrategy::XForwardedFor).resolve_trusted(&r);
        assert_eq!(ip.unwrap().to_string(), "10.0.0.2");
    }

    #[test]
    fn non_ascii_hop_does_not_drop_the_chain() {
        let v = axum::http::HeaderValue::from_bytes(b"caf\xc3\xa9, 203.0.113.7, 10.0.0.2").unwrap();
        let r = Request::builder()
            .header("x-forwarded-for", v)
            .body(Body::empty())
            .unwrap();
        let ip = trusting(HeaderStrategy::XForwardedFor).resolve_trusted(&r);
        assert_eq!(ip.unwrap().to_string(), "203.0.113.7");
    }

    #[test]
    fn non_ascii_hop_stops_the_walk() {
        let v = axum::http::HeaderValue::from_bytes(b"203.0.113.7, \xff, 10.0.0.2").unwrap();
        let r = Request::builder()
            .header("x-forwarded-for", v)
            .body(Body::empty())
            .unwrap();
        assert!(trusting(HeaderStrategy::XForwardedFor)
            .resolve_trusted(&r)
            .is_none());
    }

    #[test]
    fn trusted_x_real_ip_strips_port_and_canonicalises() {
        let t = trusting(HeaderStrategy::XRealIp);
        let r = req_with_header("x-real-ip", "203.0.113.7:8080");
        assert_eq!(t.resolve_trusted(&r).unwrap().to_string(), "203.0.113.7");
        let r = req_with_header("x-real-ip", "::ffff:198.51.100.1");
        assert_eq!(t.resolve_trusted(&r).unwrap().to_string(), "198.51.100.1");
        let r = req_with_header("x-forwarded-for", "198.51.100.1");
        assert!(t.resolve_trusted(&r).is_none());
    }

    #[test]
    fn trusted_cf_connecting_ip_strips_port_and_canonicalises() {
        let t = trusting(HeaderStrategy::CfConnectingIp);
        let r = req_with_header("cf-connecting-ip", "[2001:db8::1]:443");
        assert_eq!(t.resolve_trusted(&r).unwrap().to_string(), "2001:db8::1");
        let r = req_with_header("cf-connecting-ip", "::ffff:198.51.100.1");
        assert_eq!(t.resolve_trusted(&r).unwrap().to_string(), "198.51.100.1");
        let r = req_with_header("x-real-ip", "198.51.100.1");
        assert!(t.resolve_trusted(&r).is_none());
    }

    #[test]
    fn auto_trusted_ignores_client_sent_cf_header() {
        let r = Request::builder()
            .header("cf-connecting-ip", "9.9.9.9")
            .header("x-forwarded-for", "203.0.113.7")
            .body(Body::empty())
            .unwrap();
        let ip = trusting(HeaderStrategy::Auto).resolve_trusted(&r);
        assert_eq!(ip.unwrap().to_string(), "203.0.113.7");
    }

    #[test]
    fn ipv4_mapped_hop_is_canonicalised() {
        let r = req_with_header("x-forwarded-for", "::ffff:203.0.113.7");
        let ip = extract(&r, &HeaderStrategy::XForwardedFor).unwrap();
        assert_eq!(ip.to_string(), "203.0.113.7");
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
