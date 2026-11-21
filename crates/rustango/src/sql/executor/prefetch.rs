//! Tri-dialect `prefetch_related` helpers — `fetch_with_prefetch_pool`
//! and `fetch_with_prefetch_filtered` (issue #298 / T2.1).
//!
//! Extracted from `executor/mod.rs` as part of #116 step 6. The PG-only
//! `_on` variants (`annotate_count_children`, `annotate_count_children_on`,
//! `fetch_with_prefetch`) plus the `HasPkValue` trait + `extract_pk_value`
//! helper stay in `mod.rs` — they're tightly coupled to PG's `PgRow` /
//! `PgPool` types and the schema-mode tenancy connection path that lives
//! in mod.rs proper.

use super::{
    extract_pk_value, DecodeModel, ExecError, FkPkAccess, HasPkValue, ReusableExecutor,
    SqlExecutor as _,
};
use crate::core::Model;
use crate::sql::Pool;
// Each statement's future is boxed: an `E::Exec` held across an `.await`
// makes the caller's future not `Send` (a higher-ranked rustc limit).
use futures_util::FutureExt as _;

/// `fetch_with_prefetch` against either backend. Bi-dialect counterpart
/// of `fetch_with_prefetch` — fetches parents + children in two
/// round trips and stitches each child onto its parent via
/// [`FkPkAccess`]. Every `#[derive(Model)]` type meets the
/// [`DecodeModel`] bound automatically.
///
/// `FkPkAccess` doesn't need a MySQL counterpart — it reads i64
/// values from struct fields after decoding, with no Row dependency,
/// so the same trait works against either backend.
///
/// # Errors
/// As the PG `fetch_with_prefetch` family.
pub async fn fetch_with_prefetch_pool<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model + DecodeModel + HasPkValue,
    C: Model + DecodeModel + FkPkAccess,
{
    fetch_with_prefetch_on(parent_qs, child_fk_column, pool).await
}

/// [`fetch_with_prefetch_pool`] on a [`ReusableExecutor`]: an open
/// `&mut PoolTx` or a held `&mut PoolConn` runs every statement.
///
/// # Errors
/// As [`fetch_with_prefetch_pool`].
pub async fn fetch_with_prefetch_on<P, C, E>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    exec: E,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    E: ReusableExecutor,
    P: Model + DecodeModel + HasPkValue,
    C: Model + DecodeModel + FkPkAccess,
{
    filtered_on(
        parent_qs,
        child_fk_column,
        crate::query::QuerySet::<C>::new(),
        exec,
    )
    .await
}

/// `fetch_with_prefetch_pool` with a user-supplied **filtered** child
/// queryset. Issue #298 / T2.1.
///
/// Drop-in replacement for [`fetch_with_prefetch_pool`] when the
/// caller wants to **filter / order / limit** the child fetch:
///
/// ```ignore
/// use rustango::core::Column as _;
/// use rustango::query::QuerySet;
///
/// // Authors + their PUBLISHED posts, newest first.
/// let published_posts = QuerySet::<Post>::default()
///     .filter("published", true)
///     .order_by(&[("created", true)]);
///
/// let authors_with_posts: Vec<(Author, Vec<Post>)> =
///     fetch_with_prefetch_filtered(
///         Author::objects(),
///         "author",
///         published_posts,
///         &pool,
///     ).await?;
/// ```
///
/// # Caveat — global vs. per-parent `LIMIT`
///
/// `child_qs.limit(N)` applies **globally** across the joined fetch,
/// not per parent. A per-parent slice (LATERAL JOIN on PG,
/// `ROW_NUMBER() OVER (PARTITION BY ...)` elsewhere) is a follow-up.
///
/// # Errors
/// As [`fetch_with_prefetch_pool`]; [`ExecError::InListUnsplittable`] when the
/// parents exceed one `IN` list and `child_qs` has a limit or offset.
pub async fn fetch_with_prefetch_filtered<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    child_qs: crate::query::QuerySet<C>,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model + DecodeModel + HasPkValue,
    C: Model + DecodeModel + FkPkAccess,
{
    filtered_on(parent_qs, child_fk_column, child_qs, pool).await
}

/// [`fetch_with_prefetch_filtered`] on any [`ReusableExecutor`].
async fn filtered_on<P, C, E>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    child_qs: crate::query::QuerySet<C>,
    mut exec: E,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    E: ReusableExecutor,
    P: Model + DecodeModel + HasPkValue,
    C: Model + DecodeModel + FkPkAccess,
{
    let parents: Vec<P> = parent_qs.fetch_on(exec.again()).boxed().await?;
    let on = target_column::<P, C>(child_fk_column);
    // The IN list AND-composes with the caller's filters, order and limit.
    // Over the bind cap the keys go in batches; a limit or offset then refuses.
    let mut stitch = Stitch::new(parents, on)?;
    while let Some(keys) = stitch.next_keys() {
        // Keys are an FK target's unique values: no two fold together.
        let children = reverse_children(&mut exec, &child_qs, child_fk_column, keys)
            .await?
            .rows();
        stitch.push(
            children
                .into_iter()
                .filter_map(|c| Some((c.__rustango_fk_pk_value(child_fk_column)?, c)))
                .collect(),
        );
    }
    Ok(stitch.finish())
}

/// `child_qs` narrowed to rows whose `child_fk_column` is one of `keys`.
async fn reverse_children<C, E>(
    exec: &mut E,
    child_qs: &crate::query::QuerySet<C>,
    child_fk_column: &'static str,
    keys: Vec<crate::core::SqlValue>,
) -> Result<super::Chunked<C>, ExecError>
where
    E: ReusableExecutor,
    C: Model + DecodeModel,
{
    let batches = super::key_batches(exec.again().dialect(), child_qs, keys)?;
    let split = batches.len() > 1;
    let mut rows = Vec::new();
    for keys in batches {
        let narrowed = child_qs.clone().filter_op(
            child_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(keys),
        );
        rows.extend(narrowed.fetch_on(exec.again()).boxed().await?);
    }
    Ok(super::Chunked { rows, split })
}

/// Prefetch the relation `relation` of every parent: a many-to-many or a
/// `reverse_has` reverse FK, by name.
///
/// ```ignore
/// let posts: Vec<(Post, Vec<Tag>)> =
///     fetch_with_prefetch_related(Post::objects(), "tags", &pool).await?;
/// ```
///
/// One query for the parents and one per relation, its `IN` list chunked
/// under the backend's bind cap. A reverse FK keys on the relation's
/// `self_pk_column`; a many-to-many joins its through table, so a target
/// shared by several parents is a copy under each.
///
/// # Errors
/// As [`fetch_with_prefetch_filtered`];
/// [`QueryError::UnknownField`](crate::core::QueryError::UnknownField) for
/// an unknown name and [`ExecError::PrefetchTargetMismatch`] when the
/// relation does not point at `C`.
pub async fn fetch_with_prefetch_related<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    relation: &str,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model + DecodeModel + HasPkValue,
    C: Model + DecodeModel + HasPkValue,
{
    let child_qs = crate::query::QuerySet::<C>::new();
    fetch_with_prefetch_related_filtered(parent_qs, relation, child_qs, pool).await
}

/// [`fetch_with_prefetch_related`] with a filtered, ordered child queryset.
/// A limit on `child_qs` is global, not per parent.
///
/// # Errors
/// As [`fetch_with_prefetch_related`].
pub async fn fetch_with_prefetch_related_filtered<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    relation: &str,
    child_qs: crate::query::QuerySet<C>,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model + DecodeModel + HasPkValue,
    C: Model + DecodeModel + HasPkValue,
{
    related_filtered_on(parent_qs, relation, child_qs, pool).await
}

/// [`fetch_with_prefetch_related_filtered`] on any [`ReusableExecutor`].
async fn related_filtered_on<P, C, E>(
    parent_qs: crate::query::QuerySet<P>,
    relation: &str,
    child_qs: crate::query::QuerySet<C>,
    mut exec: E,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    E: ReusableExecutor,
    P: Model + DecodeModel + HasPkValue,
    C: Model + DecodeModel + HasPkValue,
{
    // Resolved first, so a bad name costs no query.
    let named = Named::resolve::<P, C>(relation)?;
    let parents: Vec<P> = parent_qs.fetch_on(exec.again()).boxed().await?;
    match named {
        Named::Reverse { field, column, on } => {
            let mut stitch = Stitch::new(parents, on)?;
            while let Some(keys) = stitch.next_keys() {
                let children = reverse_children(&mut exec, &child_qs, field, keys)
                    .await?
                    .distinct();
                stitch.push(
                    children
                        .into_iter()
                        .map(|c| Ok((child_key(&c, column)?, c)))
                        .collect::<Result<_, ExecError>>()?,
                );
            }
            Ok(stitch.finish())
        }
        Named::M2m(rel) => {
            let mut stitch = Stitch::new(parents, None)?;
            while let Some(keys) = stitch.next_keys() {
                stitch.push(m2m_children(&mut exec, rel, &child_qs, keys).await?);
            }
            Ok(stitch.finish())
        }
    }
}

/// A relation [`fetch_with_prefetch_related`] loads, resolved by name.
enum Named {
    /// Children whose `field` (SQL `column`) holds the parent's `on` value.
    Reverse {
        field: &'static str,
        column: &'static str,
        on: Option<&'static str>,
    },
    /// Targets linked through a junction keyed by the parent's pk.
    M2m(&'static crate::core::M2MRelation),
}

impl Named {
    /// Reverse FK first, then M2M: the order `where_has` resolves names in.
    fn resolve<P: Model, C: Model>(relation: &str) -> Result<Self, ExecError> {
        let mismatch = |expected: &'static str| ExecError::PrefetchTargetMismatch {
            model: P::SCHEMA.name,
            relation: relation.to_owned(),
            expected,
            got: C::SCHEMA.table,
        };
        if let Some(rel) = P::reverse_relations().iter().find(|r| r.name == relation) {
            if rel.child_schema.table != C::SCHEMA.table {
                return Err(mismatch(rel.child_schema.table));
            }
            let field = C::SCHEMA
                .field(rel.child_fk_column)
                .or_else(|| C::SCHEMA.field_by_column(rel.child_fk_column))
                .ok_or_else(|| {
                    ExecError::Query(crate::core::QueryError::UnknownField {
                        model: C::SCHEMA.name,
                        field: rel.child_fk_column.to_owned(),
                    })
                })?;
            let on =
                (Some(rel.self_pk_column) != P::SCHEMA.pk_column()).then_some(rel.self_pk_column);
            return Ok(Self::Reverse {
                field: field.name,
                column: field.column,
                on,
            });
        }
        if let Some(rel) = P::SCHEMA.m2m.iter().find(|m| m.name == relation) {
            if rel.to != C::SCHEMA.table {
                return Err(mismatch(rel.to));
            }
            return Ok(Self::M2m(rel));
        }
        Err(ExecError::Query(crate::core::QueryError::UnknownField {
            model: P::SCHEMA.name,
            field: relation.to_owned(),
        }))
    }
}

/// The junction's alias in the many-to-many prefetch join.
const M2M_ALIAS: &str = "__rustango_m2m";

/// `child_qs` inner-joined to `rel`'s junction on rows whose source is one
/// of `keys`, each target paired with that source: one row per link.
async fn m2m_children<C, E>(
    exec: &mut E,
    rel: &'static crate::core::M2MRelation,
    child_qs: &crate::query::QuerySet<C>,
    keys: Vec<crate::core::SqlValue>,
) -> Result<Vec<(crate::core::SqlValue, C)>, ExecError>
where
    E: ReusableExecutor,
    C: Model + DecodeModel,
{
    use crate::core::joins::aliased;
    use crate::core::{Filter, Join, JoinKind, Op, SqlValue, WhereExpr};
    let pk = C::SCHEMA.pk_column().ok_or(ExecError::MissingPrimaryKey {
        table: C::SCHEMA.table,
    })?;
    let junction = crate::sql::m2m::junction(rel.through, &[rel.src_col, rel.dst_col]);
    let src_ty = junction.field_by_column(rel.src_col).map(|f| f.ty);
    let base = child_qs.clone().compile()?;
    let linked = |keys: Option<Vec<SqlValue>>| {
        let mut on = vec![WhereExpr::ExprCompare {
            lhs: aliased(M2M_ALIAS, rel.dst_col),
            op: Op::Eq,
            rhs: aliased(C::SCHEMA.table, pk),
        }];
        if let Some(keys) = keys {
            on.push(WhereExpr::Predicate(Filter {
                column: rel.src_col,
                op: Op::In,
                value: SqlValue::List(keys),
            }));
        }
        let mut q = base.clone();
        q.joins.push(Join {
            target: junction,
            alias: M2M_ALIAS,
            kind: JoinKind::Inner,
            on: WhereExpr::And(on),
            project: vec![rel.src_col],
        });
        q
    };
    let label = crate::core::joins::joined_label(M2M_ALIAS, rel.src_col);
    let key = (label.as_str(), src_ty);
    let dialect = exec.again().dialect();
    let mut out = Vec::new();
    for keys in super::in_batches(dialect, &linked(None), child_qs.is_sliced(), keys)? {
        let q = linked(Some(keys));
        let rows = super::select_rows_keyed_on(exec.again(), &q, key);
        out.extend(rows.boxed().await?);
    }
    Ok(out)
}

/// Pairs each parent with the children read for its `on` value (else its
/// pk). A parent the query returned `n` times costs `n` child fetches: `C`
/// is not `Clone`.
struct Stitch<P, C> {
    parents: Vec<P>,
    /// Each parent's index into `keys`; `None` for a NULL key.
    slots: Vec<Option<usize>>,
    /// Each distinct key in first-seen order, with how many parents carry it.
    keys: Vec<(crate::core::SqlValue, usize)>,
    at: std::collections::HashMap<StitchKey, usize>,
    /// Per fetch round, the children of each key, by key index.
    rounds: Vec<Vec<Vec<C>>>,
}

impl<P: Model + HasPkValue, C> Stitch<P, C> {
    fn new(parents: Vec<P>, on: Option<&'static str>) -> Result<Self, ExecError> {
        let mut keys: Vec<(crate::core::SqlValue, usize)> = Vec::new();
        let mut at = std::collections::HashMap::new();
        let mut slots = Vec::with_capacity(parents.len());
        if !parents.is_empty() {
            P::SCHEMA
                .primary_key()
                .ok_or(ExecError::MissingPrimaryKey {
                    table: P::SCHEMA.table,
                })?;
        }
        for parent in &parents {
            let pk = parent_key(parent, on)?;
            if matches!(pk, crate::core::SqlValue::Null) {
                slots.push(None);
                continue;
            }
            let i = *at.entry(StitchKey::of(&pk)).or_insert_with(|| {
                keys.push((pk, 0));
                keys.len() - 1
            });
            keys[i].1 += 1;
            slots.push(Some(i));
        }
        Ok(Self {
            parents,
            slots,
            keys,
            at,
            rounds: Vec::new(),
        })
    }

    /// The keys of the next child fetch; `None` once every copy is read.
    fn next_keys(&self) -> Option<Vec<crate::core::SqlValue>> {
        let round = self.rounds.len();
        let batch: Vec<_> = self
            .keys
            .iter()
            .filter(|k| k.1 > round)
            .map(|k| k.0.clone())
            .collect();
        (!batch.is_empty()).then_some(batch)
    }

    /// The children the last [`Self::next_keys`] read, each with its key.
    fn push(&mut self, children: Vec<(crate::core::SqlValue, C)>) {
        let mut grouped: Vec<Vec<C>> = std::iter::repeat_with(Vec::new)
            .take(self.keys.len())
            .collect();
        for (fk, child) in children {
            if let Some(&i) = self.at.get(&StitchKey::owned(fk)) {
                grouped[i].push(child);
            }
        }
        self.rounds.push(grouped);
    }

    fn finish(mut self) -> Vec<(P, Vec<C>)> {
        let mut taken = vec![0_usize; self.keys.len()];
        let mut out = Vec::with_capacity(self.parents.len());
        for (parent, slot) in self.parents.into_iter().zip(self.slots) {
            let kids = slot.map_or_else(Vec::new, |i| {
                let copy = taken[i];
                taken[i] += 1;
                std::mem::take(&mut self.rounds[copy][i])
            });
            out.push((parent, kids));
        }
        out
    }
}

/// A stitch key: two values match when their display text does, so an
/// `I32` pk meets its `I64` junction key, with no string built for an int.
#[derive(Debug, PartialEq, Eq, Hash)]
enum StitchKey {
    Int(i64),
    Text(String),
}

impl StitchKey {
    fn of(v: &crate::core::SqlValue) -> Self {
        use crate::core::SqlValue;
        match v {
            SqlValue::I16(n) => Self::Int(i64::from(*n)),
            SqlValue::I32(n) => Self::Int(i64::from(*n)),
            SqlValue::I64(n) => Self::Int(*n),
            SqlValue::String(s) => {
                canonical_int(s).map_or_else(|| Self::Text(s.clone()), Self::Int)
            }
            other => Self::text(other.to_display_string()),
        }
    }

    /// [`Self::of`], moving a string in rather than copying it.
    fn owned(v: crate::core::SqlValue) -> Self {
        match v {
            crate::core::SqlValue::String(s) => Self::text(s),
            other => Self::of(&other),
        }
    }

    fn text(s: String) -> Self {
        canonical_int(&s).map_or(Self::Text(s), Self::Int)
    }
}

/// `s` as the `i64` whose display text it is exactly.
fn canonical_int(s: &str) -> Option<i64> {
    let n: i64 = s.parse().ok()?;
    let digits = s.strip_prefix('-').unwrap_or(s);
    let canonical =
        !s.starts_with('+') && !(digits.len() > 1 && digits.starts_with('0')) && s != "-0";
    canonical.then_some(n)
}

/// The value `C.child_fk_column` stores for `parent`, for the reverse
/// `<parent>::<child>_set` helpers: its `on` column, else its pk.
///
/// # Errors
/// [`ExecError::PrefetchColumnUnreadable`] when `P` cannot read that column.
#[doc(hidden)]
pub fn __reverse_fk_key<P: Model + HasPkValue, C: Model>(
    parent: &P,
    child_fk_column: &str,
) -> Result<crate::core::SqlValue, ExecError> {
    parent_key(parent, target_column::<P, C>(child_fk_column))
}

/// The parent column `C.child_fk_column` points at, when not `P`'s pk.
fn target_column<P: Model, C: Model>(child_fk_column: &str) -> Option<&'static str> {
    use crate::core::Relation;
    let field = C::SCHEMA
        .field(child_fk_column)
        .or_else(|| C::SCHEMA.field_by_column(child_fk_column))?;
    let on = match field.relation? {
        Relation::Fk { on, .. } | Relation::O2O { on, .. } => on,
    };
    (Some(on) != P::SCHEMA.pk_column()).then_some(on)
}

/// The value children store for `parent`: its `on` column, else its pk.
fn parent_key<P: Model + HasPkValue>(
    parent: &P,
    on: Option<&'static str>,
) -> Result<crate::core::SqlValue, ExecError> {
    match on {
        None => Ok(extract_pk_value(parent)),
        Some(column) => {
            parent
                .__rustango_column_value(column)
                .ok_or(ExecError::PrefetchColumnUnreadable {
                    table: P::SCHEMA.table,
                    column,
                })
        }
    }
}

/// The value `child` stores in its FK `column`; unreadable is an error, not a dropped row.
fn child_key<C: Model + HasPkValue>(
    child: &C,
    column: &'static str,
) -> Result<crate::core::SqlValue, ExecError> {
    child
        .__rustango_column_value(column)
        .ok_or(ExecError::PrefetchColumnUnreadable {
            table: C::SCHEMA.table,
            column,
        })
}

#[cfg(test)]
mod tests {
    use super::{child_key, parent_key, ExecError, HasPkValue, Stitch};
    use crate::core::{Model, ModelSchema, SqlValue};

    /// A parent with a hand-written `HasPkValue`, which reads only its pk.
    struct HandWritten;

    impl Model for HandWritten {
        const SCHEMA: &'static ModelSchema = <crate::contenttypes::ContentType as Model>::SCHEMA;
    }

    impl HasPkValue for HandWritten {
        fn __rustango_pk_value_impl(&self) -> SqlValue {
            SqlValue::I64(1)
        }
    }

    #[test]
    fn an_unreadable_fk_target_column_is_an_error_not_the_pk() {
        let err = parent_key(&HandWritten, Some("code")).expect_err("not the pk");
        assert!(
            matches!(
                err,
                ExecError::PrefetchColumnUnreadable { column: "code", .. }
            ),
            "{err}"
        );
        assert!(err.to_string().contains(".code`"), "{err}");
        assert_eq!(parent_key(&HandWritten, None).unwrap(), SqlValue::I64(1));
    }

    #[test]
    fn an_unreadable_child_fk_column_is_an_error_not_a_dropped_row() {
        let err = child_key(&HandWritten, "parent_id").expect_err("no column reader");
        assert!(
            matches!(
                err,
                ExecError::PrefetchColumnUnreadable {
                    column: "parent_id",
                    ..
                }
            ),
            "{err}"
        );
    }

    /// A parent whose pk is any value.
    struct Keyed(SqlValue);

    impl Model for Keyed {
        const SCHEMA: &'static ModelSchema = <crate::contenttypes::ContentType as Model>::SCHEMA;
    }

    impl HasPkValue for Keyed {
        fn __rustango_pk_value_impl(&self) -> SqlValue {
            self.0.clone()
        }
    }

    /// Keys match as their display text does: an `I32` pk meets an `I64` or
    /// `"1"` key, never `"01"`; a repeated parent takes the next round.
    #[test]
    fn stitch_keys_match_by_display_text() {
        use SqlValue::{Null, String as S, I16, I32, I64};
        let parents = [I32(1), I64(2), S("x".into()), I32(1), Null, S("3".into())];
        let mut stitch = Stitch::new(parents.into_iter().map(Keyed).collect(), None).unwrap();
        assert_eq!(
            stitch.next_keys().unwrap(),
            vec![I32(1), I64(2), S("x".into()), S("3".into())]
        );
        stitch.push(vec![
            (I64(1), "a"),
            (I16(2), "b"),
            (S("x".into()), "c"),
            (S("1".into()), "d"),
            (I64(3), "e"),
            (S("01".into()), "g"),
            (I64(9), "z"),
        ]);
        assert_eq!(stitch.next_keys().unwrap(), vec![I32(1)]);
        stitch.push(vec![(I64(1), "f")]);
        assert_eq!(stitch.next_keys(), None);
        let got: Vec<(SqlValue, Vec<&str>)> =
            stitch.finish().into_iter().map(|(p, k)| (p.0, k)).collect();
        assert_eq!(
            got,
            vec![
                (I32(1), vec!["a", "d"]),
                (I64(2), vec!["b"]),
                (S("x".into()), vec!["c"]),
                (I32(1), vec!["f"]),
                (Null, vec![]),
                (S("3".into()), vec!["e"]),
            ]
        );
    }
}
