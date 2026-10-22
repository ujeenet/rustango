//! Signed `?error=` / `?notice=` / `?ok=` messages.
//!
//! Handlers redirect with a message in the query and pages render it. This
//! layer signs the message on every outgoing redirect and drops an unsigned
//! or altered one on the way in, so a crafted link cannot put text on a
//! console page.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, Request, Response, Uri};
use axum::middleware::Next;

use super::ConsoleState;
use crate::session::SessionSecret;

/// Query keys whose value a console page shows as text.
const KEYS: [&str; 3] = ["error", "notice", "ok"];
/// Query key carrying the signature.
const SIG: &str = "flash_sig";

type Pairs = Vec<(String, String)>;

fn signature(secret: &SessionSecret, pairs: &Pairs) -> Option<String> {
    let mut msg = b"rustango-op-flash".to_vec();
    let mut any = false;
    for key in KEYS {
        if let Some((_, v)) = pairs.iter().find(|(k, _)| k == key) {
            any = true;
            msg.extend_from_slice(format!("\0{key}\0{v}").as_bytes());
        }
    }
    any.then(|| crate::url_codec::urlsafe_base64_encode(&crate::session::sign(secret, &msg)))
}

/// Sign the message in a redirect target; other targets pass unchanged.
fn sign_location(secret: &SessionSecret, location: &str) -> Option<String> {
    let (path, query) = location.split_once('?')?;
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let mut pairs: Pairs = serde_urlencoded::from_str(query).ok()?;
    pairs.retain(|(k, _)| k != SIG);
    let sig = signature(secret, &pairs)?;
    pairs.push((SIG.to_owned(), sig));
    let mut out = format!("{path}?{}", serde_urlencoded::to_string(&pairs).ok()?);
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Some(out)
}

/// The request query with message keys removed unless their signature checks.
fn verified_query(secret: &SessionSecret, query: &str) -> Option<String> {
    let mut pairs: Pairs = serde_urlencoded::from_str(query).ok()?;
    let given = pairs.iter().find(|(k, _)| k == SIG).map(|(_, v)| v.clone());
    let expected = signature(secret, &pairs)?;
    let ok = given
        .is_some_and(|g| crate::crypto::constant_time_compare(g.as_bytes(), expected.as_bytes()));
    if ok {
        return None;
    }
    pairs.retain(|(k, _)| !KEYS.contains(&k.as_str()) && k != SIG);
    serde_urlencoded::to_string(&pairs).ok()
}

pub(super) async fn layer(
    State(state): State<ConsoleState>,
    mut req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let secret = &state.session_secret;
    let stripped = req.uri().query().and_then(|q| verified_query(secret, q));
    if let Some(query) = stripped {
        let path = req.uri().path();
        let pq = if query.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{query}")
        };
        let mut parts = req.uri().clone().into_parts();
        if let Ok(pq) = pq.parse() {
            parts.path_and_query = Some(pq);
            if let Ok(uri) = Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
    let mut resp = next.run(req).await;
    let signed = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|loc| sign_location(secret, loc))
        .and_then(|loc| HeaderValue::from_str(&loc).ok());
    if let Some(v) = signed {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> SessionSecret {
        SessionSecret::from_bytes(vec![7; 32])
    }

    #[test]
    fn a_signed_message_survives_and_a_forged_one_is_dropped() {
        let s = secret();
        let loc = sign_location(&s, "/orgs?notice=tenant+created&page=2").unwrap();
        let query = loc.split_once('?').unwrap().1;
        assert_eq!(verified_query(&s, query), None, "signed message must pass");

        let forged = query.replace("tenant+created", "call+555");
        assert_eq!(verified_query(&s, &forged).as_deref(), Some("page=2"));
        assert_eq!(verified_query(&s, "error=phish&next=%2F").as_deref(), Some("next=%2F"));
        assert_eq!(verified_query(&s, "page=3"), None, "no message, untouched");
        assert!(sign_location(&s, "/orgs?page=2").is_none());
    }
}
