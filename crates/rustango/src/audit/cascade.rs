//! `delete` audit rows for the audited rows an FK `ON DELETE CASCADE`
//! removes. Read before the delete, written after it for the rows it took.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::core::{
    DeleteQuery, Filter, ModelEntry, ModelSchema, OnDeleteAction, Op, Relation, SelectQuery,
    SqlValue, WhereExpr,
};
use crate::sql::{ExecError, PoolTx};

use super::{emit_many_tx, pk_in, PendingEntry, Via, BULK_AUDIT_CHUNK};

/// `child.fk_column` references `parent.on` with `ON DELETE CASCADE`.
struct Edge {
    child: &'static ModelEntry,
    fk_column: &'static str,
    on: &'static str,
}

/// The cascade edges out of `table` that lead to an audited model.
fn edges(table: &str) -> &'static [Edge] {
    static GRAPH: OnceLock<HashMap<&'static str, Vec<Edge>>> = OnceLock::new();
    GRAPH
        .get_or_init(build_graph)
        .get(table)
        .map_or(&[], Vec::as_slice)
}

/// One registry entry per table: a project model over a framework one,
/// then the first by model name, so the pick does not depend on link order.
fn canonical_entries() -> Vec<&'static ModelEntry> {
    let mut by_table: HashMap<&'static str, &'static ModelEntry> = HashMap::new();
    for entry in crate::core::inventory::iter::<ModelEntry> {
        let key = |e: &ModelEntry| (e.is_framework(), e.schema.name);
        by_table
            .entry(entry.schema.table)
            .and_modify(|kept| {
                if key(entry) < key(kept) {
                    *kept = entry;
                }
            })
            .or_insert(entry);
    }
    by_table.into_values().collect()
}

fn build_graph() -> HashMap<&'static str, Vec<Edge>> {
    let entries = canonical_entries();
    let mut graph: HashMap<&'static str, Vec<Edge>> = HashMap::new();
    for &entry in &entries {
        for field in entry.schema.fields {
            let (Some(OnDeleteAction::Cascade), Some(Relation::Fk { to, on } | Relation::O2O { to, on })) =
                (field.fk_on_delete, field.relation)
            else {
                continue;
            };
            graph.entry(to).or_default().push(Edge {
                child: entry,
                fk_column: field.column,
                on,
            });
        }
    }
    // Tables whose cascade reaches an audited model, to a fixed point.
    let mut reaches: HashSet<&str> = entries
        .iter()
        .filter(|e| e.schema.audit_track.is_some())
        .map(|e| e.schema.table)
        .collect();
    loop {
        let grown: Vec<&'static str> = graph
            .iter()
            .filter(|(t, es)| {
                !reaches.contains(*t) && es.iter().any(|e| reaches.contains(e.child.schema.table))
            })
            .map(|(t, _)| *t)
            .collect();
        if grown.is_empty() {
            break;
        }
        reaches.extend(grown);
    }
    for es in graph.values_mut() {
        es.retain(|e| reaches.contains(e.child.schema.table));
    }
    graph.retain(|_, es| !es.is_empty());
    graph
}

/// Whether deleting `model` rows may cascade to an audited model.
pub(crate) fn reaches_audited(model: &ModelSchema) -> bool {
    !edges(model.table).is_empty()
}

/// Audited rows a delete will cascade to, with their entries.
struct Pending(Vec<(&'static ModelSchema, Vec<(SqlValue, PendingEntry)>)>);

/// Delete `query`'s rows in chunks of roots, each with the audited rows it
/// cascades to: read them locked, delete the chunk, emit for the rows gone.
/// Memory holds one chunk's cascade, plus the keys of the rows seen.
pub(crate) async fn delete_with_audit(
    tx: &mut PoolTx<'_>,
    query: &DeleteQuery,
) -> Result<u64, ExecError> {
    let mut seen: HashSet<(&'static str, String)> = HashSet::new();
    let Some(pk) = query.model.primary_key() else {
        return delete_chunk(tx, query, &mut seen).await;
    };
    // Locked up front, so the chunks cover exactly the rows `query` matches.
    let roots = column_values(tx, query.model, query.where_clause.clone(), pk.column).await?;
    // An audited model's own rows are audited by its delete path.
    if query.model.audit_track.is_some() {
        seen.extend(roots.iter().map(|k| (query.model.table, format!("{k:?}"))));
    }
    let mut affected = 0;
    for chunk in roots.chunks(BULK_AUDIT_CHUNK) {
        let part = DeleteQuery {
            model: query.model,
            where_clause: pk_in(query.model, chunk.to_vec())?,
        };
        affected += delete_chunk(tx, &part, &mut seen).await?;
    }
    Ok(affected)
}

async fn delete_chunk(
    tx: &mut PoolTx<'_>,
    query: &DeleteQuery,
    seen: &mut HashSet<(&'static str, String)>,
) -> Result<u64, ExecError> {
    let pending = snapshot(tx, query, seen).await?;
    let plan = crate::sql::compile_delete_plan(tx.dialect(), query)?;
    let affected = crate::sql::run_delete_statements_tx(tx, plan).await?;
    emit_removed(tx, pending).await?;
    Ok(affected)
}

/// Read, locked, every audited row deleting `query`'s rows would cascade to.
async fn snapshot(
    tx: &mut PoolTx<'_>,
    query: &DeleteQuery,
    seen: &mut HashSet<(&'static str, String)>,
) -> Result<Pending, ExecError> {
    let mut out: Vec<(&'static ModelSchema, Vec<(SqlValue, PendingEntry)>)> = Vec::new();
    let mut frontier = vec![(query.model, query.where_clause.clone())];
    while let Some((model, rows)) = frontier.pop() {
        for edge in edges(model.table) {
            let keys = column_values(tx, model, rows.clone(), edge.on).await?;
            let child = edge.child.schema;
            let pk = child
                .primary_key()
                .ok_or(ExecError::MissingPrimaryKey { table: child.table })?;
            for chunk in keys.chunks(BULK_AUDIT_CHUNK) {
                let child_rows = WhereExpr::Predicate(Filter::new(
                    edge.fk_column,
                    Op::In,
                    SqlValue::List(chunk.to_vec()),
                ));
                let found: Vec<(SqlValue, Option<PendingEntry>)> = if child.audit_track.is_some() {
                    let read = edge.child.audited_cascade().ok_or(ExecError::AuditUnsupported {
                        table: child.table,
                        reason: "its registry entry has no cascade recorder",
                    })?;
                    let rows = read(tx, child_rows).await?;
                    rows.into_iter().map(|(k, e)| (k, Some(e))).collect()
                } else {
                    let pks = column_values(tx, child, child_rows, pk.column).await?;
                    pks.into_iter().map(|k| (k, None)).collect()
                };
                let mut fresh = Vec::new();
                let mut entries = Vec::new();
                for (key, entry) in found {
                    if seen.insert((child.table, format!("{key:?}"))) {
                        fresh.push(key.clone());
                        entries.extend(entry.map(|e| (key, e)));
                    }
                }
                if !entries.is_empty() {
                    out.push((child, entries));
                }
                if reaches_audited(child) {
                    for part in fresh.chunks(BULK_AUDIT_CHUNK) {
                        frontier.push((child, pk_in(child, part.to_vec())?));
                    }
                }
            }
        }
    }
    Ok(Pending(out))
}

/// Emit the entries of the snapshot rows the delete removed.
async fn emit_removed(tx: &mut PoolTx<'_>, pending: Pending) -> Result<(), ExecError> {
    for (model, rows) in pending.0 {
        let pk = model
            .primary_key()
            .ok_or(ExecError::MissingPrimaryKey { table: model.table })?;
        let keys: Vec<SqlValue> = rows.iter().map(|(k, _)| k.clone()).collect();
        // A missing FK (or SQLite with `foreign_keys` off) cascades nothing.
        let mut left = HashSet::new();
        for part in keys.chunks(BULK_AUDIT_CHUNK) {
            let found = column_values(tx, model, pk_in(model, part.to_vec())?, pk.column).await?;
            left.extend(found.iter().map(|k| format!("{k:?}")));
        }
        let entries: Vec<PendingEntry> = rows
            .into_iter()
            .filter(|(k, _)| !left.contains(&format!("{k:?}")))
            .map(|(_, e)| e)
            .collect();
        emit_many_tx(tx, Via::Scoped, &entries).await?;
    }
    Ok(())
}

/// `column` of `model`'s rows matching `rows`, locked on PG / MySQL.
async fn column_values(
    tx: &mut PoolTx<'_>,
    model: &'static ModelSchema,
    rows: WhereExpr,
    column: &'static str,
) -> Result<Vec<SqlValue>, ExecError> {
    let mut select = SelectQuery::new(model).where_clause(rows);
    select.projection = Some(vec![column]);
    select.lock_mode = Some(crate::core::LockMode {
        silent_on_sqlite: true,
        ..crate::core::LockMode::default()
    });
    let rows = crate::sql::fetch_values_list_on(tx, &select).await?;
    Ok(rows.into_iter().filter_map(|r| r.into_iter().next()).collect())
}

/// Run `fut`, a delete through `pool`, so cascade entries keep the source.
pub(crate) async fn via_pool<F: std::future::Future>(
    pool: &crate::sql::Pool,
    fut: F,
) -> F::Output {
    super::writing_via(pool, fut).await
}

/// Warn once that a delete on a single-statement executor cascaded to an
/// audited model without audit rows.
pub(crate) fn warn_unaudited(table: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            table,
            "delete cascades to an audited model, but this executor runs one statement, \
             so the cascaded rows get no audit rows; use `delete_pool` or a `PoolTx`"
        );
    }
}
