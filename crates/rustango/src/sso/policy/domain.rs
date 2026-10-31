//! Parsed policy values: email domains, emails and plain token lists.

use std::fmt;
use std::str::FromStr;

/// Why a stored policy value does not parse. A provider with one refuses
/// every sign-in until it is fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PolicyError(pub String);

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PolicyError {}

fn bad(what: impl Into<String>) -> PolicyError {
    PolicyError(what.into())
}

/// One domain rule: `client.org` matches only that domain, `*.client.org`
/// only its subdomains. Matching is on whole labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainRule {
    domain: String,
    subdomains: bool,
}

impl DomainRule {
    /// The domain, without the `*.`.
    #[must_use]
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Whether `host` (any ASCII case, trailing dot allowed) is covered.
    #[must_use]
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if !self.subdomains {
            return host == self.domain;
        }
        host.len() > self.domain.len() + 1
            && host.ends_with(&self.domain)
            && host.as_bytes()[host.len() - self.domain.len() - 1] == b'.'
    }
}

fn valid_label(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= 63
        && !l.starts_with('-')
        && !l.ends_with('-')
        && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl FromStr for DomainRule {
    type Err = PolicyError;

    fn from_str(s: &str) -> Result<Self, PolicyError> {
        let raw = s.trim();
        let (subdomains, rest) = match raw.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        let domain = rest.trim_end_matches('.').to_ascii_lowercase();
        if !domain.is_ascii() {
            return Err(bad(format!("`{raw}`: write the domain in punycode (xn--)")));
        }
        let labels: Vec<&str> = domain.split('.').collect();
        if domain.len() > 253 || labels.len() < 2 || !labels.iter().all(|l| valid_label(l)) {
            return Err(bad(format!(
                "`{raw}` is not a domain like `client.org` or `*.client.org`"
            )));
        }
        Ok(Self { domain, subdomains })
    }
}

impl fmt::Display for DomainRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.subdomains {
            write!(f, "*.{}", self.domain)
        } else {
            f.write_str(&self.domain)
        }
    }
}

/// Split a stored list on commas and newlines, dropping blanks.
fn items(s: &str) -> impl Iterator<Item = &str> {
    s.split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|i| !i.is_empty())
}

/// A comma or newline separated list of [`DomainRule`]s.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DomainList(Vec<DomainRule>);

impl DomainList {
    /// A stored column: blank or NULL is no rule (`None`).
    ///
    /// # Errors
    /// The first entry that is not a domain rule.
    pub fn parse_opt(s: Option<&str>) -> Result<Option<Self>, PolicyError> {
        match s.map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(s) => s.parse().map(Some),
        }
    }

    /// The first rule covering `host`.
    #[must_use]
    pub fn find(&self, host: &str) -> Option<&DomainRule> {
        self.0.iter().find(|r| r.matches(host))
    }

    #[must_use]
    pub fn rules(&self) -> &[DomainRule] {
        &self.0
    }
}

impl FromStr for DomainList {
    type Err = PolicyError;

    fn from_str(s: &str) -> Result<Self, PolicyError> {
        let mut out: Vec<DomainRule> = Vec::new();
        for item in items(s) {
            let rule: DomainRule = item.parse()?;
            if !out.contains(&rule) {
                out.push(rule);
            }
        }
        Ok(Self(out))
    }
}

impl fmt::Display for DomainList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, r) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{r}")?;
        }
        Ok(())
    }
}

/// A list of opaque ids, e.g. Microsoft directory ids. Compared ASCII-case-blind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TokenList(Vec<String>);

impl TokenList {
    /// A stored column: blank or NULL is no rule (`None`).
    ///
    /// # Errors
    /// An entry with characters other than letters, digits, `-`, `_` and `.`.
    pub fn parse_opt(s: Option<&str>) -> Result<Option<Self>, PolicyError> {
        match s.map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(s) => s.parse().map(Some),
        }
    }

    #[must_use]
    pub fn contains(&self, token: &str) -> bool {
        self.0.iter().any(|t| t.eq_ignore_ascii_case(token))
    }
}

impl FromStr for TokenList {
    type Err = PolicyError;

    fn from_str(s: &str) -> Result<Self, PolicyError> {
        let mut out: Vec<String> = Vec::new();
        for item in items(s) {
            if item.len() > 128
                || !item
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                return Err(bad(format!("`{item}` is not a valid id")));
            }
            let t = item.to_ascii_lowercase();
            if !out.contains(&t) {
                out.push(t);
            }
        }
        Ok(Self(out))
    }
}

impl fmt::Display for TokenList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(", "))
    }
}

/// An email address, ASCII-lowercased, with exactly one `@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAddr(String);

impl EmailAddr {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The part after `@`.
    #[must_use]
    pub fn domain(&self) -> &str {
        self.0.rsplit_once('@').map_or("", |(_, d)| d)
    }

    /// The part before `@`.
    #[must_use]
    pub fn local(&self) -> &str {
        self.0.rsplit_once('@').map_or("", |(l, _)| l)
    }
}

impl FromStr for EmailAddr {
    type Err = PolicyError;

    fn from_str(s: &str) -> Result<Self, PolicyError> {
        let s = s.trim();
        match s.split_once('@') {
            Some((local, domain))
                if !local.is_empty()
                    && !domain.is_empty()
                    && !domain.contains('@')
                    && !s.chars().any(char::is_whitespace) =>
            {
                Ok(Self(s.to_ascii_lowercase()))
            }
            _ => Err(bad(format!("`{s}` is not an email address"))),
        }
    }
}

impl fmt::Display for EmailAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(s: &str) -> DomainRule {
        s.parse().unwrap()
    }

    #[test]
    fn domain_rules_match_whole_labels() {
        let apex = rule("client.org");
        let sub = rule("*.client.org");
        let table = [
            ("client.org", true, false),
            ("CLIENT.org.", true, false),
            ("eu.client.org", false, true),
            ("a.eu.client.org", false, true),
            ("evilclient.org", false, false),
            ("client.org.evil.com", false, false),
            ("xclient.org", false, false),
            (".client.org", false, false),
            ("org", false, false),
        ];
        for (host, by_apex, by_sub) in table {
            assert_eq!(apex.matches(host), by_apex, "client.org vs {host}");
            assert_eq!(sub.matches(host), by_sub, "*.client.org vs {host}");
        }
    }

    #[test]
    fn domain_rules_refuse_what_is_not_a_domain() {
        for bad in [
            "*",
            "*.",
            "org",
            "*.org",
            "a@b.org",
            "client .org",
            "cli*nt.org",
            "-a.org",
            "a-.org",
            "a..org",
            "münchen.de",
            "",
            ".",
            "*.*.org",
        ] {
            assert!(bad.parse::<DomainRule>().is_err(), "{bad:?} parsed");
        }
        assert_eq!(rule(" Client.ORG. ").to_string(), "client.org");
        assert_eq!(
            rule("*.xn--mnchen-3ya.de").to_string(),
            "*.xn--mnchen-3ya.de"
        );
    }

    #[test]
    fn lists_normalise_and_refuse_a_bad_entry() {
        let l: DomainList = "Client.org,\n *.client.org ,client.org,".parse().unwrap();
        assert_eq!(l.to_string(), "client.org, *.client.org");
        assert!("client.org, gmail".parse::<DomainList>().is_err());
        assert_eq!(DomainList::parse_opt(Some("  ")).unwrap(), None);
        assert_eq!(DomainList::parse_opt(None).unwrap(), None);
        let t: TokenList = "ABC-1, abc-1\n9f2".parse().unwrap();
        assert_eq!(t.to_string(), "abc-1, 9f2");
        assert!(t.contains("ABC-1"));
        assert!("a b".parse::<TokenList>().is_err());
    }

    #[test]
    fn email_addr_lowercases_and_splits() {
        let e: EmailAddr = "Ann@Client.ORG".parse().unwrap();
        assert_eq!((e.local(), e.domain()), ("ann", "client.org"));
        for bad in ["ann", "@x.org", "ann@", "a@b@c", "a b@c.org"] {
            assert!(bad.parse::<EmailAddr>().is_err(), "{bad}");
        }
    }
}
