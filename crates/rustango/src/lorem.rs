//! Lorem ipsum placeholder text.
//!
//! Use it to fill demo pages, build test fixtures, or draft Tera
//! templates with text that looks real.
//!
//! ```ignore
//! use rustango::lorem::{words, paragraphs, sentence, COMMON_PARAGRAPH_TEXT};
//!
//! // Canonical opener — stable string, 67 words.
//! assert_eq!(COMMON_PARAGRAPH_TEXT.split_whitespace().count(), 67);
//!
//! // 5 random words (RNG-driven; output varies per call).
//! let w = words(5, /* common = */ false);
//! assert_eq!(w.split_whitespace().count(), 5);
//!
//! // One random sentence — 6-12 words, capitalized, period at end.
//! let s = sentence();
//! assert!(s.ends_with('.'));
//!
//! // 3 paragraphs joined by blank lines.
//! let p = paragraphs(3, /* common = */ false);
//! assert_eq!(p.split("\n\n").count(), 3);
//! ```
//!
//! The `common` flag on [`words`] and [`paragraphs`] starts the output
//! with the canonical "Lorem ipsum dolor sit amet…" opener, so readers
//! see at once that it is placeholder text. With `common = false`
//! every word is picked at random.
//!
//! ## Determinism
//!
//! Output is random, so the same call gives different text each time.
//! For a fixed fixture, use [`COMMON_PARAGRAPH_TEXT`].
//!
//! [`words`]: crate::lorem::words
//! [`paragraphs`]: crate::lorem::paragraphs
//! [`COMMON_PARAGRAPH_TEXT`]: crate::lorem::COMMON_PARAGRAPH_TEXT

use rand::seq::SliceRandom;

/// The canonical "Lorem ipsum dolor sit amet, …" opener: 84 words.
/// Use it when you need
/// a stable string, such as a snapshot test, instead of a random
/// [`paragraphs`] call.
pub const COMMON_PARAGRAPH_TEXT: &str = "Lorem ipsum dolor sit amet, consectetuer adipiscing elit, sed diam nonummy nibh euismod tincidunt ut laoreet dolore magna aliquam erat volutpat. Ut wisi enim ad minim veniam, quis nostrud exerci tation ullamcorper suscipit lobortis nisl ut aliquip ex ea commodo consequat. Duis autem vel eum iriure dolor in hendrerit in vulputate velit esse molestie consequat, vel illum dolore eu feugiat nulla facilisis at vero eros et accumsan et iusto odio dignissim qui blandit praesent luptatum zzril delenit augue duis dolore te feugait nulla facilisi.";

/// The word pool [`words`] and [`paragraphs`] draw from: 185
/// Latin-looking tokens, in alphabetical order.
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

/// `count` words separated by spaces. With `common = true` the first
/// 19 words are the canonical opener and the rest are random.
///
/// `count = 0` returns an empty string.
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

/// One sentence: 6 to 12 words, a capital first letter, a full stop,
/// and up to two commas inside.
#[must_use]
pub fn sentence() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let len = rng.gen_range(6..=12);
    let mut picks: Vec<&str> = (0..len)
        .map(|_| WORDS.choose(&mut rng).copied().unwrap_or("lorem"))
        .collect();

    // Maybe add one or two commas somewhere in the middle.
    let comma_count = rng.gen_range(0..=2);
    if comma_count > 0 && len >= 4 {
        for _ in 0..comma_count {
            let idx = rng.gen_range(1..len - 1);
            // Skip a word that already ends in a comma.
            if !picks[idx].ends_with(',') {
                let with_comma = format!("{},", picks[idx]);
                // `picks` holds `&'static str`, so switch to owned
                // Strings before putting the comma version in.
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
    // Capitalize the first char.
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

/// One paragraph of 1 to 5 sentences.
#[must_use]
pub fn paragraph() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let count = rng.gen_range(1..=5);
    (0..count).map(|_| sentence()).collect::<Vec<_>>().join(" ")
}

/// `count` paragraphs joined by blank lines. With `common = true` the
/// first one is [`COMMON_PARAGRAPH_TEXT`] and the rest come from
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

#[cfg(feature = "template_views")]
mod tera_fn {
    use super::{paragraphs, words};
    use std::collections::HashMap;
    use tera::{to_value, Tera, Value};

    /// Register a `lorem` Tera function. Tera has no positional args,
    /// so the call shape is `{{ lorem(count=N, method="w") }}`.
    ///
    /// * `count` (default 1): how many words or paragraphs.
    /// * `method` (default `"b"`): `"w"` words, `"p"` paragraphs
    ///   wrapped in `<p>` tags, `"b"` paragraphs split by blank lines.
    /// * `common` (default `true`): start with the canonical opener.
    ///
    /// ```jinja
    /// {{ lorem(count=3, method="p") | safe }}
    /// {{ lorem(count=20, method="w") }}
    /// ```
    pub fn register_functions(tera: &mut Tera) {
        tera.register_function("lorem", lorem);
    }

    fn lorem(args: &HashMap<String, Value>) -> tera::Result<Value> {
        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .min(10_000) as usize;
        let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("b");
        let common = args.get("common").and_then(|v| v.as_bool()).unwrap_or(true);
        let out = match method {
            "w" => words(count, common),
            "p" => {
                let raw = paragraphs(count, common);
                raw.split("\n\n")
                    .map(|p| format!("<p>{p}</p>"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => paragraphs(count, common),
        };
        Ok(to_value(out)?)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn lorem_words_method_returns_n_words() {
            let mut args = HashMap::new();
            args.insert("count".into(), json!(5));
            args.insert("method".into(), json!("w"));
            let out = lorem(&args).unwrap();
            let s = out.as_str().unwrap();
            assert_eq!(s.split_whitespace().count(), 5);
        }

        #[test]
        fn lorem_paragraph_method_wraps_in_p_tags() {
            let mut args = HashMap::new();
            args.insert("count".into(), json!(2));
            args.insert("method".into(), json!("p"));
            let out = lorem(&args).unwrap();
            let s = out.as_str().unwrap();
            assert!(s.starts_with("<p>"), "got: {}", s);
            assert!(s.contains("</p>\n<p>"), "got: {}", s);
        }

        #[test]
        fn lorem_blank_method_emits_double_newlines() {
            let mut args = HashMap::new();
            args.insert("count".into(), json!(2));
            // "b" is the default.
            let out = lorem(&args).unwrap();
            let s = out.as_str().unwrap();
            assert!(s.contains("\n\n"), "got: {}", s);
            assert!(!s.contains("<p>"), "raw method should not wrap");
        }

        #[test]
        fn lorem_common_false_skips_canonical_opener() {
            let mut args = HashMap::new();
            args.insert("count".into(), json!(1));
            args.insert("method".into(), json!("p"));
            args.insert("common".into(), json!(false));
            let out = lorem(&args).unwrap();
            // Random output, so only the wrapper is checked.
            assert!(out.as_str().unwrap().starts_with("<p>"));
        }

        #[test]
        fn register_functions_wires_lorem_through_tera() {
            let mut tera = Tera::default();
            register_functions(&mut tera);
            tera.add_raw_template("t", r#"{{ lorem(count=3, method="w") }}"#)
                .unwrap();
            let out = tera.render("t", &tera::Context::new()).unwrap();
            assert_eq!(out.split_whitespace().count(), 3);
        }
    }
}

#[cfg(feature = "template_views")]
pub use tera_fn::register_functions;

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
    fn words_common_first_19_match_canonical_opener() {
        // common=true starts with the canonical 19-word opener.
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
        // Drop a trailing comma the picker may have added.
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
        // Drop the full stop before counting words.
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
            "common=true should start with the canonical opener; got: `{}`",
            &p[..50.min(p.len())]
        );
        assert_eq!(p.split("\n\n").count(), 3);
    }

    #[test]
    fn common_paragraph_text_word_count_pinned() {
        // Pinned so a typo in the constant cannot slip through.
        assert_eq!(COMMON_PARAGRAPH_TEXT.split_whitespace().count(), 84);
    }
}
