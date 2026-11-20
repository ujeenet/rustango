//! Enum fields stored as text.
//!
//! `#[derive(rustango::TextEnum)]` on a fieldless enum implements
//! [`TextEnum`]; mark the model field `#[rustango(text_enum, max_length = N)]`.
//! The column is plain text on every backend; reads refuse a value that
//! names no variant with an [`UnknownVariant`] decode error.
//!
//! ```
//! use rustango::core::TextEnum;
//!
//! #[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
//! #[text_enum(rename_all = "snake_case")]
//! pub enum Plan {
//!     Free,
//!     #[text_enum(rename = "pro", alias = "professional", label = "Pro plan")]
//!     Professional,
//! }
//!
//! assert_eq!(Plan::Professional.as_str(), "pro");
//! assert_eq!(Plan::from_db("professional").unwrap(), Plan::Professional);
//! assert!(Plan::from_db("Pro").is_err());
//! assert_eq!(Plan::Free.to_string(), "free");
//! ```
//!
//! On a model, `max_length` is required and a `default` is a quoted stored
//! name; filters take the enum:
//!
//! ```
//! use rustango::core::Column as _;
//!
//! #[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
//! #[text_enum(rename_all = "snake_case")]
//! pub enum Status { Draft, Live }
//!
//! #[derive(rustango::Model)]
//! #[rustango(table = "te_doc_post")]
//! pub struct Post {
//!     #[rustango(primary_key)]
//!     pub id: rustango::Auto<i64>,
//!     #[rustango(text_enum, max_length = 8, default = "'draft'")]
//!     pub status: Status,
//! }
//!
//! fn main() {
//!     let _ = Post::status.eq(Status::Live);
//! }
//! ```
//!
//! A filter does not take a string:
//!
//! ```compile_fail
//! # use rustango::core::Column as _;
//! # #[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
//! # #[text_enum(rename_all = "snake_case")]
//! # pub enum Status { Draft, Live }
//! # #[derive(rustango::Model)]
//! # #[rustango(table = "te_doc_post")]
//! # pub struct Post {
//! #     #[rustango(primary_key)]
//! #     pub id: rustango::Auto<i64>,
//! #     #[rustango(text_enum, max_length = 8)]
//! #     pub status: Status,
//! # }
//! fn main() {
//!     let _ = Post::status.eq("live");
//! }
//! ```
//!
//! `max_length` shorter than the longest stored value does not compile:
//!
//! ```compile_fail
//! # #[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
//! # #[text_enum(rename_all = "snake_case")]
//! # pub enum Status { Draft, Live }
//! #[derive(rustango::Model)]
//! #[rustango(table = "te_doc_post")]
//! pub struct Post {
//!     #[rustango(primary_key)]
//!     pub id: rustango::Auto<i64>,
//!     #[rustango(text_enum, max_length = 4)]
//!     pub status: Status,
//! }
//! # fn main() {}
//! ```
//!
//! Nor does a `default` that names no variant:
//!
//! ```compile_fail
//! # #[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
//! # #[text_enum(rename_all = "snake_case")]
//! # pub enum Status { Draft, Live }
//! #[derive(rustango::Model)]
//! #[rustango(table = "te_doc_post")]
//! pub struct Post {
//!     #[rustango(primary_key)]
//!     pub id: rustango::Auto<i64>,
//!     #[rustango(text_enum, max_length = 8, default = "'gone'")]
//!     pub status: Status,
//! }
//! # fn main() {}
//! ```
//!
//! Nor `choices` (or `blank`, `citext`, `validators`) on the field:
//!
//! ```compile_fail
//! # #[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
//! # #[text_enum(rename_all = "snake_case")]
//! # pub enum Status { Draft, Live }
//! #[derive(rustango::Model)]
//! #[rustango(table = "te_doc_post")]
//! pub struct Post {
//!     #[rustango(primary_key)]
//!     pub id: rustango::Auto<i64>,
//!     #[rustango(text_enum, max_length = 8, choices = "draft, live")]
//!     pub status: Status,
//! }
//! # fn main() {}
//! ```
//!
//! ```compile_fail
//! # #[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
//! # #[text_enum(rename_all = "snake_case")]
//! # pub enum Status { Draft, Live }
//! #[derive(rustango::Model)]
//! #[rustango(table = "te_doc_post")]
//! pub struct Post {
//!     #[rustango(primary_key)]
//!     pub id: rustango::Auto<i64>,
//!     #[rustango(text_enum, max_length = 8, blank)]
//!     pub status: Status,
//! }
//! # fn main() {}
//! ```
//!
//! A variant carries no data:
//!
//! ```compile_fail
//! #[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
//! pub enum Status { Draft, Live(u8) }
//! # fn main() {}
//! ```
//!
//! A field of a type that is not a `TextEnum` does not compile:
//!
//! ```compile_fail
//! #[derive(Clone, Copy, Debug, PartialEq, Eq)]
//! pub enum Status { Draft, Live }
//! #[derive(rustango::Model)]
//! #[rustango(table = "te_doc_post")]
//! pub struct Post {
//!     #[rustango(primary_key)]
//!     pub id: rustango::Auto<i64>,
//!     #[rustango(text_enum, max_length = 8)]
//!     pub status: Status,
//! }
//! # fn main() {}
//! ```

use std::fmt;

/// A fieldless enum stored as one text value per variant.
///
/// Derive it with `#[derive(rustango::TextEnum)]`. `VARIANTS` is the one
/// source: `as_str`, `from_db`, form choices and the OpenAPI `enum` all
/// come from it. The derive also emits `Display`, `FromStr` and serde as
/// the stored string, so do not derive serde on the enum as well.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a `TextEnum`",
    label = "add `#[derive(rustango::TextEnum)]` to `{Self}`"
)]
pub trait TextEnum: Copy + Eq + Send + Sync + Unpin + 'static {
    /// The Rust type name, for error messages.
    const NAME: &'static str;
    /// Every variant with its stored value and its label.
    const VARIANTS: &'static [(Self, &'static str, &'static str)];
    /// Older stored spellings `from_db` still reads; never written.
    const ALIASES: &'static [(Self, &'static str)] = &[];
    /// The byte length of the longest stored value.
    const MAX_LEN: usize = longest_value(Self::VARIANTS);

    /// The stored value.
    fn as_str(&self) -> &'static str {
        Self::VARIANTS
            .iter()
            .find(|(v, _, _)| v == self)
            .map_or("", |(_, s, _)| s)
    }

    /// The label forms and the admin show.
    fn label(&self) -> &'static str {
        Self::VARIANTS
            .iter()
            .find(|(v, _, _)| v == self)
            .map_or("", |(_, _, l)| l)
    }

    /// Read a stored value: a stored name or an alias, exact match only.
    ///
    /// # Errors
    /// [`UnknownVariant`] when `stored` names no variant.
    fn from_db(stored: &str) -> Result<Self, UnknownVariant> {
        Self::VARIANTS
            .iter()
            .find(|(_, s, _)| *s == stored)
            .map(|(v, _, _)| *v)
            .or_else(|| {
                Self::ALIASES
                    .iter()
                    .find(|(_, s)| *s == stored)
                    .map(|(v, _)| *v)
            })
            .ok_or_else(|| UnknownVariant::new::<Self>(stored))
    }
}

/// `true` when `stored` is an alias of `E`, not its stored name.
#[must_use]
pub fn is_alias<E: TextEnum>(stored: &str) -> bool {
    E::ALIASES.iter().any(|(_, s)| *s == stored)
}

/// The longest stored value in `variants`, in bytes.
#[doc(hidden)]
#[must_use]
pub const fn longest_value<E>(variants: &[(E, &'static str, &'static str)]) -> usize {
    let mut i = 0;
    let mut max = 0;
    while i < variants.len() {
        let n = variants[i].1.len();
        if n > max {
            max = n;
        }
        i += 1;
    }
    max
}

/// `true` when some variant is stored as `value`. Backs the derive's
/// const check on a `default`.
#[doc(hidden)]
#[must_use]
pub const fn has_value<E>(variants: &[(E, &'static str, &'static str)], value: &str) -> bool {
    let mut i = 0;
    while i < variants.len() {
        if const_str_eq(variants[i].1, value) {
            return true;
        }
        i += 1;
    }
    false
}

const fn const_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `E::VARIANTS` as `(value, label)` pairs, for `FieldSchema::choices`.
#[doc(hidden)]
#[must_use]
pub const fn choices<E: TextEnum, const N: usize>() -> [(&'static str, &'static str); N] {
    let mut out = [("", ""); N];
    let mut i = 0;
    while i < N {
        out[i] = (E::VARIANTS[i].1, E::VARIANTS[i].2);
        i += 1;
    }
    out
}

/// The most of a bad stored value an error repeats, in bytes.
const SHOWN_BYTES: usize = 64;

/// A stored value that names no variant of a [`TextEnum`]. A read
/// reports it as the source of a `sqlx::Error::ColumnDecode`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnknownVariant {
    /// The enum's Rust name.
    pub type_name: &'static str,
    /// The stored value, cut to 64 bytes with control characters escaped.
    pub value: String,
    /// The stored names `from_db` accepts (aliases left out).
    pub allowed: Vec<&'static str>,
}

impl UnknownVariant {
    /// The error for `stored` on `E`.
    #[must_use]
    pub fn new<E: TextEnum>(stored: &str) -> Self {
        Self {
            type_name: E::NAME,
            value: shown_value(stored),
            allowed: E::VARIANTS.iter().map(|(_, s, _)| *s).collect(),
        }
    }
}

/// `stored` cut on a char boundary at 64 bytes, control characters escaped.
fn shown_value(stored: &str) -> String {
    let mut end = stored.len().min(SHOWN_BYTES);
    while !stored.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 3);
    for c in stored[..end].chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    if end < stored.len() {
        out.push('…');
    }
    out
}

impl fmt::Display for UnknownVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown value `{}` for {}; expected one of ",
            self.value, self.type_name
        )?;
        for (i, s) in self.allowed.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "`{s}`")?;
        }
        Ok(())
    }
}

impl std::error::Error for UnknownVariant {}

impl<E: TextEnum> From<E> for super::SqlValue {
    fn from(e: E) -> Self {
        Self::String(e.as_str().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Mode {
        A,
        Bee,
    }

    impl TextEnum for Mode {
        const NAME: &'static str = "Mode";
        const VARIANTS: &'static [(Self, &'static str, &'static str)] =
            &[(Self::A, "a", "A"), (Self::Bee, "bee", "Bee")];
        const ALIASES: &'static [(Self, &'static str)] = &[(Self::Bee, "b")];
    }

    #[test]
    fn provided_methods_follow_variants() {
        assert_eq!(Mode::Bee.as_str(), "bee");
        assert_eq!(Mode::Bee.label(), "Bee");
        assert_eq!(Mode::from_db("a"), Ok(Mode::A));
        assert_eq!(Mode::from_db("b"), Ok(Mode::Bee));
        assert!(Mode::from_db("A").is_err());
        assert!(Mode::from_db(" a").is_err());
        assert_eq!(Mode::MAX_LEN, 3);
        assert!(is_alias::<Mode>("b") && !is_alias::<Mode>("bee"));
        assert!(has_value(Mode::VARIANTS, "bee") && !has_value(Mode::VARIANTS, "b"));
        assert_eq!(choices::<Mode, 2>(), [("a", "A"), ("bee", "Bee")]);
    }

    #[test]
    fn unknown_value_is_cut_and_escaped() {
        let e = Mode::from_db("x\ny").unwrap_err();
        assert_eq!(
            e.to_string(),
            "unknown value `x\\ny` for Mode; expected one of `a`, `bee`"
        );
        // 63 ASCII bytes then a 2-byte char: the cut lands before it.
        let long = format!("{}é{}", "a".repeat(63), "z".repeat(10));
        let e = Mode::from_db(&long).unwrap_err();
        assert_eq!(e.value, format!("{}…", "a".repeat(63)));
    }
}
