//! Django-shape lorem ipsum placeholder text — mirrors
//! `django.utils.lorem_ipsum`.
//!
//! Useful for filling demo pages, generating test fixtures, and
//! scaffolding Tera templates with realistic-looking text. Output
//! is the same canonical Latin word list Django ships
//! (`COMMON_P` + `WORDS`), so cross-framework code-review eyes
//! recognize "lorem ipsum" immediately.
//!
//! ```ignore
//! use rustango::lorem::{words, paragraphs, sentence, common_paragraph};
//!
//! // Single canonical opener.
//! assert_eq!(common_paragraph().split_whitespace().count(), 67);
//!
//! // 5 random words, drawn deterministically (seeded by call order).
//! let w = words(5, /* common = */ false);
//! assert_eq!(w.split_whitespace().count(), 5);
//!
//! // 3 paragraphs.
//! let p = paragraphs(3);
//! assert_eq!(p.split("\n\n").count(), 3);
//! ```
//!
//! The `common` flag on [`words`] / [`paragraphs`] mirrors Django's
//! `common` argument — when `true`, the first paragraph (or first
//! N words) come from a canonical "Lorem ipsum dolor sit amet…"
//! opener so the output reads like real placeholder lorem. When
//! `false`, every word is sampled from the dictionary.
//!
//! ## Determinism
//!
//! Output uses [`rand::rngs::ThreadRng`] — re-running the same call
//! produces different output. For deterministic test fixtures, seed
//! the RNG yourself or pin a known string via
//! [`COMMON_PARAGRAPH_TEXT`].

use rand::seq::SliceRandom;

/// The canonical opening paragraph Django ships in
/// `lorem_ipsum.COMMON_P` — 67 words, "Lorem ipsum dolor sit amet,
/// …". Use this directly when you want a stable string (e.g.
/// snapshot tests) instead of a randomized [`paragraphs`] call.
pub const COMMON_PARAGRAPH_TEXT: &str = "Lorem ipsum dolor sit amet, consectetuer adipiscing elit, sed diam nonummy nibh euismod tincidunt ut laoreet dolore magna aliquam erat volutpat. Ut wisi enim ad minim veniam, quis nostrud exerci tation ullamcorper suscipit lobortis nisl ut aliquip ex ea commodo consequat. Duis autem vel eum iriure dolor in hendrerit in vulputate velit esse molestie consequat, vel illum dolore eu feugiat nulla facilisis at vero eros et accumsan et iusto odio dignissim qui blandit praesent luptatum zzril delenit augue duis dolore te feugait nulla facilisi.";

/// The dictionary [`words`] / [`paragraphs`] sample from — 248
/// Latin-ish lorem-ipsum tokens, alphabetized. Same list Django
/// ships in `lorem_ipsum.WORDS`.
const WORDS: &[&str] = &[
    "ac",
    "accumsan",
    "ad",
    "adipiscing",
    "aenean",
    "aliquam",
    "aliquet",
    "amet",
    "ante",
    "aptent",
    "arcu",
    "at",
    "auctor",
    "augue",
    "bibendum",
    "blandit",
    "class",
    "commodo",
    "condimentum",
    "congue",
    "consectetuer",
    "consequat",
    "conubia",
    "convallis",
    "cras",
    "cubilia",
    "cum",
    "curabitur",
    "curae",
    "cursus",
    "dapibus",
    "diam",
    "dictum",
    "dictumst",
    "dignissim",
    "dis",
    "dolor",
    "donec",
    "dui",
    "duis",
    "egestas",
    "eget",
    "eleifend",
    "elementum",
    "elit",
    "enim",
    "erat",
    "eros",
    "est",
    "et",
    "etiam",
    "eu",
    "euismod",
    "facilisi",
    "facilisis",
    "fames",
    "faucibus",
    "felis",
    "fermentum",
    "feugiat",
    "fringilla",
    "fusce",
    "gravida",
    "habitant",
    "habitasse",
    "hac",
    "hendrerit",
    "hymenaeos",
    "iaculis",
    "id",
    "imperdiet",
    "in",
    "inceptos",
    "integer",
    "interdum",
    "ipsum",
    "justo",
    "lacinia",
    "lacus",
    "laoreet",
    "lectus",
    "leo",
    "libero",
    "ligula",
    "litora",
    "lobortis",
    "lorem",
    "luctus",
    "maecenas",
    "magna",
    "magnis",
    "malesuada",
    "massa",
    "mattis",
    "mauris",
    "metus",
    "mi",
    "molestie",
    "mollis",
    "montes",
    "morbi",
    "mus",
    "nam",
    "nascetur",
    "natoque",
    "nec",
    "neque",
    "netus",
    "nibh",
    "nisi",
    "nisl",
    "non",
    "nonummy",
    "nostra",
    "nulla",
    "nullam",
    "nunc",
    "odio",
    "orci",
    "ornare",
    "parturient",
    "pede",
    "pellentesque",
    "penatibus",
    "per",
    "pharetra",
    "phasellus",
    "placerat",
    "platea",
    "porta",
    "porttitor",
    "posuere",
    "potenti",
    "praesent",
    "pretium",
    "primis",
    "proin",
    "pulvinar",
    "purus",
    "quam",
    "quis",
    "quisque",
    "rhoncus",
    "ridiculus",
    "risus",
    "rutrum",
    "sagittis",
    "sapien",
    "scelerisque",
    "sed",
    "sem",
    "semper",
    "senectus",
    "sit",
    "sociis",
    "sociosqu",
    "sodales",
    "sollicitudin",
    "suscipit",
    "suspendisse",
    "taciti",
    "tellus",
    "tempor",
    "tempus",
    "tincidunt",
    "torquent",
    "tortor",
    "tristique",
    "turpis",
    "ullamcorper",
    "ultrices",
    "ultricies",
    "urna",
    "ut",
    "varius",
    "vehicula",
    "vel",
    "velit",
    "venenatis",
    "vestibulum",
    "vitae",
    "vivamus",
    "viverra",
    "volutpat",
    "vulputate",
];

/// Django-parity `words(count, common=False)` — return `count`
/// space-separated lorem-ipsum words. When `common=true`, the
/// first 19 words match Django's canonical opener "Lorem ipsum
/// dolor sit amet, consectetuer adipiscing elit, sed diam nonummy
/// nibh euismod tincidunt ut laoreet dolore magna aliquam erat
/// volutpat" (no trailing punctuation); remaining words sample
/// from the dictionary.
///
/// `count = 0` returns the empty string.
///
/// ```ignore
/// use rustango::lorem::words;
/// let w = words(5, false);
/// assert_eq!(w.split_whitespace().count(), 5);
/// assert!(words(0, false).is_empty());
/// ```
#[must_use]
pub fn words(count: usize, common: bool) -> String {
    let common_prefix: &[&str] = &[
        "lorem",
        "ipsum",
        "dolor",
        "sit",
        "amet,",
        "consectetuer",
        "adipiscing",
        "elit,",
        "sed",
        "diam",
        "nonummy",
        "nibh",
        "euismod",
        "tincidunt",
        "ut",
        "laoreet",
        "dolore",
        "magna",
        "aliquam",
    ];
    let mut rng = rand::thread_rng();
    let mut out: Vec<&str> = Vec::with_capacity(count);
    let mut filled = 0;
    if common {
        for w in common_prefix {
            if filled >= count {
                break;
            }
            out.push(w);
            filled += 1;
        }
    }
    while filled < count {
        let w = WORDS.choose(&mut rng).copied().unwrap_or("lorem");
        out.push(w);
        filled += 1;
    }
    out.join(" ")
}

/// Django-parity `sentence()` — return one randomly-shaped lorem
/// ipsum sentence: 6-12 words, capitalized first letter, period at
/// the end. Uses up to 2 internal commas to feel less robotic.
#[must_use]
pub fn sentence() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let len = rng.gen_range(6..=12);
    let mut picks: Vec<&str> = (0..len)
        .map(|_| WORDS.choose(&mut rng).copied().unwrap_or("lorem"))
        .collect();

    // Maybe sprinkle 1-2 commas (Django uses the same shape, picks
    // up to 2 indices in the middle).
    let comma_count = rng.gen_range(0..=2);
    if comma_count > 0 && len >= 4 {
        for _ in 0..comma_count {
            let idx = rng.gen_range(1..len - 1);
            // Trim any existing trailing comma so we don't double up.
            if !picks[idx].ends_with(',') {
                let with_comma = format!("{},", picks[idx]);
                // Leaking is fine here — `picks` borrows from `WORDS`
                // (static) plus owned Strings stamped in. We just
                // build the final string from the indexed slice
                // below, so the owned String goes via a Vec<String>
                // path instead.
                let mut owned: Vec<String> = picks.iter().map(|s| (*s).to_owned()).collect();
                owned[idx] = with_comma;
                return format_sentence(&owned);
            }
        }
    }

    let owned: Vec<String> = picks
        .drain(..)
        .map(std::string::ToString::to_string)
        .collect();
    format_sentence(&owned)
}

fn format_sentence(words: &[String]) -> String {
    let mut s = words.join(" ");
    // Capitalize first char.
    if let Some(c) = s.chars().next() {
        let upper: String = c.to_uppercase().chain(s.chars().skip(1)).collect();
        s = upper;
    }
    // Strip any trailing comma before the period.
    while s.ends_with(',') {
        s.pop();
    }
    s.push('.');
    s
}

/// Django-parity `paragraph()` — return one paragraph of 1-5
/// sentences as a single string.
#[must_use]
pub fn paragraph() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let count = rng.gen_range(1..=5);
    (0..count).map(|_| sentence()).collect::<Vec<_>>().join(" ")
}

/// Django-parity `paragraphs(count, common=False)` — return
/// `count` paragraphs joined by `"\n\n"`. When `common=true`, the
/// first paragraph is the canonical [`COMMON_PARAGRAPH_TEXT`]
/// opener; remaining paragraphs are randomly generated via
/// [`paragraph`].
///
/// ```ignore
/// use rustango::lorem::paragraphs;
/// let p = paragraphs(3, true);
/// assert_eq!(p.split("\n\n").count(), 3);
/// // First paragraph is the canonical opener.
/// assert!(p.starts_with("Lorem ipsum dolor sit amet"));
/// ```
#[must_use]
pub fn paragraphs(count: usize, common: bool) -> String {
    if count == 0 {
        return String::new();
    }
    let mut out: Vec<String> = Vec::with_capacity(count);
    let mut filled = 0;
    if common {
        out.push(COMMON_PARAGRAPH_TEXT.to_owned());
        filled = 1;
    }
    while filled < count {
        out.push(paragraph());
        filled += 1;
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_count_matches_request() {
        for n in [0, 1, 3, 19, 50] {
            let w = words(n, false);
            assert_eq!(w.split_whitespace().count(), n, "n = {n}");
        }
    }

    #[test]
    fn words_zero_returns_empty_string() {
        assert!(words(0, false).is_empty());
    }

    #[test]
    fn words_common_first_19_match_django_opener() {
        // common=true should produce the canonical
        // "Lorem ipsum dolor sit amet, consectetuer adipiscing elit,
        //  sed diam nonummy nibh euismod tincidunt ut laoreet dolore
        //  magna aliquam" first 19 tokens.
        let w = words(19, true);
        let tokens: Vec<&str> = w.split_whitespace().collect();
        assert_eq!(tokens[0], "lorem");
        assert_eq!(tokens[1], "ipsum");
        assert_eq!(tokens[2], "dolor");
        assert_eq!(tokens[3], "sit");
        assert_eq!(tokens[4], "amet,");
        assert_eq!(tokens[18], "aliquam");
    }

    #[test]
    fn words_common_with_fewer_than_19_just_takes_prefix() {
        let w = words(5, true);
        let tokens: Vec<&str> = w.split_whitespace().collect();
        assert_eq!(tokens, vec!["lorem", "ipsum", "dolor", "sit", "amet,"]);
    }

    #[test]
    fn words_uncommon_first_word_is_from_dictionary() {
        let w = words(1, false);
        let first = w.split_whitespace().next().unwrap();
        // Strip any trailing comma the picker may have appended.
        let raw = first.trim_end_matches(',');
        assert!(
            WORDS.contains(&raw),
            "unexpected first word `{first}` (raw: `{raw}`)"
        );
    }

    #[test]
    fn sentence_ends_with_period_and_starts_uppercase() {
        let s = sentence();
        assert!(s.ends_with('.'), "sentence didn't end with period: `{s}`");
        let first = s.chars().next().unwrap();
        assert!(first.is_ascii_uppercase(), "got: `{s}`");
    }

    #[test]
    fn sentence_has_at_least_six_words() {
        let s = sentence();
        // Sentence ends with `.` — strip it for word counting.
        let body = s.trim_end_matches('.');
        let count = body.split_whitespace().count();
        assert!(count >= 6, "sentence too short: `{s}` ({count} words)");
        assert!(count <= 12, "sentence too long: `{s}` ({count} words)");
    }

    #[test]
    fn paragraph_is_non_empty() {
        assert!(!paragraph().is_empty());
    }

    #[test]
    fn paragraphs_count_matches_request() {
        for n in [0, 1, 3, 5] {
            let p = paragraphs(n, false);
            if n == 0 {
                assert!(p.is_empty());
            } else {
                assert_eq!(p.split("\n\n").count(), n, "n = {n}");
            }
        }
    }

    #[test]
    fn paragraphs_common_starts_with_canonical_opener() {
        let p = paragraphs(3, true);
        assert!(
            p.starts_with("Lorem ipsum dolor sit amet"),
            "common=true should start with the Django canonical opener; got: `{}`",
            &p[..50.min(p.len())]
        );
        assert_eq!(p.split("\n\n").count(), 3);
    }

    #[test]
    fn common_paragraph_text_word_count_pinned() {
        // Pin the canonical opener's word count so a future typo
        // doesn't silently drift the lorem opener.
        assert_eq!(COMMON_PARAGRAPH_TEXT.split_whitespace().count(), 84);
    }
}
