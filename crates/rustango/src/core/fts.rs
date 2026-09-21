//! Postgres full-text search builder.
//!
//! Wraps the `to_tsvector`, `*_tsquery`, `ts_rank` and `ts_headline`
//! functions in a typed builder, so a search vector, a query and a
//! rank compose instead of being spelled out as raw SQL.
//!
//! **Postgres only.** MySQL needs a `FULLTEXT INDEX` and SQLite needs
//! an FTS5 virtual table, so neither can build a vector from arbitrary
//! expressions. On those backends the writer returns
//! [`crate::sql::SqlError::OpNotSupportedInDialect`].
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
//! ## What is here
//!
//! - [`Weight`] — the A / B / C / D weight classes.
//! - [`SearchVector`] — the tsvector side of the match.
//! - [`SearchQuery`] — the tsquery side, one constructor per PG parser.
//! - [`SearchVector::matches`] — builds a
//!   [`WhereExpr`](crate::core::WhereExpr) for `.where_raw(...)`.
//! - [`SearchRank`] / [`SearchHeadline`] — ranking and snippets.

use super::expr::{Expr, ScalarFn};
use super::query::{Op, WhereExpr};
use super::value::SqlValue;

/// Weight class for one part of a tsvector.
///
/// `ts_rank` scores a match in a higher class above one in a lower
/// class. A is usually the title, B the summary, C the body, D tags.
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
/// Build it with [`SearchVector::single`] for one expression, or
/// [`SearchVector::weighted`] to weight several.
#[derive(Debug, Clone)]
pub struct SearchVector {
    /// The built tsvector: `to_tsvector(<col>)`, or a chain of
    /// `setweight(…) || setweight(…)` when weighted.
    expr: Expr,
}

impl SearchVector {
    /// `to_tsvector(<expr>)`. Pass a column with `F("title")`, or any
    /// expression that returns text.
    #[must_use]
    pub fn single(text: impl Into<Expr>) -> Self {
        Self {
            expr: Expr::Function {
                kind: ScalarFn::ToTsVector,
                args: vec![text.into()],
            },
        }
    }

    /// A vector over several expressions, each with its own weight.
    /// Every `(expr, weight)` pair becomes one
    /// `setweight(to_tsvector(...))`, joined with the PG `||` operator.
    /// Empty input is rejected when the SQL is written.
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
        // One pair: emit the bare setweight. The TsConcat wrapper
        // needs at least two arguments.
        let expr = if weighted.len() == 1 {
            weighted.into_iter().next().expect("weighted.len() == 1")
        } else {
            Expr::Function {
                kind: ScalarFn::TsConcat,
                args: weighted,
            }
        };
        Self { expr }
    }

    /// Borrow the underlying [`Expr`] to nest it in a larger tree.
    #[must_use]
    pub fn as_expr(&self) -> &Expr {
        &self.expr
    }

    /// Build `<vector> @@ <query>` as a [`WhereExpr`]. Pass it to
    /// `.where_raw(...)` on a [`QuerySet`](crate::query::QuerySet).
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
/// Pick the constructor that matches the syntax of your input:
///
/// - [`SearchQuery::plain`] — plain keywords, joined with AND.
/// - [`SearchQuery::phrase`] — keeps the word order.
/// - [`SearchQuery::websearch`] — Google style: `"exact phrase"`,
///   `-exclude`, `OR`.
/// - [`SearchQuery::raw`] — real tsquery syntax such as
///   `'rust & !python'`. You must pass something valid.
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

    /// `to_tsquery(<text>)`. You must pass valid tsquery syntax, such
    /// as `'rust & orm'`, `'rust | python'` or `'rust & !python'`.
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

/// `ts_rank(<vector>, <query>)` — a relevance score between 0.0 and
/// 1.0. Sort by it with `.order_by_expr(..., true)` for best first.
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

    /// Build `ts_rank_cd(<vector>, <query>)`, the cover-density
    /// variant. Better for short documents.
    #[must_use]
    pub fn cover_density(vector: &SearchVector, query: &SearchQuery) -> Expr {
        Expr::Function {
            kind: ScalarFn::TsRankCd,
            args: vec![vector.expr.clone(), query.expr.clone()],
        }
    }
}

/// `ts_headline(<doc>, <query> [, <options>])` — a snippet of `doc`
/// with the matched terms wrapped in markers (`<b>…</b>` by default).
/// `doc` is the text to highlight, often `F("body")`. Pass the same
/// `SearchQuery` you used for the match.
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

    /// `ts_headline(<doc>, <query>, <options>)`. The options string
    /// sets the markers and the fragment count, e.g.
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
