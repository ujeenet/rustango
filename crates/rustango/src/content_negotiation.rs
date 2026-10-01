//! Pick a response format from the client's `Accept` header.
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
//! It reads RFC 7231 `Accept` headers, including `q=` values.

/// Choose the media type to send back.
///
/// `accept` is the raw header, such as
/// `"application/json,text/html;q=0.9,*/*;q=0.5"`. `available` is
/// what your handler can produce, best first.
///
/// Each type takes the `q` of the most specific range that matches it
/// (RFC 9110 §12.5.1), whatever the header order; `q=0` means "not
/// acceptable". A higher `q` wins; at the same `q` an exact match beats
/// a wildcard; after that the order of `available` decides. Returns
/// `None` when `available` is empty or nothing on offer is acceptable.
#[must_use]
pub fn negotiate<'a, S: AsRef<str>>(accept: &str, available: &'a [S]) -> Option<&'a str> {
    if available.is_empty() {
        return None;
    }
    if accept.trim().is_empty() {
        // No Accept header, so take our own first choice.
        return Some(available[0].as_ref());
    }

    let prefs = parse_accept(accept);
    // (server index, q in thousandths, specificity); `>` keeps the first on ties.
    let mut best: Option<(usize, u16, u8)> = None;
    for (idx, srv) in available.iter().enumerate() {
        let Some((q, spec)) = prefs
            .iter()
            .filter_map(|p| specificity(p, srv.as_ref()).map(|spec| (p.q, spec)))
            .max_by_key(|&(_, spec)| spec)
        else {
            continue;
        };
        if q == 0 {
            continue;
        }
        if best.is_none_or(|(_, bq, bs)| (q, spec) > (bq, bs)) {
            best = Some((idx, q, spec));
        }
    }
    best.map(|(idx, _, _)| available[idx].as_ref())
}

#[derive(Debug, Clone)]
struct AcceptPref {
    type_: String,
    subtype: String,
    /// `q` in thousandths, the precision RFC 9110 allows.
    q: u16,
}

fn parse_accept(header: &str) -> Vec<AcceptPref> {
    header
        .split(',')
        .filter_map(|raw| {
            let mut parts = raw.split(';').map(str::trim);
            let media = parts.next()?;
            let (type_, subtype) = media.split_once('/')?;
            let mut q = 1000;
            for kv in parts {
                if let Some(rest) = kv.strip_prefix("q=").or_else(|| kv.strip_prefix("Q=")) {
                    // A non-finite q is as unusable as an unparsable one.
                    if let Some(parsed) = rest.trim().parse::<f32>().ok().filter(|p| p.is_finite())
                    {
                        q = (parsed.clamp(0.0, 1.0) * 1000.0).round() as u16;
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

/// How specific `pref` is for `srv_type` (2 exact, 1 `type/*`, 0
/// `*/*`), or `None` when it does not match.
fn specificity(pref: &AcceptPref, srv_type: &str) -> Option<u8> {
    let (s_type, s_subtype) = srv_type.split_once('/')?;
    let type_matches = pref.type_ == "*" || pref.type_.eq_ignore_ascii_case(s_type);
    let subtype_matches = pref.subtype == "*" || pref.subtype.eq_ignore_ascii_case(s_subtype);
    if !(type_matches && subtype_matches) {
        return None;
    }
    Some(match (pref.type_ == "*", pref.subtype == "*") {
        (false, false) => 2,
        (false, true) => 1,
        _ => 0,
    })
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
        // We list JSON first, but the browser asks for HTML at q=1.
        assert_eq!(
            negotiate(header, &["application/json", "text/html"]),
            Some("text/html"),
        );
    }

    /// `q=0` refuses a type, and the most specific range sets its q,
    /// not the first one listed (#1957).
    #[test]
    fn q_zero_refuses_and_specificity_beats_header_order() {
        assert_eq!(
            negotiate("application/json;q=0", &["application/json"]),
            None
        );
        assert_eq!(
            negotiate(
                "*/*, application/json;q=0",
                &["application/json", "text/html"]
            ),
            Some("text/html"),
        );
        assert_eq!(
            negotiate("text/*;q=0.1, text/html", &["text/plain", "text/html"]),
            Some("text/html"),
        );
    }

    /// A NaN or infinite q keeps the default q, like an unparsable one.
    #[test]
    fn non_finite_q_keeps_the_default() {
        for q in ["NaN", "-inf", "inf"] {
            assert_eq!(
                negotiate(
                    &format!("text/html;q=0.5, application/json;q={q}"),
                    &["text/html", "application/json"]
                ),
                Some("application/json"),
                "q={q}"
            );
        }
    }

    #[test]
    fn exact_type_beats_wildcard_at_same_q() {
        // Exact `text/html` beats `*/*`, both at q=1.0.
        assert_eq!(
            negotiate("text/html,*/*", &["application/json", "text/html"]),
            Some("text/html"),
        );
    }
}
