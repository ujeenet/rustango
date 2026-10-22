//! Signed `?error=` / `?notice=` / `?ok=` messages.
//!
//! Handlers redirect with [`redirect`]; the layer signs that message, bound
//! to the target path and a short expiry, and drops any other message on
//! the way in, so a crafted link cannot put text on a console page.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, Request, Response, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Redirect};

use super::ConsoleState;
use crate::session::SessionSecret;

/// Query keys whose value a console page shows as text.
const KEYS: [&str; 3] = ["error", "notice", "ok"];
/// Query key carrying the signature.
const SIG: &str = "flash_sig";
/// Query key carrying the expiry (unix seconds).
const EXP: &str = "flash_exp";
/// How long a signed message stays readable.
const TTL_SECS: i64 = 300;

type Pairs = Vec<(String, String)>;

/// A message for the page a handler redirects to.
#[derive(Clone, Copy, Debug)]
pub(super) enum Flash<'a> {
    Error(&'a str),
    Notice(&'a str),
    Ok(&'a str),
}

impl Flash<'_> {
    fn parts(self) -> (&'static str, String) {
        match self {
            Self::Error(m) => ("error", m.to_owned()),
            Self::Notice(m) => ("notice", m.to_owned()),
            Self::Ok(m) => ("ok", m.to_owned()),
        }
    }
}

/// Marks a response whose message the layer should sign.
#[derive(Clone)]
struct Pending {
    key: &'static str,
    text: String,
}

/// The messages that passed the check, for pages that render them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Messages {
    pub(super) error: Option<String>,
    pub(super) notice: Option<String>,
    pub(super) ok: Option<String>,
}

/// Redirect to `location` (which may carry its own query) with `flash`.
pub(super) fn redirect(location: &str, flash: Flash<'_>) -> Response<Body> {
    let (key, text) = flash.parts();
    let sep = if location.contains('?') { '&' } else { '?' };
    let target = format!(
        "{location}{sep}{key}={}",
        crate::url_codec::url_encode(&text)
    );
    let mut resp = Redirect::to(&target).into_response();
    resp.extensions_mut().insert(Pending { key, text });
    resp
}

/// `next` without message keys, so a login bounce cannot carry one.
pub(super) fn strip_from_next(next: String) -> String {
    let Some((path, query)) = next.split_once('?') else {
        return next;
    };
    let Ok(mut pairs) = serde_urlencoded::from_str::<Pairs>(query) else {
        return path.to_owned();
    };
    pairs.retain(|(k, _)| !is_flash_key(k));
    match serde_urlencoded::to_string(&pairs) {
        Ok(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_owned(),
    }
}

fn is_flash_key(k: &str) -> bool {
    KEYS.contains(&k) || k == SIG || k == EXP
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn signature(secret: &SessionSecret, path: &str, msgs: &[(&str, &str)], exp: i64) -> String {
    // Length-prefixed, so no value can pose as another field.
    let mut msg = format!("rustango-op-flash\0{}:{path}\0{exp}", path.len()).into_bytes();
    for (k, v) in msgs {
        msg.extend_from_slice(format!("\0{k}={}:{v}", v.len()).as_bytes());
    }
    crate::url_codec::urlsafe_base64_encode(&crate::session::sign(secret, &msg))
}

/// `location` with the signature for `pending` appended.
fn sign_location(secret: &SessionSecret, location: &str, pending: &Pending, now: i64) -> String {
    let path = location.split('?').next().unwrap_or(location);
    let exp = now + TTL_SECS;
    let sig = signature(secret, path, &[(pending.key, &pending.text)], exp);
    format!("{location}&{EXP}={exp}&{SIG}={sig}")
}

/// What the check made of a request query.
#[derive(Debug, PartialEq)]
enum Verdict {
    /// No message keys at all.
    Untouched,
    /// Signed for this path and still fresh.
    Verified(Messages),
    /// The query to use instead, with every message key removed.
    Stripped(String),
}

fn verify(secret: &SessionSecret, path: &str, query: &str, now: i64) -> Verdict {
    let Ok(mut pairs) = serde_urlencoded::from_str::<Pairs>(query) else {
        return Verdict::Untouched;
    };
    if !pairs.iter().any(|(k, _)| is_flash_key(k)) {
        return Verdict::Untouched;
    }
    if let Some(messages) = signed_messages(secret, path, &pairs, now) {
        return Verdict::Verified(messages);
    }
    pairs.retain(|(k, _)| !is_flash_key(k));
    Verdict::Stripped(serde_urlencoded::to_string(&pairs).unwrap_or_default())
}

/// The messages, when each flash key appears at most once and the signature checks.
fn signed_messages(
    secret: &SessionSecret,
    path: &str,
    pairs: &Pairs,
    now: i64,
) -> Option<Messages> {
    let one = |key: &str| -> Result<Option<&str>, ()> {
        let mut it = pairs.iter().filter(|(k, _)| k == key);
        let first = it.next().map(|(_, v)| v.as_str());
        if it.next().is_some() {
            return Err(());
        }
        Ok(first)
    };
    let given = one(SIG).ok()??;
    let exp: i64 = one(EXP).ok()??.parse().ok()?;
    if now > exp {
        return None;
    }
    let mut msgs = Vec::new();
    for key in KEYS {
        if let Some(v) = one(key).ok()? {
            msgs.push((key, v));
        }
    }
    let expected = signature(secret, path, &msgs, exp);
    if !crate::crypto::constant_time_compare(given.as_bytes(), expected.as_bytes()) {
        return None;
    }
    let get = |key: &str| {
        msgs.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
    };
    Some(Messages {
        error: get("error"),
        notice: get("notice"),
        ok: get("ok"),
    })
}

pub(super) async fn layer(
    State(state): State<ConsoleState>,
    mut req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let secret = &state.session_secret;
    let path = req.uri().path().to_owned();
    let verdict = req
        .uri()
        .query()
        .map_or(Verdict::Untouched, |q| verify(secret, &path, q, now()));
    match verdict {
        Verdict::Untouched => {}
        Verdict::Verified(messages) => {
            req.extensions_mut().insert(messages);
        }
        Verdict::Stripped(query) => {
            let pq = if query.is_empty() {
                path
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
    }
    let mut resp = next.run(req).await;
    let Some(pending) = resp.extensions_mut().remove::<Pending>() else {
        return resp;
    };
    let signed = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|loc| sign_location(secret, loc, &pending, now()))
        .and_then(|loc| HeaderValue::from_str(&loc).ok());
    if let Some(v) = signed {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn secret() -> SessionSecret {
        SessionSecret::from_bytes(vec![7; 32])
    }

    fn signed(location: &str, flash: Flash<'_>) -> String {
        let resp = redirect(location, flash);
        let pending = resp.extensions().get::<Pending>().unwrap().clone();
        let loc = resp.headers()[header::LOCATION].to_str().unwrap();
        sign_location(&secret(), loc, &pending, NOW)
    }

    fn query(loc: &str) -> &str {
        loc.split_once('?').unwrap().1
    }

    #[test]
    fn a_signed_message_survives_and_a_forged_one_is_dropped() {
        let s = secret();
        let loc = signed("/orgs?page=2", Flash::Notice("tenant created"));
        let q = query(&loc);
        let Verdict::Verified(m) = verify(&s, "/orgs", q, NOW) else {
            panic!("signed message must pass: {loc}");
        };
        assert_eq!(m.notice.as_deref(), Some("tenant created"));

        let forged = q.replace("tenant", "call");
        assert!(matches!(verify(&s, "/orgs", &forged, NOW), Verdict::Stripped(q) if q == "page=2"));
        assert_eq!(
            verify(&s, "/orgs", "error=phish&next=%2F", NOW),
            Verdict::Stripped("next=%2F".into())
        );
        assert_eq!(verify(&s, "/orgs", "page=3", NOW), Verdict::Untouched);
    }

    /// A signature holds only on its own path and only until it expires.
    #[test]
    fn a_signature_is_bound_to_its_path_and_expiry() {
        let s = secret();
        let loc = signed("/change-password", Flash::Error("bad"));
        let q = query(&loc);
        assert!(matches!(
            verify(&s, "/change-password", q, NOW + TTL_SECS),
            Verdict::Verified(_)
        ));
        assert!(matches!(verify(&s, "/orgs", q, NOW), Verdict::Stripped(_)));
        assert!(matches!(
            verify(&s, "/change-password", q, NOW + TTL_SECS + 1),
            Verdict::Stripped(_)
        ));
    }

    /// A repeated or added message key voids the signature.
    #[test]
    fn a_repeated_key_drops_every_message() {
        let s = secret();
        let loc = signed("/orgs", Flash::Error("bad"));
        let q = query(&loc);
        for extra in ["&error=phish", "&ok=phish", "&flash_exp=1", "&flash_sig=x"] {
            let v = verify(&s, "/orgs", &format!("{q}{extra}"), NOW);
            assert_eq!(v, Verdict::Stripped(String::new()), "{extra}");
        }
    }

    #[test]
    fn next_loses_its_message_keys() {
        assert_eq!(
            strip_from_next("/orgs?error=x&page=2&flash_sig=y&flash_exp=1&ok=z".into()),
            "/orgs?page=2"
        );
        assert_eq!(strip_from_next("/orgs?notice=x".into()), "/orgs");
        assert_eq!(strip_from_next("/orgs/a/edit".into()), "/orgs/a/edit");
    }
}
