//! Django's [messages framework](https://docs.djangoproject.com/en/6.0/ref/contrib/messages/),
//! ported to a cookie-backed signed-storage shape. Issue #9.
//!
//! Powers the standard POST→303→render flash idiom: a handler stages
//! a one-shot message ("Saved successfully"), redirects, the next
//! render reads + clears the message from the cookie.
//!
//! ```ignore
//! use rustango::messages;
//!
//! const SECRET: &[u8] = b"app-wide secret — derive from Settings.secret_key";
//!
//! async fn save_handler(headers: HeaderMap) -> Response {
//!     // … do the save …
//!     let cookie = messages::success(SECRET, &headers, "Saved successfully.");
//!     let mut res = Redirect::to("/items").into_response();
//!     if let Ok(v) = HeaderValue::from_str(&cookie) {
//!         res.headers_mut().append(header::SET_COOKIE, v);
//!     }
//!     res
//! }
//!
//! async fn list_handler(headers: HeaderMap) -> Response {
//!     let (msgs, clear_cookie) = messages::drain(SECRET, &headers);
//!     // … render template, pass `msgs` ...
//!     // Apply `clear_cookie` to the response so the messages don't repeat.
//! }
//! ```
//!
//! ## Storage
//!
//! Default storage is a signed cookie (`HMAC-SHA256` over the encoded
//! payload). The cookie body is `base64url(payload).base64url(sig)`
//! and is dropped when read so messages are one-shot (matching
//! Django's behavior). Session-backed storage that survives cookie
//! eviction is queued as a follow-up.
//!
//! ## Tampering / replay
//!
//! Bad / missing signatures → `drain` returns an empty Vec. Forged
//! cookies don't crash the handler; they're just ignored. Replayed
//! cookies (browser kept the value past the clear-cookie response)
//! re-show the messages once — the next drain clears them.
//!
//! ## Out of scope (queued as follow-ups)
//!
//! - **`Secure` cookie attribute** — currently the cookie ships with
//!   `HttpOnly` + `SameSite=Lax` but no `Secure`. Mirror the
//!   [`crate::forms::csrf::CsrfConfig`] shape — `secure: bool`
//!   default true + `allow_insecure_for_dev()` opt-out — once the
//!   config-struct surface lands. Lower stakes than CSRF (one-shot
//!   UI hints) but worth following the same convention.
//! - **Session-backed storage** + **FallbackStorage** (cookie + session
//!   fallback) — requires plumbing through `sessions::SessionStore`.
//! - **Middleware-shape auto-apply** — current API requires callers to
//!   thread the cookie into the response manually. A response-side
//!   layer that auto-applies is feasible but adds tower-service
//!   complexity.
//! - **`SuccessMessageMixin` on CBVs** — small wiring on
//!   `CreateView` / `UpdateView` for the common "post-save flash"
//!   pattern.
//! - **`MESSAGE_TAGS` global setting** — Django maps `{ERROR: 'danger'}`
//!   for Bootstrap CSS classes; the rustango shape exposes the `tags`
//!   field per message so the template can do whatever mapping it
//!   wants directly.

use std::str::FromStr;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Message severity, mirroring Django's five-level scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Success,
    Warning,
    Error,
}

impl Level {
    /// Stable wire-format tag. Used in the cookie payload + the
    /// `level` field stamped into the Tera context. Lowercase so it
    /// composes directly with Bootstrap-style class names
    /// (`message message--success`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl FromStr for Level {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "debug" => Self::Debug,
            "info" => Self::Info,
            "success" => Self::Success,
            "warning" => Self::Warning,
            "error" => Self::Error,
            _ => return Err(()),
        })
    }
}

impl serde::Serialize for Level {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Level {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Level::from_str(&s).map_err(|()| serde::de::Error::custom(format!("invalid level: {s}")))
    }
}

/// One flash message. The `tags` field carries Django-style
/// `extra_tags` (free-form, typically CSS class names) that the
/// template renders alongside the level-derived class.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub level: Level,
    pub body: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tags: String,
}

/// Cookie name the messages framework writes / reads. Distinct from
/// the session / CSRF cookies so they don't collide.
pub const MESSAGES_COOKIE: &str = "rustango_messages";

/// Maximum number of messages staged in the cookie at any time.
/// Past this point the **oldest** message is dropped — chosen because
/// recently-pushed flashes are almost always the relevant ones (the
/// "thing just happened" feedback). A `tracing::warn` fires on each
/// drop so misbehaving callers surface in logs rather than silently
/// losing data. 50 is far above any reasonable POST→303 flow.
pub const MAX_MESSAGES: usize = 50;

/// Append a message to the storage cookie and return the updated
/// `Set-Cookie` header value the caller should attach to the
/// response. `extra_tags` is whatever class-name-ish string the
/// template wants alongside the level (`""` if you don't care).
///
/// ```ignore
/// let cookie = messages::push(
///     secret,
///     &headers,
///     messages::Level::Success,
///     "Saved.",
///     "fade-in",
/// );
/// ```
#[must_use]
pub fn push(
    secret: &[u8],
    headers: &axum::http::HeaderMap,
    level: Level,
    body: &str,
    extra_tags: &str,
) -> String {
    let mut existing = read_cookie(secret, headers).unwrap_or_default();
    existing.push(Message {
        level,
        body: body.to_owned(),
        tags: extra_tags.to_owned(),
    });
    // Cap total staged count so the cookie doesn't grow past the
    // 4KB browser limit. Drop oldest first (recent messages are the
    // ones the user just produced and most wants to see). Tracing
    // warn fires on each drop so misbehaving callers surface.
    while existing.len() > MAX_MESSAGES {
        let dropped = existing.remove(0);
        tracing::warn!(
            target: "rustango::messages",
            level = %dropped.level.as_str(),
            body = %dropped.body,
            "messages cookie exceeded MAX_MESSAGES={MAX_MESSAGES} — dropped oldest message"
        );
    }
    set_cookie(secret, &existing, false)
}

/// Read every staged message and produce a clear-cookie value the
/// caller should attach to the response so the messages don't show
/// up again on the next render. One-shot semantics: subsequent
/// drains see an empty list until something pushes again.
///
/// `clear_cookie` is `Some` whenever messages were drained (even an
/// empty Vec doesn't trigger a clear — we only clear when there was
/// something to clear).
#[must_use]
pub fn drain(secret: &[u8], headers: &axum::http::HeaderMap) -> (Vec<Message>, Option<String>) {
    let Some(messages) = read_cookie(secret, headers) else {
        return (Vec::new(), None);
    };
    let clear = set_cookie(secret, &[], true);
    (messages, Some(clear))
}

// ------------------------------------------------------------------ shortcuts

/// Shortcut for `push(Level::Debug, body, "")`.
#[must_use]
pub fn debug(secret: &[u8], headers: &axum::http::HeaderMap, body: &str) -> String {
    push(secret, headers, Level::Debug, body, "")
}

/// Shortcut for `push(Level::Info, body, "")`.
#[must_use]
pub fn info(secret: &[u8], headers: &axum::http::HeaderMap, body: &str) -> String {
    push(secret, headers, Level::Info, body, "")
}

/// Shortcut for `push(Level::Success, body, "")`.
#[must_use]
pub fn success(secret: &[u8], headers: &axum::http::HeaderMap, body: &str) -> String {
    push(secret, headers, Level::Success, body, "")
}

/// Shortcut for `push(Level::Warning, body, "")`.
#[must_use]
pub fn warning(secret: &[u8], headers: &axum::http::HeaderMap, body: &str) -> String {
    push(secret, headers, Level::Warning, body, "")
}

/// Shortcut for `push(Level::Error, body, "")`.
#[must_use]
pub fn error(secret: &[u8], headers: &axum::http::HeaderMap, body: &str) -> String {
    push(secret, headers, Level::Error, body, "")
}

// ------------------------------------------------------------------ Tera helper

/// Drain messages from the request cookie and stamp them into the
/// Tera context as `messages` — a list of `{level, body, tags}`
/// objects. Returns the clear-cookie the caller attaches to the
/// response. Pairs with [`crate::shortcuts::render`] /
/// [`crate::template_views`] for the standard
/// `{% for msg in messages %}…{% endfor %}` template pattern.
///
/// ```jinja
/// {% for msg in messages %}
///   <div class="message message--{{ msg.level }} {{ msg.tags }}">{{ msg.body }}</div>
/// {% endfor %}
/// ```
#[cfg(feature = "template_views")]
#[must_use]
pub fn stamp_into_context(
    secret: &[u8],
    headers: &axum::http::HeaderMap,
    ctx: &mut tera::Context,
) -> Option<String> {
    let (msgs, clear) = drain(secret, headers);
    ctx.insert("messages", &msgs);
    clear
}

// ------------------------------------------------------------------ cookie internals

fn read_cookie(secret: &[u8], headers: &axum::http::HeaderMap) -> Option<Vec<Message>> {
    let raw = headers
        .get(axum::http::header::COOKIE)
        .and_then(|h| h.to_str().ok())?;
    let value = raw
        .split(';')
        .map(str::trim)
        .find_map(|kv| kv.strip_prefix(MESSAGES_COOKIE)?.strip_prefix('='))?;
    let (payload_b64, sig_b64) = value.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let sig = URL_SAFE_NO_PAD.decode(sig_b64).ok()?;
    if !verify_sig(secret, &payload, &sig) {
        return None;
    }
    serde_json::from_slice(&payload).ok()
}

fn set_cookie(secret: &[u8], messages: &[Message], clearing: bool) -> String {
    let payload = serde_json::to_vec(messages).expect("Vec<Message> serializes");
    let sig = compute_sig(secret, &payload);
    let body = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(&sig)
    );
    // Path=/ so messages drain on any subsequent request, SameSite=Lax
    // so they survive the post-redirect GET, HttpOnly so JS can't
    // exfiltrate (messages can carry validation hints).
    let max_age = if clearing { "Max-Age=0; " } else { "" };
    format!("{MESSAGES_COOKIE}={body}; Path=/; SameSite=Lax; HttpOnly; {max_age}")
}

fn compute_sig(secret: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC accepts any key");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

fn verify_sig(secret: &[u8], payload: &[u8], sig: &[u8]) -> bool {
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC accepts any key");
    mac.update(payload);
    mac.verify_slice(sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret-32-bytes-aaaaaaaaaaaa";

    fn empty_headers() -> axum::http::HeaderMap {
        axum::http::HeaderMap::new()
    }

    fn headers_with(cookie: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(cookie).unwrap(),
        );
        h
    }

    /// Extract the cookie value portion from a `Set-Cookie` header
    /// string so we can fold it back into the next request's `Cookie:`.
    fn cookie_from_set(set: &str) -> String {
        let first = set.split(';').next().unwrap();
        first.to_owned()
    }

    #[test]
    fn level_round_trips_via_str() {
        for l in [
            Level::Debug,
            Level::Info,
            Level::Success,
            Level::Warning,
            Level::Error,
        ] {
            assert_eq!(Level::from_str(l.as_str()), Ok(l));
        }
        assert!(Level::from_str("nope").is_err());
    }

    #[test]
    fn push_then_drain_returns_message() {
        let set = push(SECRET, &empty_headers(), Level::Success, "Saved.", "");
        let cookie = cookie_from_set(&set);
        let (msgs, clear) = drain(SECRET, &headers_with(&cookie));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].level, Level::Success);
        assert_eq!(msgs[0].body, "Saved.");
        assert!(clear.is_some(), "drain should return a clear-cookie");
    }

    #[test]
    fn push_appends_to_existing_messages() {
        let set1 = push(SECRET, &empty_headers(), Level::Info, "one", "");
        let cookie1 = cookie_from_set(&set1);
        let set2 = push(SECRET, &headers_with(&cookie1), Level::Warning, "two", "");
        let cookie2 = cookie_from_set(&set2);
        let (msgs, _) = drain(SECRET, &headers_with(&cookie2));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].body, "one");
        assert_eq!(msgs[1].body, "two");
    }

    #[test]
    fn drain_with_no_cookie_returns_empty_no_clear() {
        let (msgs, clear) = drain(SECRET, &empty_headers());
        assert!(msgs.is_empty());
        assert!(clear.is_none(), "no cookie → nothing to clear");
    }

    #[test]
    fn drain_clears_cookie_via_max_age_zero() {
        let set = push(SECRET, &empty_headers(), Level::Info, "msg", "");
        let cookie = cookie_from_set(&set);
        let (_, clear) = drain(SECRET, &headers_with(&cookie));
        let clear = clear.unwrap();
        assert!(
            clear.contains("Max-Age=0"),
            "clear cookie should set Max-Age=0: {clear}"
        );
    }

    #[test]
    fn tampered_cookie_returns_empty_doesnt_crash() {
        let set = push(SECRET, &empty_headers(), Level::Success, "real", "");
        let cookie = cookie_from_set(&set);
        // Replace one character mid-payload so the signature no longer
        // verifies. Stays ASCII so the `&str` boundary doesn't shift.
        let eq = cookie.find('=').unwrap();
        let mut tampered = String::with_capacity(cookie.len());
        tampered.push_str(&cookie[..=eq]);
        let target = &cookie[eq + 1..];
        let first_char = target.chars().next().unwrap();
        // Flip "A↔B" style — base64url alphabet, so picking the
        // opposite-case version of the first char produces a different
        // valid base64url symbol.
        let flipped = if first_char.is_ascii_uppercase() {
            first_char.to_ascii_lowercase()
        } else if first_char.is_ascii_lowercase() {
            first_char.to_ascii_uppercase()
        } else {
            // Digit or `-`/`_` — toggle to a known different symbol.
            'X'
        };
        tampered.push(flipped);
        tampered.push_str(&target[first_char.len_utf8()..]);
        let (msgs, _) = drain(SECRET, &headers_with(&tampered));
        assert!(msgs.is_empty(), "tampered cookie must NOT round-trip");
    }

    #[test]
    fn wrong_secret_rejects_cookie() {
        let set = push(SECRET, &empty_headers(), Level::Success, "real", "");
        let cookie = cookie_from_set(&set);
        let (msgs, _) = drain(b"different-secret", &headers_with(&cookie));
        assert!(msgs.is_empty());
    }

    #[test]
    fn five_shortcuts_emit_the_right_level() {
        let cases: &[(fn(&[u8], &axum::http::HeaderMap, &str) -> String, Level)] = &[
            (debug, Level::Debug),
            (info, Level::Info),
            (success, Level::Success),
            (warning, Level::Warning),
            (error, Level::Error),
        ];
        for (shortcut, want) in cases {
            let set = shortcut(SECRET, &empty_headers(), "hi");
            let cookie = cookie_from_set(&set);
            let (msgs, _) = drain(SECRET, &headers_with(&cookie));
            assert_eq!(msgs[0].level, *want);
        }
    }

    #[test]
    fn extra_tags_round_trip_through_cookie() {
        let set = push(
            SECRET,
            &empty_headers(),
            Level::Warning,
            "Heads up",
            "dismissible fade",
        );
        let cookie = cookie_from_set(&set);
        let (msgs, _) = drain(SECRET, &headers_with(&cookie));
        assert_eq!(msgs[0].tags, "dismissible fade");
    }

    #[test]
    fn push_caps_at_max_messages_drops_oldest() {
        // Push enough to exceed MAX_MESSAGES (50) — the bound should
        // drop oldest first so the most-recent N survive.
        let mut headers = empty_headers();
        let total = MAX_MESSAGES + 5;
        for i in 0..total {
            let body = format!("msg-{i}");
            let set = push(SECRET, &headers, Level::Info, &body, "");
            let cookie = cookie_from_set(&set);
            headers = headers_with(&cookie);
        }
        let (msgs, _) = drain(SECRET, &headers);
        assert_eq!(msgs.len(), MAX_MESSAGES, "must cap at MAX_MESSAGES");
        // Oldest dropped → first surviving is `msg-5` (we pushed 0..54).
        assert_eq!(msgs[0].body, "msg-5");
        assert_eq!(msgs.last().unwrap().body, format!("msg-{}", total - 1));
    }

    #[cfg(feature = "template_views")]
    #[test]
    fn stamp_into_context_inserts_messages_as_list() {
        let set = push(SECRET, &empty_headers(), Level::Success, "Saved", "");
        let cookie = cookie_from_set(&set);
        let mut ctx = tera::Context::new();
        let clear = stamp_into_context(SECRET, &headers_with(&cookie), &mut ctx);
        assert!(clear.is_some());

        // Render through Tera to confirm the context shape.
        let mut tera = tera::Tera::default();
        tera.add_raw_template(
            "_",
            "{% for m in messages %}{{ m.level }}:{{ m.body }};{% endfor %}",
        )
        .unwrap();
        let out = tera.render("_", &ctx).unwrap();
        assert_eq!(out, "success:Saved;");
    }
}
