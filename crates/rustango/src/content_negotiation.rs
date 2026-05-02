//! HTTP content negotiation — pick the best response format from the
//! client's `Accept` header.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::content_negotiation::negotiate;
//!
//! let format = negotiate(
//!     accept_header,
//!     &["application/json", "text/html", "text/plain"],
//! );
//! match format {
//!     Some("application/json") => render_json(...),
//!     Some("text/html") => render_html(...),
//!     _ => render_plaintext(...),
//! }
//! ```
//!
//! Parses RFC 7231 `Accept` (with `q=` quality values) and picks the
//! highest-priority format that the server can produce.

/// Pick the best media type the server can produce, given the client's
/// `Accept` header value and the list of types the server supports.
///
/// `accept` is the raw header value (e.g. `"application/json,text/html;q=0.9,*/*;q=0.5"`).
/// `available` is the ordered list of types the server can serve.
///
/// Matching rules:
/// - Exact match wins over wildcard match
/// - Higher q-value wins over lower
/// - Server's `available` order breaks ties at equal q
/// - Returns `None` only when `available` is empty or no client preference matches
///   (a `*/*` client preference will always match the first available type)
#[must_use]
pub fn negotiate<'a, S: AsRef<str>>(accept: &str, available: &'a [S]) -> Option<&'a str> {
    if available.is_empty() {
        return None;
    }
    if accept.trim().is_empty() {
        // No Accept header → first available
        return Some(available[0].as_ref());
    }

    let prefs = parse_accept(accept);
    let mut best: Option<(usize, f32)> = None; // (server-index, score)

    for (idx, srv) in available.iter().enumerate() {
        let srv_str = srv.as_ref();
        for pref in &prefs {
            let score = match_score(pref, srv_str);
            if let Some(s) = score {
                let beats = match best {
                    None => true,
                    Some((_, prev_score)) => s > prev_score,
                };
                if beats {
                    best = Some((idx, s));
                }
                break; // server's order matters at equal q — stop at first match
            }
        }
    }

    best.map(|(idx, _)| available[idx].as_ref())
}

#[derive(Debug, Clone)]
struct AcceptPref {
    type_: String,
    subtype: String,
    q: f32,
}

fn parse_accept(header: &str) -> Vec<AcceptPref> {
    header
        .split(',')
        .filter_map(|raw| {
            let mut parts = raw.split(';').map(str::trim);
            let media = parts.next()?;
            let (type_, subtype) = media.split_once('/')?;
            let mut q = 1.0;
            for kv in parts {
                if let Some(rest) = kv.strip_prefix("q=") {
                    if let Ok(parsed) = rest.parse::<f32>() {
                        q = parsed;
                    }
                }
            }
            Some(AcceptPref {
                type_: type_.to_ascii_lowercase(),
                subtype: subtype.to_ascii_lowercase(),
                q,
            })
        })
        .collect()
}

/// Return the q-value of the match, or `None` if `pref` doesn't match `srv_type`.
fn match_score(pref: &AcceptPref, srv_type: &str) -> Option<f32> {
    let (s_type, s_subtype) = srv_type.split_once('/')?;
    let s_type = s_type.to_ascii_lowercase();
    let s_subtype = s_subtype.to_ascii_lowercase();

    let type_matches = pref.type_ == "*" || pref.type_ == s_type;
    let subtype_matches = pref.subtype == "*" || pref.subtype == s_subtype;

    if type_matches && subtype_matches {
        // Slight bonus for exact (non-wildcard) matches so `text/html`
        // beats `*/*` at the same q-value.
        let exact_bonus = match (pref.type_ == "*", pref.subtype == "*") {
            (false, false) => 0.0001, // most specific
            (false, true) => 0.00005,
            _ => 0.0,
        };
        Some(pref.q + exact_bonus)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert_eq!(
            negotiate("application/json", &["application/json", "text/html"]),
            Some("application/json"),
        );
    }

    #[test]
    fn highest_q_wins() {
        assert_eq!(
            negotiate(
                "text/html;q=0.5,application/json;q=0.9",
                &["application/json", "text/html"],
            ),
            Some("application/json"),
        );
    }

    #[test]
    fn wildcard_falls_back_to_first_available() {
        assert_eq!(
            negotiate("*/*", &["application/json", "text/html"]),
            Some("application/json"),
        );
    }

    #[test]
    fn type_wildcard_matches() {
        assert_eq!(
            negotiate("text/*", &["application/json", "text/html"]),
            Some("text/html"),
        );
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(
            negotiate("application/xml", &["application/json", "text/html"]),
            None,
        );
    }

    #[test]
    fn empty_accept_picks_first_available() {
        assert_eq!(
            negotiate("", &["application/json", "text/html"]),
            Some("application/json"),
        );
    }

    #[test]
    fn empty_available_returns_none() {
        let empty: &[&str] = &[];
        assert_eq!(negotiate("application/json", empty), None);
    }

    #[test]
    fn case_insensitive_match() {
        assert_eq!(
            negotiate("APPLICATION/JSON", &["application/json"]),
            Some("application/json"),
        );
    }

    #[test]
    fn complex_real_world_browser_accept() {
        let header = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
        // Server prefers JSON, falls back to HTML
        assert_eq!(
            negotiate(header, &["application/json", "text/html"]),
            Some("text/html"),
        );
    }

    #[test]
    fn exact_type_beats_wildcard_at_same_q() {
        // "text/html" (q=1.0 exact) should beat "*/*" (q=1.0 wildcard)
        assert_eq!(
            negotiate("text/html,*/*", &["application/json", "text/html"]),
            Some("text/html"),
        );
    }
}
