//! A port of the mailbox check `lettre` 0.11 runs (`Mailbox::from_str`):
//! its RFC 2822 parser, then the `email_address` local-part and domain
//! rules, with the IDNA retry. Lets the mail doubles refuse what
//! `SmtpMailer` refuses in builds without `email-smtp`.

/// `true` when lettre would parse `input` as a `Mailbox`.
pub(super) fn is_mailbox(input: &str) -> bool {
    let s = input.trim_start_matches(char::is_whitespace);
    // nom's `alt`: the first branch that parses wins, no backtracking after.
    let parsed = name_addr(s).or_else(|| addr_spec(s));
    let Some(((user, domain), rest)) = parsed else {
        return false;
    };
    rest.trim_start_matches(char::is_whitespace).is_empty()
        && valid_user(&user)
        && valid_domain(&domain)
}

/// `true` when lettre would parse `input` as a bare `Address`.
#[cfg(any(feature = "forms", all(test, feature = "email-smtp")))]
pub(super) fn is_address(input: &str) -> bool {
    input
        .rsplit_once('@')
        .is_some_and(|(user, domain)| valid_user(user) && valid_domain(domain))
}

type Parsed<'a, T> = Option<(T, &'a str)>;

fn skip_fws(s: &str) -> &str {
    s.trim_start_matches([' ', '\t'])
}

fn is_parser_atext(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(c) || c.len_utf8() > 1
}

/// `[CFWS] 1*atext`, the leading space dropped.
fn atom(s: &str) -> Parsed<'_, String> {
    let s = skip_fws(s);
    let end = s.find(|c| !is_parser_atext(c)).unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_owned(), &s[end..]))
}

/// `[CFWS] 1*atext *("." 1*atext)`.
fn dot_atom(s: &str) -> Parsed<'_, String> {
    let s = skip_fws(s);
    let (_, mut rest) = atom_text(s)?;
    while let Some(after) = rest.strip_prefix('.') {
        match atom_text(after) {
            Some((_, r)) => rest = r,
            None => break,
        }
    }
    let len = s.len() - rest.len();
    Some((s[..len].to_owned(), rest))
}

fn atom_text(s: &str) -> Parsed<'_, ()> {
    let end = s.find(|c| !is_parser_atext(c)).unwrap_or(s.len());
    (end > 0).then(|| ((), &s[end..]))
}

/// `DQUOTE *([FWS] qcontent) [FWS] DQUOTE`, unescaped; a run of white
/// space keeps its first character.
fn quoted_string(s: &str) -> Parsed<'_, String> {
    let mut rest = s.strip_prefix('"')?;
    let mut out = String::new();
    loop {
        let after_ws = skip_fws(rest);
        let mut chars = after_ws.chars();
        let qcontent = match chars.next() {
            Some(c) if matches!(c as u32, 33 | 35..=91 | 93..=126 | 1..=8 | 11 | 12 | 14..=31 | 127) => {
                Some(c)
            }
            Some('\\') => chars
                .next()
                .filter(|c| matches!(*c as u32, 1..=9 | 11 | 12 | 14..=127)),
            Some(c) if c.len_utf8() > 1 => Some(c),
            _ => None,
        };
        let Some(c) = qcontent else { break };
        if let Some(ws) = rest.chars().next().filter(|_| after_ws.len() < rest.len()) {
            out.push(ws);
        }
        out.push(c);
        rest = chars.as_str();
    }
    let rest = skip_fws(rest).strip_prefix('"')?;
    Some((out, rest))
}

/// `atom / quoted-string`.
fn word(s: &str) -> Parsed<'_, ()> {
    quoted_string(s)
        .map(|(_, r)| ((), r))
        .or_else(|| atom(s).map(|(_, r)| ((), r)))
}

/// `word *([FWS] (word / "."))`, lettre's display name.
fn phrase(s: &str) -> Parsed<'_, ()> {
    let (_, mut rest) = word(s)?;
    loop {
        let next = skip_fws(rest);
        match word(next).or_else(|| next.strip_prefix('.').map(|r| ((), r))) {
            Some((_, r)) => rest = r,
            None => return Some(((), rest)),
        }
    }
}

/// `[display-name] [CFWS] "<" addr-spec ">" [CFWS]`.
fn name_addr(s: &str) -> Parsed<'_, (String, String)> {
    let s = phrase(s).map_or(s, |(_, r)| r);
    let rest = skip_fws(s).strip_prefix('<')?;
    let (addr, rest) = addr_spec(rest)?;
    Some((addr, skip_fws(rest.strip_prefix('>')?)))
}

/// `local-part "@" domain`; lettre's domain is a dot-atom only.
fn addr_spec(s: &str) -> Parsed<'_, (String, String)> {
    let (user, rest) = dot_atom(s).or_else(|| quoted_string(s))?;
    let (domain, rest) = dot_atom(rest.strip_prefix('@')?)?;
    Some(((user, domain), rest))
}

// ---- `email_address` 0.2 rules, as lettre's `Address::new` applies them.

fn is_atext(c: char) -> bool {
    c.is_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(c) || is_utf8_non_ascii(c)
}

/// `email_address` tests the code point's bytes, not its UTF-8 form.
fn is_utf8_non_ascii(c: char) -> bool {
    matches!(
        (c as u32).to_be_bytes(),
        [0x00, 0x00, 0xC2..=0xDF, 0x80..=0xBF]
    )
}

fn is_atom(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_atext)
}

fn valid_user(user: &str) -> bool {
    if user.is_empty() || user.len() > 64 {
        return false;
    }
    if user.len() > 2 && user.starts_with('"') && user.ends_with('"') {
        return is_qcontent(&user[1..user.len() - 1]);
    }
    if user.starts_with('"') && user.ends_with('"') {
        return false;
    }
    user.split('.').all(is_atom)
}

fn is_qcontent(s: &str) -> bool {
    let qtext = |c: char| {
        c == '\x21'
            || ('\x23'..='\x5B').contains(&c)
            || ('\x5D'..='\x7E').contains(&c)
            || is_utf8_non_ascii(c)
    };
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        let ok = if c == '\\' {
            chars.next().is_some_and(|c| ('\x21'..='\x7E').contains(&c))
        } else {
            c == ' ' || c == '\t' || qtext(c)
        };
        if !ok {
            return false;
        }
    }
    true
}

fn valid_domain(domain: &str) -> bool {
    valid_ascii_domain(domain)
        || idna::domain_to_ascii(domain).is_ok_and(|ascii| valid_ascii_domain(&ascii))
}

fn valid_ascii_domain(domain: &str) -> bool {
    let text_ok = !domain.is_empty()
        && domain.len() <= 254
        && if let Some(lit) = domain.strip_prefix('[').and_then(|d| d.strip_suffix(']')) {
            lit.chars().all(|c| {
                ('\x21'..='\x5A').contains(&c)
                    || ('\x5E'..='\x7E').contains(&c)
                    || is_utf8_non_ascii(c)
            })
        } else {
            domain.split('.').all(|l| {
                l.starts_with(char::is_alphanumeric)
                    && l.ends_with(char::is_alphanumeric)
                    && l.len() <= 63
                    && is_atom(l)
            })
        };
    let ip = domain
        .strip_prefix('[')
        .and_then(|d| d.strip_suffix(']'))
        .unwrap_or(domain);
    text_ok || ip.parse::<std::net::IpAddr>().is_ok()
}
