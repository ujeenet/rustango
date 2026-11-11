//! The `Authorization: Bearer` credential and the 401 challenge for it.

use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
#[cfg(any(feature = "tenancy", feature = "admin", feature = "mcp"))]
use axum::http::{header::WWW_AUTHENTICATE, HeaderValue, StatusCode};
#[cfg(any(feature = "tenancy", feature = "admin", feature = "mcp"))]
use axum::response::Response;

/// The bearer token in `headers`. The scheme is case-insensitive
/// (RFC 9110 §11.1); an empty token is none.
///
/// ```
/// use axum::http::{header, HeaderMap};
/// let mut h = HeaderMap::new();
/// h.insert(header::AUTHORIZATION, "bearer abc".parse().unwrap());
/// assert_eq!(rustango::bearer::token(&h), Some("abc"));
/// ```
#[must_use]
pub fn token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.trim_start().split_once(' ')?;
    let token = rest.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[cfg(any(feature = "tenancy", feature = "admin", feature = "mcp"))]
pub(crate) use challenge::{challenge, Sent};

/// The 401 challenge, for the auth layers that send one.
#[cfg(any(feature = "tenancy", feature = "admin", feature = "mcp"))]
mod challenge {
    use super::{token, HeaderMap, HeaderValue, Response, StatusCode, WWW_AUTHENTICATE};

    /// What the client sent, which picks the challenge (RFC 6750 §3).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Sent {
        Nothing,
        BadToken,
    }

    impl Sent {
        /// From the request's headers.
        pub(crate) fn of(headers: &HeaderMap) -> Self {
            if token(headers).is_some() {
                Self::BadToken
            } else {
                Self::Nothing
            }
        }
    }

    /// `resp` with a `Bearer` challenge, when it is a 401 that has none.
    pub(crate) fn challenge(mut resp: Response, sent: Sent) -> Response {
        if resp.status() == StatusCode::UNAUTHORIZED
            && !resp.headers().contains_key(WWW_AUTHENTICATE)
        {
            let value = match sent {
                Sent::Nothing => "Bearer",
                Sent::BadToken => r#"Bearer error="invalid_token""#,
            };
            resp.headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static(value));
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        for v in ["Bearer abc", "bearer abc", "BEARER  abc ", " Bearer abc"] {
            assert_eq!(token(&with(v)), Some("abc"), "{v}");
        }
        for v in ["Bearer", "Bearer   ", "Basic abc", "Bearerabc", "Token abc"] {
            assert_eq!(token(&with(v)), None, "{v}");
        }
    }
}
