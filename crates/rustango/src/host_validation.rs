//! Host-header allowlist middleware — Django parity for the
//! `ALLOWED_HOSTS` setting + the host-validation step
//! `SecurityMiddleware` runs implicitly before every view.
//!
//! Django gate: when `ALLOWED_HOSTS` is set, any request whose
//! `Host:` header isn't in the list is rejected with a 400. The
//! list supports exact-host entries (`example.com`), dot-prefix
//! subdomain wildcards (`.example.com` matches `api.example.com`
//! AND `example.com` itself), and the lone catch-all `*`.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::host_validation::{AllowedHostsLayer, AllowedHostsRouterExt};
//!
//! let app = Router::new()
//!     .route("/", get(home))
//!     .allowed_hosts(AllowedHostsLayer::new([
//!         "example.com",
//!         ".example.com",       // any subdomain
//!         "localhost",
//!     ]));
//! ```
//!
//! Requests with a missing or non-matching `Host` header receive a
//! `400 Bad Request` body that mirrors Django's `DisallowedHost`
//! message; the exact host echoes back to ease ops debugging without
//! leaking which hosts are allowed.
//!
//! ## Settings wiring
//!
//! `Settings.security.allowed_hosts: Vec<String>` (already parsed by
//! `env::list("ALLOWED_HOSTS")`) feeds the layer via
//! [`AllowedHostsLayer::from_settings_list`]. Empty list disables
//! validation — matches Django's "DEBUG=True allows all" behavior
//! by convention, but rustango doesn't have a DEBUG flag so the
//! operator must opt in explicitly.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

/// One allowed-host entry. Owns the comparison logic so the
/// matching loop stays cheap (no per-request allocation).
#[derive(Clone, Debug)]
enum Pattern {
    /// Catch-all `*` — every host matches. Use sparingly.
    Wildcard,
    /// Exact match: `Host` header (lowercased) equals this string.
    Exact(String),
    /// Dot-prefix wildcard `.example.com` — matches `example.com`
    /// itself plus any subdomain (`api.example.com`,
    /// `a.b.example.com`). The stored string omits the leading dot.
    Subdomain(String),
}

impl Pattern {
    fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        if entry == "*" {
            return Some(Self::Wildcard);
        }
        if let Some(rest) = entry.strip_prefix('.') {
            if rest.is_empty() {
                return None;
            }
            return Some(Self::Subdomain(rest.to_ascii_lowercase()));
        }
        Some(Self::Exact(entry.to_ascii_lowercase()))
    }

    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Wildcard => true,
            Self::Exact(h) => host == h,
            Self::Subdomain(tail) => {
                // `tail` does not include the leading dot. Match
                // `tail` itself (the base domain) or any host whose
                // suffix is `.<tail>` (avoids matching
                // `eviltail.com` against `.tail.com`).
                host == tail
                    || host
                        .strip_suffix(tail)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
        }
    }
}

/// Tower-layer-equivalent configuration. Holds the parsed pattern
/// list; applied via [`AllowedHostsRouterExt::allowed_hosts`].
#[derive(Clone)]
pub struct AllowedHostsLayer {
    patterns: Arc<Vec<Pattern>>,
}

impl AllowedHostsLayer {
    /// Build a layer from a list of allowed-host entries. Entries
    /// support exact-match hostnames, `.example.com` subdomain
    /// wildcards, and the lone catch-all `*`. Empty / whitespace
    /// entries are silently dropped.
    #[must_use]
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let patterns: Vec<Pattern> = entries
            .into_iter()
            .filter_map(|s| Pattern::parse(s.as_ref()))
            .collect();
        Self {
            patterns: Arc::new(patterns),
        }
    }

    /// Convenience: wire from `Settings.security.allowed_hosts`. An
    /// empty list disables the layer (every host passes) — matches
    /// the "no ALLOWED_HOSTS configured → no enforcement" shape
    /// Django uses with `DEBUG=True`.
    #[must_use]
    pub fn from_settings_list<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new(entries)
    }

    /// `true` when the configured list permits this host header.
    /// Empty list passes every host (operator opted out of
    /// validation by leaving the setting empty).
    #[must_use]
    pub fn permits(&self, host: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let host = strip_port(host).to_ascii_lowercase();
        self.patterns.iter().any(|p| p.matches(&host))
    }
}

/// Strip a trailing `:<port>` from a Host-header value so the
/// allowlist comparison ignores it. Returns the input untouched if
/// no port is present. Handles bracketed IPv6 literals too.
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: `[::1]:8080` → strip from the closing bracket.
        if let Some(end) = rest.find(']') {
            return &host[..end + 2.min(host.len())];
        }
        return host;
    }
    match host.rfind(':') {
        Some(i) => &host[..i],
        None => host,
    }
}

/// Router extension trait — `.allowed_hosts(layer)`.
pub trait AllowedHostsRouterExt {
    #[must_use]
    fn allowed_hosts(self, layer: AllowedHostsLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> AllowedHostsRouterExt for Router<S> {
    fn allowed_hosts(self, layer: AllowedHostsLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<AllowedHostsLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if cfg.permits(host) {
        next.run(req).await
    } else {
        let msg = format!(
            "DisallowedHost: rejected Host header {host:?} — \
             add it to Settings.security.allowed_hosts to allow"
        );
        let mut resp = Response::new(Body::from(msg));
        *resp.status_mut() = StatusCode::BAD_REQUEST;
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_passes_every_host() {
        let layer = AllowedHostsLayer::new(Vec::<String>::new());
        assert!(layer.permits("anywhere.example.com"));
        assert!(layer.permits(""));
    }

    #[test]
    fn exact_match_is_case_insensitive() {
        let layer = AllowedHostsLayer::new(["Example.COM"]);
        assert!(layer.permits("example.com"));
        assert!(layer.permits("EXAMPLE.com"));
        assert!(!layer.permits("api.example.com"));
    }

    #[test]
    fn dot_prefix_wildcard_matches_subdomains_plus_base() {
        let layer = AllowedHostsLayer::new([".example.com"]);
        assert!(layer.permits("example.com"));
        assert!(layer.permits("api.example.com"));
        assert!(layer.permits("a.b.example.com"));
        // Tricky case Django gets right: an unrelated host that
        // *ends with* "example.com" but isn't a subdomain shouldn't
        // match. `evilexample.com` ends with `example.com` but the
        // boundary char isn't a dot.
        assert!(!layer.permits("evilexample.com"));
    }

    #[test]
    fn star_is_catchall() {
        let layer = AllowedHostsLayer::new(["*"]);
        assert!(layer.permits("anything"));
        assert!(layer.permits("attacker.com"));
    }

    #[test]
    fn port_is_stripped_before_comparison() {
        let layer = AllowedHostsLayer::new(["example.com"]);
        assert!(layer.permits("example.com:8080"));
        assert!(layer.permits("example.com:443"));
    }

    #[test]
    fn ipv6_with_port_is_handled() {
        let layer = AllowedHostsLayer::new(["[::1]"]);
        assert!(layer.permits("[::1]:8080"));
    }

    #[test]
    fn whitespace_entries_are_ignored() {
        let layer = AllowedHostsLayer::new(["", "   ", "example.com"]);
        // Only the real entry counts; non-matching hosts still get
        // rejected.
        assert!(layer.permits("example.com"));
        assert!(!layer.permits("attacker.com"));
    }

    #[test]
    fn rejected_host_does_not_match_other_patterns_in_list() {
        let layer = AllowedHostsLayer::new(["a.com", ".b.com", "c.com"]);
        assert!(layer.permits("a.com"));
        assert!(layer.permits("foo.b.com"));
        assert!(layer.permits("c.com"));
        assert!(!layer.permits("d.com"));
        assert!(!layer.permits("malicious.a.com"));
    }
}
