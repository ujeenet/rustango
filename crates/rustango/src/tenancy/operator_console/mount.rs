//! Where the console is mounted (#2007).
//!
//! Handlers and templates speak console-relative paths (`/orgs`). The
//! [`layer`] learns the prefix a `Router::nest` stripped and adds it at
//! the edge: to every `Location`, and to templates as `console_prefix`.

use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::{header, HeaderValue, Request, Response};
use axum::middleware::Next;

tokio::task_local! {
    static CURRENT: MountPrefix;
}

/// The path the console is nested under, e.g. `/ops`; empty at the root.
///
/// Built only from segments of unreserved URL characters, so it is safe
/// in an attribute, a script string and a `Location` header alike.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MountPrefix(String);

impl MountPrefix {
    /// What `nest` stripped: the original path minus the routed one.
    fn of(original: &str, routed: &str) -> Self {
        let prefix = if routed == "/" {
            original.trim_end_matches('/')
        } else {
            original.strip_suffix(routed).unwrap_or("")
        };
        Self::parse(prefix).unwrap_or_default()
    }

    fn parse(prefix: &str) -> Option<Self> {
        if prefix.is_empty() {
            return Some(Self::default());
        }
        let segment_ok = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~%".contains(&b))
        };
        let rest = prefix.strip_prefix('/')?;
        rest.split('/')
            .all(segment_ok)
            .then(|| Self(prefix.to_owned()))
    }

    /// This request's prefix; empty outside the [`layer`].
    pub(super) fn current() -> Self {
        CURRENT.try_with(Clone::clone).unwrap_or_default()
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// `path` under the prefix when it is a local path (`/…`). Absolute
    /// and scheme-relative URLs pass through.
    pub(super) fn url(&self, path: &str) -> String {
        if self.0.is_empty() || crate::auth_decorators::safe_next(path).is_none() {
            return path.to_owned();
        }
        // `nest` answers `/ops`, not `/ops/`.
        match path.strip_prefix('/') {
            Some(rest) if rest.is_empty() || rest.starts_with('?') => format!("{}{rest}", self.0),
            _ => format!("{}{path}", self.0),
        }
    }
}

/// Run the console under its prefix and put it on every redirect.
pub(super) async fn layer(req: Request<Body>, next: Next) -> Response<Body> {
    let prefix = req
        .extensions()
        .get::<OriginalUri>()
        .map(|o| MountPrefix::of(o.0.path(), req.uri().path()))
        .unwrap_or_default();
    let mut resp = CURRENT.scope(prefix.clone(), next.run(req)).await;
    if prefix.0.is_empty() {
        return resp;
    }
    let rewritten = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|loc| prefix.url(loc))
        .and_then(|loc| HeaderValue::from_str(&loc).ok());
    if let Some(v) = rewritten {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::MountPrefix;

    #[test]
    fn the_prefix_is_what_nest_stripped() {
        assert_eq!(MountPrefix::of("/ops/orgs", "/orgs").as_str(), "/ops");
        assert_eq!(MountPrefix::of("/ops", "/").as_str(), "/ops");
        assert_eq!(MountPrefix::of("/a/b/orgs", "/orgs").as_str(), "/a/b");
        assert_eq!(MountPrefix::of("/orgs", "/orgs").as_str(), "");
        assert_eq!(MountPrefix::of("/", "/").as_str(), "");
    }

    #[test]
    fn an_unsafe_prefix_is_dropped() {
        for (original, routed) in [
            ("//evil.com/orgs", "/orgs"),
            ("/a//orgs", "/orgs"),
            ("/../orgs", "/orgs"),
            ("/a\"b/orgs", "/orgs"),
            ("/a<b/orgs", "/orgs"),
        ] {
            assert_eq!(MountPrefix::of(original, routed).as_str(), "", "{original}");
        }
    }

    #[test]
    fn url_prefixes_console_paths_only() {
        let p = MountPrefix::parse("/ops").unwrap();
        assert_eq!(p.url("/orgs"), "/ops/orgs");
        assert_eq!(p.url("/"), "/ops");
        assert_eq!(p.url("/?notice=x"), "/ops?notice=x");
        assert_eq!(p.url("provision/3"), "provision/3");
        assert_eq!(p.url("https://t.example/x"), "https://t.example/x");
        assert_eq!(p.url("//cdn.example/x"), "//cdn.example/x");
        assert_eq!(MountPrefix::default().url("/orgs"), "/orgs");
    }
}
