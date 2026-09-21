//! Typed column references — the compile-time-checked side of the
//! query API.
//!
//! `#[derive(Model)]` makes one zero-sized type per scalar field and
//! exposes it as a `pub const` on the struct, so you write `User::id`,
//! `User::name` and so on. Each one carries its own `Value` type, so
//! `User::id.eq("alice")` fails to compile instead of raising a
//! runtime `TypeMismatch`.

use std::marker::PhantomData;

use super::{Assignment, Filter, Model, Op, SqlValue, WhereExpr};

/// A typed reference to a single scalar column on `Self::Model`.
///
/// `#[derive(Model)]` writes the impls. You normally reach them
/// through the `User::<field>` consts, not by naming the trait.
pub trait Column: Copy + 'static {
    /// The model the column belongs to.
    type Model: Model;
    /// The Rust-side type of the column. Must be convertible into `SqlValue`.
    type Value: Into<SqlValue>;
    /// Rust-side field name.
    const NAME: &'static str;
    /// SQL-side column name.
    const COLUMN: &'static str;
    /// Dialect-neutral classification.
    const FIELD_TYPE: super::FieldType;

    /// `column = value`.
    fn eq<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Eq, value.into().into())
    }

    /// `column <> value`.
    fn ne<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Ne, value.into().into())
    }

    /// `column < value`.
    fn lt<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Lt, value.into().into())
    }

    /// `column <= value`.
    fn lte<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Lte, value.into().into())
    }

    /// `column > value`.
    fn gt<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Gt, value.into().into())
    }

    /// `column >= value`.
    fn gte<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Gte, value.into().into())
    }

    /// `column LIKE value` — case-sensitive.
    fn like<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Like, value.into().into())
    }

    /// `column NOT LIKE value` — case-sensitive.
    fn not_like<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::NotLike, value.into().into())
    }

    /// `column ILIKE value` — case-insensitive (Postgres).
    fn ilike<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::ILike, value.into().into())
    }

    /// `column NOT ILIKE value` — case-insensitive (Postgres).
    fn not_ilike<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::NotILike, value.into().into())
    }

    /// Django `__contains` — the value is a **literal** substring.
    /// `%` and `_` in it match themselves: they are escaped and the
    /// SQL carries `ESCAPE '!'` on every dialect. Use [`Column::like`]
    /// for a raw pattern you build yourself.
    fn contains(self, value: impl AsRef<str>) -> TypedFilter<Self::Model> {
        Self::escaped_like(self, "%", "%", value, false)
    }

    /// Django `__icontains` — case-insensitive literal substring
    /// match.
    fn icontains(self, value: impl AsRef<str>) -> TypedFilter<Self::Model> {
        Self::escaped_like(self, "%", "%", value, true)
    }

    /// Django `__startswith` — literal prefix match.
    fn startswith(self, value: impl AsRef<str>) -> TypedFilter<Self::Model> {
        Self::escaped_like(self, "", "%", value, false)
    }

    /// Django `__istartswith` — case-insensitive literal prefix
    /// match.
    fn istartswith(self, value: impl AsRef<str>) -> TypedFilter<Self::Model> {
        Self::escaped_like(self, "", "%", value, true)
    }

    /// Django `__endswith` — literal suffix match.
    fn endswith(self, value: impl AsRef<str>) -> TypedFilter<Self::Model> {
        Self::escaped_like(self, "%", "", value, false)
    }

    /// Django `__iendswith` — case-insensitive literal suffix match.
    fn iendswith(self, value: impl AsRef<str>) -> TypedFilter<Self::Model> {
        Self::escaped_like(self, "%", "", value, true)
    }

    /// Django `__iexact` — case-insensitive **equality**. The value
    /// is a literal, never a pattern: `%` and `_` match themselves.
    /// [`Column::ilike`] instead binds your pattern as written.
    fn iexact(self, value: impl AsRef<str>) -> TypedFilter<Self::Model> {
        Self::escaped_like(self, "", "", value, true)
    }

    /// Shared builder for the escaped-LIKE lookups above. It escapes
    /// the value with [`crate::core::escape_like`] and picks the
    /// matching `*Escaped` op, so the two can never drift apart.
    #[doc(hidden)]
    fn escaped_like(
        self,
        prefix: &str,
        suffix: &str,
        value: impl AsRef<str>,
        case_insensitive: bool,
    ) -> TypedFilter<Self::Model> {
        let escaped = super::query::escape_like(value.as_ref());
        let op = if case_insensitive {
            Op::ILikeEscaped
        } else {
            Op::LikeEscaped
        };
        TypedFilter::scalar(
            Self::COLUMN,
            op,
            SqlValue::String(format!("{prefix}{escaped}{suffix}")),
        )
    }

    /// `column REGEXP pattern` — POSIX regex match, Django `__regex`.
    /// The pattern is bound as a `String`. Emits PG `~`, MySQL
    /// `REGEXP`, SQLite `REGEXP`.
    ///
    /// On SQLite you must register a `regexp(pattern, value)`
    /// user-function yourself. sqlx-sqlite only provides one through
    /// its `regexp` cargo feature and the `.with_regexp()` builder.
    fn regex(self, pattern: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Regex, SqlValue::String(pattern.into()))
    }

    /// `NOT column REGEXP pattern`. Django `~Q(field__regex=...)`.
    /// Same SQLite caveat as [`Column::regex`].
    fn not_regex(self, pattern: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::NotRegex, SqlValue::String(pattern.into()))
    }

    /// Case-insensitive POSIX regex match, Django `__iregex`. Emits
    /// PG `~*`. MySQL and SQLite use
    /// `LOWER(col) REGEXP LOWER(pattern)`, so case folding does not
    /// depend on the collation.
    fn iregex(self, pattern: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::IRegex, SqlValue::String(pattern.into()))
    }

    /// Case-insensitive POSIX regex non-match. Django
    /// `~Q(field__iregex=...)`.
    fn not_iregex(self, pattern: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::NotIRegex,
            SqlValue::String(pattern.into()),
        )
    }

    /// `column % pattern` — pg_trgm trigram similarity, Django's
    /// `__trigram_similar`. Compares whole strings at
    /// `pg_trgm.similarity_threshold` (0.3 unless changed).
    /// **PG-only**: MySQL and SQLite reject it at compile time.
    /// Needs `CREATE EXTENSION pg_trgm` on the database.
    fn trigram_similar(self, pattern: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::TrigramSimilar,
            SqlValue::String(pattern.into()),
        )
    }

    /// `column %> pattern` — pg_trgm word similarity, Django's
    /// `__trigram_word_similar`. Matches when **any word** in
    /// `column` is similar to the pattern. **PG-only**, and needs the
    /// same `pg_trgm` extension as [`Column::trigram_similar`].
    fn trigram_word_similar(self, pattern: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::TrigramWordSimilar,
            SqlValue::String(pattern.into()),
        )
    }

    /// `to_tsvector(column) @@ plainto_tsquery(query)` — Postgres
    /// full-text search, Django's `__search`. Uses the database's
    /// default text-search config. **PG-only**: MySQL and SQLite
    /// reject it at compile time, because their FTS shapes
    /// (MATCH…AGAINST, FTS5 MATCH) need different table layouts.
    fn search(self, query: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::Search, SqlValue::String(query.into()))
    }

    /// `column @> value` — PG array containment, Django's
    /// `__contains` lookup on `ArrayField`. Returns rows whose array
    /// holds every element of `values`. **PG-only**: MySQL and SQLite
    /// have no array type and reject it at compile time.
    ///
    /// Only `I32`, `I64`, `String` and `Bool` elements can be bound.
    /// Any other element type panics at bind time.
    fn array_contains<V>(self, values: V) -> TypedFilter<Self::Model>
    where
        V: IntoIterator,
        V::Item: Into<SqlValue>,
    {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::ArrayContains,
            SqlValue::Array(values.into_iter().map(Into::into).collect()),
        )
    }

    /// `column <@ value` — PG array containment, inverted. Django's
    /// `__contained_by` lookup. **PG-only**.
    fn array_contained_by<V>(self, values: V) -> TypedFilter<Self::Model>
    where
        V: IntoIterator,
        V::Item: Into<SqlValue>,
    {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::ArrayContainedBy,
            SqlValue::Array(values.into_iter().map(Into::into).collect()),
        )
    }

    /// `column && value` — PG array overlap. Django's `__overlap`
    /// lookup. Returns rows whose array shares at least one element
    /// with `values`. **PG-only**.
    fn array_overlap<V>(self, values: V) -> TypedFilter<Self::Model>
    where
        V: IntoIterator,
        V::Item: Into<SqlValue>,
    {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::ArrayOverlap,
            SqlValue::Array(values.into_iter().map(Into::into).collect()),
        )
    }

    /// `column @> range_literal` — the range column contains the
    /// given range. Django's `__range_contains`. `literal` is a PG
    /// range literal such as `"[1, 10)"` or
    /// `"[2025-01-01, 2025-02-01)"`; PG casts it to the column's
    /// range type. **PG-only**.
    fn range_contains(self, literal: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::RangeContains,
            SqlValue::RangeLiteral(literal.into()),
        )
    }

    /// `column <@ range_literal` — the range column is contained by
    /// the given range. Django's `__range_contained_by`.
    /// **PG-only**.
    fn range_contained_by(self, literal: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::RangeContainedBy,
            SqlValue::RangeLiteral(literal.into()),
        )
    }

    /// `column && range_literal` — PG range overlap. Django's
    /// `__range_overlap` lookup. **PG-only**.
    fn range_overlap(self, literal: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::RangeOverlap,
            SqlValue::RangeLiteral(literal.into()),
        )
    }

    /// `column << range_literal` — PG strictly-left-of. **PG-only**.
    fn range_strictly_left(self, literal: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::RangeStrictlyLeft,
            SqlValue::RangeLiteral(literal.into()),
        )
    }

    /// `column >> range_literal` — PG strictly-right-of. **PG-only**.
    fn range_strictly_right(self, literal: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::RangeStrictlyRight,
            SqlValue::RangeLiteral(literal.into()),
        )
    }

    /// `column -|- range_literal` — PG range adjacency. **PG-only**.
    fn range_adjacent(self, literal: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::RangeAdjacent,
            SqlValue::RangeLiteral(literal.into()),
        )
    }

    /// `column IS NULL`.
    fn is_null(self) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::IsNull, SqlValue::Bool(true))
    }

    /// `column IS NOT NULL`.
    fn is_not_null(self) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::IsNull, SqlValue::Bool(false))
    }

    /// `column IN (v1, v2, …)`. An empty iterator is allowed here;
    /// the SQL writer rejects it at compile time.
    fn is_in<V, I>(self, values: I) -> TypedFilter<Self::Model>
    where
        V: Into<Self::Value>,
        I: IntoIterator<Item = V>,
    {
        let list: Vec<SqlValue> = values.into_iter().map(|v| v.into().into()).collect();
        TypedFilter::scalar(Self::COLUMN, Op::In, SqlValue::List(list))
    }

    /// `column NOT IN (v1, v2, …)`.
    fn not_in<V, I>(self, values: I) -> TypedFilter<Self::Model>
    where
        V: Into<Self::Value>,
        I: IntoIterator<Item = V>,
    {
        let list: Vec<SqlValue> = values.into_iter().map(|v| v.into().into()).collect();
        TypedFilter::scalar(Self::COLUMN, Op::NotIn, SqlValue::List(list))
    }

    /// `column BETWEEN lo AND hi`. Both bounds are inclusive.
    fn between<V: Into<Self::Value>>(self, lo: V, hi: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(
            Self::COLUMN,
            Op::Between,
            SqlValue::List(vec![lo.into().into(), hi.into().into()]),
        )
    }

    /// `column IS DISTINCT FROM value` — null-safe inequality.
    fn is_distinct_from<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::IsDistinctFrom, value.into().into())
    }

    /// `column IS NOT DISTINCT FROM value` — null-safe equality.
    fn is_not_distinct_from<V: Into<Self::Value>>(self, value: V) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::IsNotDistinctFrom, value.into().into())
    }

    /// JSONB `@>` — column contains the given JSON value.
    /// Bind a `serde_json::Value`.
    fn json_contains(self, value: serde_json::Value) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::JsonContains, SqlValue::Json(value))
    }

    /// JSONB `<@` — column is contained by the given JSON value.
    fn json_contained_by(self, value: serde_json::Value) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::JsonContainedBy, SqlValue::Json(value))
    }

    /// JSONB `?` — the key exists as a top-level key in the column.
    fn json_has_key(self, key: impl Into<String>) -> TypedFilter<Self::Model> {
        TypedFilter::scalar(Self::COLUMN, Op::JsonHasKey, SqlValue::String(key.into()))
    }

    /// JSONB `?|` — any of the given keys exist as top-level keys.
    fn json_has_any_key<I: IntoIterator<Item = impl Into<String>>>(
        self,
        keys: I,
    ) -> TypedFilter<Self::Model> {
        let list: Vec<SqlValue> = keys
            .into_iter()
            .map(|k| SqlValue::String(k.into()))
            .collect();
        TypedFilter::scalar(Self::COLUMN, Op::JsonHasAnyKey, SqlValue::List(list))
    }

    /// JSONB `?&` — all of the given keys exist as top-level keys.
    fn json_has_all_keys<I: IntoIterator<Item = impl Into<String>>>(
        self,
        keys: I,
    ) -> TypedFilter<Self::Model> {
        let list: Vec<SqlValue> = keys
            .into_iter()
            .map(|k| SqlValue::String(k.into()))
            .collect();
        TypedFilter::scalar(Self::COLUMN, Op::JsonHasAllKeys, SqlValue::List(list))
    }

    /// `SET column = value` for an UPDATE.
    fn set<V: Into<Self::Value>>(self, value: V) -> TypedAssignment<Self::Model> {
        // value: V → Self::Value → SqlValue → Expr; one `From` impl
        // per step.
        let sql: SqlValue = value.into().into();
        TypedAssignment {
            inner: Assignment {
                column: Self::COLUMN,
                value: sql.into(),
            },
            _model: PhantomData,
        }
    }

    /// `SET column = <expression>` for an UPDATE. Takes the full
    /// [`Expr`](crate::core::Expr) form, for `F()` column references
    /// and arithmetic. Use [`Column::set`] for a plain value.
    fn set_expr(self, expr: impl Into<crate::core::Expr>) -> TypedAssignment<Self::Model> {
        TypedAssignment {
            inner: Assignment {
                column: Self::COLUMN,
                value: expr.into(),
            },
            _model: PhantomData,
        }
    }

    // ----- Column-vs-expression predicates (Django `F()` rhs) -----

    /// `column = <expr>` — Django's `filter(col=F("other"))` shape.
    /// Takes a bare [`F`](crate::core::F), a full
    /// [`Expr`](crate::core::Expr) such as `F("a") + 1`, or anything
    /// with `Into<Expr>`. Returns a [`TypedExpr`], so `.and()` and
    /// `.or()` chain as usual.
    fn eq_expr(self, rhs: impl Into<crate::core::Expr>) -> TypedExpr<Self::Model> {
        TypedExpr::column_cmp(Self::COLUMN, Op::Eq, rhs.into())
    }

    /// `column <> <expr>`.
    fn ne_expr(self, rhs: impl Into<crate::core::Expr>) -> TypedExpr<Self::Model> {
        TypedExpr::column_cmp(Self::COLUMN, Op::Ne, rhs.into())
    }

    /// `column < <expr>` — the usual column-vs-column compare, e.g.
    /// `start_date.lt_expr(F("end_date"))`.
    fn lt_expr(self, rhs: impl Into<crate::core::Expr>) -> TypedExpr<Self::Model> {
        TypedExpr::column_cmp(Self::COLUMN, Op::Lt, rhs.into())
    }

    /// `column <= <expr>`.
    fn lte_expr(self, rhs: impl Into<crate::core::Expr>) -> TypedExpr<Self::Model> {
        TypedExpr::column_cmp(Self::COLUMN, Op::Lte, rhs.into())
    }

    /// `column > <expr>`.
    fn gt_expr(self, rhs: impl Into<crate::core::Expr>) -> TypedExpr<Self::Model> {
        TypedExpr::column_cmp(Self::COLUMN, Op::Gt, rhs.into())
    }

    /// `column >= <expr>`.
    fn gte_expr(self, rhs: impl Into<crate::core::Expr>) -> TypedExpr<Self::Model> {
        TypedExpr::column_cmp(Self::COLUMN, Op::Gte, rhs.into())
    }
}

/// A `Filter` tagged with the model it applies to.
///
/// Built by the [`Column`] methods. `QuerySet<M>::where_` takes only
/// `TypedFilter<M>`, so a filter for one model cannot reach another
/// model's queryset.
pub struct TypedFilter<M: Model> {
    pub(crate) inner: Filter,
    _model: PhantomData<fn() -> M>,
}

impl<M: Model> TypedFilter<M> {
    fn scalar(column: &'static str, op: Op, value: SqlValue) -> Self {
        Self {
            inner: Filter { column, op, value },
            _model: PhantomData,
        }
    }

    /// Unwrap to the dialect-neutral filter form.
    #[must_use]
    pub fn into_filter(self) -> Filter {
        self.inner
    }

    /// Join with another predicate using SQL `AND`. Returns a
    /// [`TypedExpr`], so `.and()` / `.or()` can chain on the result.
    #[must_use]
    pub fn and<E: Into<TypedExpr<M>>>(self, rhs: E) -> TypedExpr<M> {
        TypedExpr::from(self).and(rhs)
    }

    /// Join with another predicate using SQL `OR`. Returns a
    /// [`TypedExpr`], so `.and()` / `.or()` can chain on the result.
    #[must_use]
    pub fn or<E: Into<TypedExpr<M>>>(self, rhs: E) -> TypedExpr<M> {
        TypedExpr::from(self).or(rhs)
    }

    /// Join with another predicate using SQL `XOR` — Django 4.1+
    /// `Q(a) ^ Q(b)`. Returns a [`TypedExpr`], so `.and()` / `.or()`
    /// / `.xor()` can chain on the result.
    #[must_use]
    pub fn xor<E: Into<TypedExpr<M>>>(self, rhs: E) -> TypedExpr<M> {
        TypedExpr::from(self).xor(rhs)
    }

    /// Negate this predicate — emits `NOT (col op val)`.
    #[must_use]
    pub fn not(self) -> TypedExpr<M> {
        TypedExpr::from(self).not()
    }
}

impl<M: Model> From<TypedFilter<M>> for TypedExpr<M> {
    fn from(tf: TypedFilter<M>) -> Self {
        Self {
            inner: WhereExpr::Predicate(tf.inner),
            _model: PhantomData,
        }
    }
}

// --- Untyped lowering for `case().when(...)` and similar consumers.
//     It drops the model tag, so the caller must use the right
//     model's columns; the queryset still checks them at `compile()`
//     time.

impl<M: Model> From<TypedFilter<M>> for WhereExpr {
    fn from(tf: TypedFilter<M>) -> Self {
        Self::Predicate(tf.inner)
    }
}

impl<M: Model> From<TypedExpr<M>> for WhereExpr {
    fn from(te: TypedExpr<M>) -> Self {
        te.inner
    }
}

/// Typed boolean expression: a tree of [`TypedFilter`] joined with
/// `.and()` / `.or()`. Any `TypedFilter` converts into one through
/// `Into`, so you rarely name this type:
///
/// ```ignore
/// User::objects()
///     .where_(User::name.eq("alice").or(User::name.eq("bob")))
///     .where_(User::active.eq(true))
///     .fetch(&pool).await?;
/// // → WHERE ("name" = $1 OR "name" = $2) AND "active" = $3
/// ```
///
/// Each `.where_(…)` call AND-joins its expression to the WHERE
/// clause built so far. An `OR` must sit inside one `.where_()` call.
pub struct TypedExpr<M: Model> {
    pub(crate) inner: WhereExpr,
    _model: PhantomData<fn() -> M>,
}

impl<M: Model> TypedExpr<M> {
    /// Tag a dialect-neutral [`WhereExpr`] as an expression for `M`.
    /// Used by the [`crate::query::Q`] runtime builder and any other
    /// untyped source that needs to reach `.where_()`. Field names
    /// are still checked at `compile()` time, so a typo errors there.
    #[must_use]
    pub fn from_where_expr(inner: WhereExpr) -> Self {
        Self {
            inner,
            _model: PhantomData,
        }
    }

    /// Build a [`WhereExpr::ColumnCompare`] leaf — the `F()`-style
    /// "column <op> expr" predicate behind [`Column::eq_expr`] and
    /// friends.
    #[must_use]
    pub(crate) fn column_cmp(column: &'static str, op: Op, rhs: crate::core::Expr) -> Self {
        Self {
            inner: WhereExpr::ColumnCompare(super::query::ColumnFilter { column, op, rhs }),
            _model: PhantomData,
        }
    }

    /// Unwrap to the dialect-neutral expression form.
    #[must_use]
    pub fn into_expr(self) -> WhereExpr {
        self.inner
    }

    /// Join with `AND`. Adjacent `And` nodes are flattened, so the
    /// tree stays shallow: `a.and(b).and(c)` gives
    /// `And(vec![a, b, c])`, not `And(And(a, b), c)`.
    #[must_use]
    pub fn and<E: Into<Self>>(self, rhs: E) -> Self {
        let rhs = rhs.into();
        let inner = match (self.inner, rhs.inner) {
            (WhereExpr::And(mut a), WhereExpr::And(b)) => {
                a.extend(b);
                WhereExpr::And(a)
            }
            (WhereExpr::And(mut a), b) => {
                a.push(b);
                WhereExpr::And(a)
            }
            (a, WhereExpr::And(mut b)) => {
                b.insert(0, a);
                WhereExpr::And(b)
            }
            (a, b) => WhereExpr::And(vec![a, b]),
        };
        Self {
            inner,
            _model: PhantomData,
        }
    }

    /// Join with `OR`. Adjacent `Or` nodes are flattened, so the tree
    /// stays shallow.
    #[must_use]
    pub fn or<E: Into<Self>>(self, rhs: E) -> Self {
        let rhs = rhs.into();
        let inner = match (self.inner, rhs.inner) {
            (WhereExpr::Or(mut a), WhereExpr::Or(b)) => {
                a.extend(b);
                WhereExpr::Or(a)
            }
            (WhereExpr::Or(mut a), b) => {
                a.push(b);
                WhereExpr::Or(a)
            }
            (a, WhereExpr::Or(mut b)) => {
                b.insert(0, a);
                WhereExpr::Or(b)
            }
            (a, b) => WhereExpr::Or(vec![a, b]),
        };
        Self {
            inner,
            _model: PhantomData,
        }
    }

    /// Join with `XOR` — Django 4.1+ `Q(a) ^ Q(b)`. Matches a row
    /// when an odd number of operands are true. Adjacent `Xor` nodes
    /// flatten like `.and()` / `.or()`, so `a.xor(b).xor(c)` keeps
    /// that odd-parity meaning instead of nesting `(a^b)^c`.
    #[must_use]
    pub fn xor<E: Into<Self>>(self, rhs: E) -> Self {
        let rhs = rhs.into();
        let inner = match (self.inner, rhs.inner) {
            (WhereExpr::Xor(mut a), WhereExpr::Xor(b)) => {
                a.extend(b);
                WhereExpr::Xor(a)
            }
            (WhereExpr::Xor(mut a), b) => {
                a.push(b);
                WhereExpr::Xor(a)
            }
            (a, WhereExpr::Xor(mut b)) => {
                b.insert(0, a);
                WhereExpr::Xor(b)
            }
            (a, b) => WhereExpr::Xor(vec![a, b]),
        };
        Self {
            inner,
            _model: PhantomData,
        }
    }

    /// Negate this expression — emits `NOT (…)`.
    #[must_use]
    pub fn not(self) -> Self {
        Self {
            inner: WhereExpr::Not(Box::new(self.inner)),
            _model: PhantomData,
        }
    }
}

/// An `Assignment` tagged with the model it applies to.
pub struct TypedAssignment<M: Model> {
    pub(crate) inner: Assignment,
    _model: PhantomData<fn() -> M>,
}

impl<M: Model> TypedAssignment<M> {
    /// Unwrap to the dialect-neutral assignment form.
    #[must_use]
    pub fn into_assignment(self) -> Assignment {
        self.inner
    }
}

/// A mixed list of typed [`Column`] references for one model. Drives
/// `Model::save_partial_typed(...)`.
///
/// Each field is its own zero-sized type, so `&[Post::title,
/// Post::slug]` does not type-check: the element types differ. A
/// tuple does, because every slot keeps its own type, and the
/// `Column<Model = M>` bound on each slot checks at compile time that
/// they all belong to the same model:
///
/// ```ignore
/// post.save_partial_typed((Post::title, Post::slug), &pool).await?;
/// //                       ──────────  ──────────
/// //                       title_col   slug_col   ← distinct ZSTs
/// //
/// // (Post::title, Author::name)  →  compile error: Author::name's
/// //                                  Model = Author, not Post.
/// ```
///
/// The trait is sealed: only the built-in tuple impls implement it.
pub trait TypedFieldList<M: Model>: sealed::Sealed {
    /// Rust-side field names, in tuple order. Passed to
    /// [`crate::core::ModelSchema::field`] to resolve SQL columns,
    /// the same way the string-keyed [`Model::save_partial`] path
    /// does.
    ///
    /// [`Model::save_partial`]: crate::core::Model
    fn rust_field_names(&self) -> Vec<&'static str>;
}

mod sealed {
    pub trait Sealed {}
}

/// Single-element tuple — supports the `(Post::title,)` one-field call.
impl<M: Model, A: Column<Model = M>> TypedFieldList<M> for (A,) {
    fn rust_field_names(&self) -> Vec<&'static str> {
        vec![A::NAME]
    }
}
impl<A> sealed::Sealed for (A,) {}

// Tuple impls up to 12 elements. For more fields than that, use
// `save_partial(&[&str], _)` instead.
macro_rules! impl_typed_field_list_tuple {
    ( $( ( $($T:ident),+ ) ),+ $(,)? ) => {
        $(
            impl<M: Model, $($T: Column<Model = M>),+> TypedFieldList<M> for ($($T,)+) {
                fn rust_field_names(&self) -> Vec<&'static str> {
                    vec![$($T::NAME),+]
                }
            }
            impl<$($T),+> sealed::Sealed for ($($T,)+) {}
        )+
    };
}

impl_typed_field_list_tuple!(
    (A, B),
    (A, B, C),
    (A, B, C, D),
    (A, B, C, D, E),
    (A, B, C, D, E, F),
    (A, B, C, D, E, F, G),
    (A, B, C, D, E, F, G, H),
    (A, B, C, D, E, F, G, H, I),
    (A, B, C, D, E, F, G, H, I, J),
    (A, B, C, D, E, F, G, H, I, J, K),
    (A, B, C, D, E, F, G, H, I, J, K, L),
);
