//! Host-header allowlist middleware.
//!
//! The client sends the `Host:` header, so it is only a claim. Code
//! that builds absolute URLs, reset links or cache keys from it trusts
//! whoever called. This layer is the check: a request whose `Host` is
//! not on the list is rejected with a 400. It is an allowlist, not a
//! blocklist, so a host you never named is refused without you having
//! to predict it.
//!
//! Entries may be exact hosts (`example.com`), dot-prefix subdomain
//! wildcards (`.example.com` matches `api.example.com` and
//! `example.com` itself), or the catch-all `*`.
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
//! A missing or unlisted `Host` gets a `400 Bad Request`. The body
//! echoes the rejected host to help ops, but never the allowed list.
//!
//! ## Settings wiring
//!
//! `Settings.security.allowed_hosts: Vec<String>` (parsed by
//! `env::list("ALLOWED_HOSTS")`) feeds
//! [`AllowedHostsLayer::from_settings_list`]. An empty list turns the
//! check off, so the operator has to opt in.
//!
//! [`AllowedHostsLayer::from_settings_list`]: crate::host_validation::AllowedHostsLayer::from_settings_list

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

/// One allowed-host entry, pre-parsed so matching allocates nothing
/// per request.
#[derive(Clone, Debug)]
enum Pattern {
    /// Catch-all `*`: every host matches, which switches the check
    /// off. Dangerous in production — name your hosts instead.
    Wildcard,
    /// Exact match against the lowercased `Host` header.
    Exact(String),
    /// Dot-prefix wildcard `.example.com`: matches `example.com` and
    /// any subdomain. Stored without the leading dot.
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
                // Match the base domain itself, or a host ending in
                // `.<tail>`. The dot matters: without it
                // `eviltail.com` would match `.tail.com`.
                host == tail
                    || host
                        .strip_suffix(tail)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
        }
    }
}

/// The parsed pattern list. Apply it with
/// [`AllowedHostsRouterExt::allowed_hosts`].
#[derive(Clone)]
pub struct AllowedHostsLayer {
    patterns: Arc<Vec<Pattern>>,
}

impl AllowedHostsLayer {
    /// Build a layer from allowed-host entries: exact hostnames,
    /// `.example.com` subdomain wildcards, or the catch-all `*`.
    /// Blank entries are dropped.
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

    /// Wire from `Settings.security.allowed_hosts`. An empty list
    /// disables the layer and every host passes.
    #[must_use]
    pub fn from_settings_list<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new(entries)
    }

    /// `true` when the list permits this host header. An empty list
    /// passes every host: the operator opted out.
    #[must_use]
    pub fn permits(&self, host: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let host = strip_port(host).to_ascii_lowercase();
        self.patterns.iter().any(|p| p.matches(&host))
    }
}

/// Drop a trailing `:<port>` so the allowlist compares host names
/// only. Returns the input unchanged when there is no port. Handles
/// bracketed IPv6 literals.
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: `[::1]:8080` → cut at the closing bracket.
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

/// Router extension: `.allowed_hosts(layer)`.
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
        // `evilexample.com` ends with "example.com" but is not a
        // subdomain: the boundary character is not a dot.
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
        // Only the real entry counts; other hosts are still rejected.
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
