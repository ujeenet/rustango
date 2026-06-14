//! Postgres full-text search builder — closes #295 / T2.4.
//!
//! Composable wrapper over the `to_tsvector` / `*_tsquery` / `ts_rank` /
//! `ts_headline` scalar functions added in #266 / T1.4. Replaces the
//! hand-rolled `Expr::Function { kind: ScalarFn::ToTsVector, … }`
//! incantation with a discoverable builder shape that matches Django's
//! `django.contrib.postgres.search` API.
//!
//! **Postgres-only by language semantics.** MySQL's `MATCH … AGAINST`
//! requires a `FULLTEXT INDEX` on the column and only matches against
//! the indexed columns; SQLite's FTS5 virtual tables require a
//! separate `CREATE VIRTUAL TABLE` and don't compose against bare
//! columns. Neither shape maps onto this builder's "build a tsvector
//! from arbitrary expressions, match it against a tsquery" semantics,
//! so the writer emits [`crate::sql::SqlError::OpNotSupportedInDialect`]
//! on non-PG.
//!
//! ## Usage
//!
//! ```ignore
//! use rustango::core::F;
//! use rustango::core::fts::{SearchQuery, SearchRank, SearchVector, Weight};
//!
//! // Weighted multi-column vector — `title` matches outweigh `body`.
//! let v = SearchVector::weighted(&[(F("title"), Weight::A), (F("body"), Weight::B)]);
//! let q = SearchQuery::plain("alice");
//!
//! Post::objects()
//!     .annotate("rank", SearchRank::new(&v, &q))
//!     .where_raw(v.matches(&q))
//!     .order_by_expr(SearchRank::new(&v, &q), true) // rank DESC
//!     .fetch(&pool).await?;
//! ```
//!
//! ## API surface
//!
//! - [`Weight`] — A / B / C / D weighting class (Django parity).
//! - [`SearchVector::single`] / [`SearchVector::weighted`] — tsvector LHS.
//! - [`SearchQuery::plain`] / [`::phrase`] / [`::websearch`] / [`::raw`] —
//!   tsquery RHS, mapping onto the four PG parsers
//!   (`plainto_tsquery`, `phraseto_tsquery`, `websearch_to_tsquery`,
//!   `to_tsquery`).
//! - [`SearchVector::matches`] — composes a
//!   [`WhereExpr`](crate::core::WhereExpr) for `.where_raw(...)`.
//! - [`SearchRank::new`] / [`SearchHeadline::new`] — `Expr`-returning
//!   annotations.

use super::expr::{Expr, ScalarFn};
use super::query::{Op, WhereExpr};
use super::value::SqlValue;

/// Postgres tsvector weight class — Django parity.
///
/// Weights bias `ts_rank` so matches in higher-weighted positions
/// outrank lower-weighted ones. Customary use is A for title, B for
/// summary, C for body, D for tags / metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// Highest priority. `setweight(..., 'A')`.
    A,
    /// `setweight(..., 'B')`.
    B,
    /// `setweight(..., 'C')`.
    C,
    /// Lowest priority. `setweight(..., 'D')`.
    D,
}

impl Weight {
    fn as_literal(self) -> Expr {
        let s = match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
        };
        Expr::Literal(SqlValue::String(s.to_owned()))
    }
}

/// A Postgres `tsvector` — the searchable side of a `@@` match.
///
/// Built from one or more text expressions. Construct via
/// [`SearchVector::single`] (single column / expression) or
/// [`SearchVector::weighted`] (multi-column with per-column weighting).
#[derive(Debug, Clone)]
pub struct SearchVector {
    /// The compiled tsvector expression. For `single` this is
    /// `to_tsvector(<col>)`; for `weighted` it's `(setweight(...) ||
    /// setweight(...) || …)`.
    expr: Expr,
}

impl SearchVector {
    /// `to_tsvector(<expr>)` — single-column vector. The expression is
    /// typically a column reference via `F("title")`, but any text-
    /// returning expression works.
    #[must_use]
    pub fn single(text: impl Into<Expr>) -> Self {
        Self {
            expr: Expr::Function {
                kind: ScalarFn::ToTsVector,
                args: vec![text.into()],
            },
        }
    }

    /// `setweight(to_tsvector(c1), 'A') || setweight(to_tsvector(c2),
    /// 'B') || …` — weighted multi-column vector. Each `(expr, weight)`
    /// pair becomes one `setweight(to_tsvector(...))` clause; the
    /// emitter concatenates them with the PG `||` operator.
    ///
    /// Returns a vector that matches Django's
    /// `SearchVector('title', weight='A') + SearchVector('body',
    /// weight='B')` shape. Empty input is rejected at write time with
    /// a clear error.
    #[must_use]
    pub fn weighted<I, E>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (E, Weight)>,
        E: Into<Expr>,
    {
        let weighted: Vec<Expr> = pairs
            .into_iter()
            .map(|(text, w)| Expr::Function {
                kind: ScalarFn::SetWeight,
                args: vec![
                    Expr::Function {
                        kind: ScalarFn::ToTsVector,
                        args: vec![text.into()],
                    },
                    w.as_literal(),
                ],
            })
            .collect();
        // Single-column weighted case: emit one setweight, no TsConcat
        // wrapper (the wrapper's arity gate would reject a 1-arg input).
        let expr = if weighted.len() == 1 {
            // Take the only element out of the Vec by index.
            weighted.into_iter().next().expect("weighted.len() == 1")
        } else {
            Expr::Function {
                kind: ScalarFn::TsConcat,
                args: weighted,
            }
        };
        Self { expr }
    }

    /// Borrow the underlying [`Expr`]. Useful for compositional callers
    /// that want to inject the vector into a larger expression tree.
    #[must_use]
    pub fn as_expr(&self) -> &Expr {
        &self.expr
    }

    /// Produce a [`WhereExpr`] that matches this vector against the
    /// supplied [`SearchQuery`]: `<vector> @@ <query>`.
    ///
    /// Chain into `.where_raw(...)` on a [`QuerySet`](crate::query::QuerySet).
    #[must_use]
    pub fn matches(&self, query: &SearchQuery) -> WhereExpr {
        WhereExpr::ExprCompare {
            lhs: self.expr.clone(),
            op: Op::Search,
            rhs: query.as_expr().clone(),
        }
    }
}

/// A Postgres `tsquery` — the search-term side of a `@@` match.
///
/// Built from a free-form query string via one of the four PG query
/// parsers. Each variant maps onto a different syntax flavor:
///
/// - [`SearchQuery::plain`] — `plainto_tsquery`: user-facing keyword
///   list, AND semantics, no operator characters.
/// - [`SearchQuery::phrase`] — `phraseto_tsquery`: word-order
///   preserving (`'rust orm'` → `'rust' <-> 'orm'`).
/// - [`SearchQuery::websearch`] — `websearch_to_tsquery`: Google-style
///   syntax (quoted "exact phrase", `-exclude`, the literal `OR`).
/// - [`SearchQuery::raw`] — `to_tsquery`: low-level pre-parsed syntax
///   (`'rust & orm'`, `'rust | python'`, `'rust & !python'`). Caller
///   is responsible for valid tsquery syntax.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    expr: Expr,
}

impl SearchQuery {
    /// `plainto_tsquery(<text>)`.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self::from_parser(ScalarFn::PlainToTsQuery, text)
    }

    /// `phraseto_tsquery(<text>)`. Preserves word order.
    #[must_use]
    pub fn phrase(text: impl Into<String>) -> Self {
        Self::from_parser(ScalarFn::PhraseToTsQuery, text)
    }

    /// `websearch_to_tsquery(<text>)`. Google-style operators.
    #[must_use]
    pub fn websearch(text: impl Into<String>) -> Self {
        Self::from_parser(ScalarFn::WebsearchToTsQuery, text)
    }

    /// `to_tsquery(<text>)` — pre-parsed `tsquery` syntax. Caller must
    /// produce a valid query (`'rust & orm'`, `'rust | python'`,
    /// `'rust & !python'`).
    #[must_use]
    pub fn raw(text: impl Into<String>) -> Self {
        Self::from_parser(ScalarFn::ToTsQuery, text)
    }

    fn from_parser(kind: ScalarFn, text: impl Into<String>) -> Self {
        Self {
            expr: Expr::Function {
                kind,
                args: vec![Expr::Literal(SqlValue::String(text.into()))],
            },
        }
    }

    /// Borrow the underlying [`Expr`].
    #[must_use]
    pub fn as_expr(&self) -> &Expr {
        &self.expr
    }
}

/// `ts_rank(<vector>, <query>)` — relevance score (`real` between
/// 0.0 and 1.0). Annotate via `.annotate("rank", SearchRank::new(...))`
/// and sort by it via `.order_by_expr(..., true)` for "best first".
#[derive(Debug, Clone)]
pub struct SearchRank;

impl SearchRank {
    /// Build the `ts_rank(<vector>, <query>)` [`Expr`].
    #[must_use]
    pub fn new(vector: &SearchVector, query: &SearchQuery) -> Expr {
        Expr::Function {
            kind: ScalarFn::TsRank,
            args: vec![vector.expr.clone(), query.expr.clone()],
        }
    }

    /// Build `ts_rank_cd(<vector>, <query>)` — cover-density variant,
    /// better for short documents.
    #[must_use]
    pub fn cover_density(vector: &SearchVector, query: &SearchQuery) -> Expr {
        Expr::Function {
            kind: ScalarFn::TsRankCd,
            args: vec![vector.expr.clone(), query.expr.clone()],
        }
    }
}

/// `ts_headline(<doc>, <query> [, <options>])` — Postgres FTS snippet
/// generator. Returns the document with matching terms wrapped in
/// highlight markers (`<b>…</b>` by default).
///
/// `doc` is the text to highlight (typically `F("body")`); `query` is
/// the same `SearchQuery` you passed to `SearchRank` / `matches`.
#[derive(Debug, Clone)]
pub struct SearchHeadline;

impl SearchHeadline {
    /// `ts_headline(<doc>, <query>)` — default highlight markers.
    #[must_use]
    pub fn new(doc: impl Into<Expr>, query: &SearchQuery) -> Expr {
        Expr::Function {
            kind: ScalarFn::TsHeadline,
            args: vec![doc.into(), query.expr.clone()],
        }
    }

    /// `ts_headline(<doc>, <query>, <options>)` — override the highlight
    /// markers and fragment count via PG's options-string syntax, e.g.
    /// `"StartSel='<mark>', StopSel='</mark>', MaxFragments=1"`.
    #[must_use]
    pub fn with_options(
        doc: impl Into<Expr>,
        query: &SearchQuery,
        options: impl Into<String>,
    ) -> Expr {
        Expr::Function {
            kind: ScalarFn::TsHeadline,
            args: vec![
                doc.into(),
                query.expr.clone(),
                Expr::Literal(SqlValue::String(options.into())),
            ],
        }
    }
}
