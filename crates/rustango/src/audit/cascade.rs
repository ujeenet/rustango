//! `delete` audit rows for the audited rows an FK `ON DELETE CASCADE`
//! removes, `update` rows for the ones `SET NULL` / `SET DEFAULT` changes.
//! Read before the delete, written after it for the rows it took or changed.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::core::{
    CountQuery, DeleteQuery, Filter, ModelEntry, ModelSchema, OnDeleteAction, Op, Relation, SelectQuery,
    SqlValue, WhereExpr,
};
use crate::sql::{ExecError, PoolTx};

use super::{emit_many_tx, pk_in, AuditOp, PendingEntry, Via, BULK_AUDIT_CHUNK};

/// `child.fk_column` references `parent.on` with `ON DELETE CASCADE`, or
/// with `SET NULL` / `SET DEFAULT` to an audited child (`sets`).
struct Edge {
    child: &'static ModelEntry,
    fk_column: &'static str,
    on: &'static str,
    sets: bool,
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
        let audited = entry.schema.audit_track.is_some();
        for field in entry.schema.fields {
            let (Some(action), Some(Relation::Fk { to, on } | Relation::O2O { to, on })) =
                (field.fk_on_delete, field.relation)
            else {
                continue;
            };
            let sets = match action {
                OnDeleteAction::Cascade => false,
                OnDeleteAction::SetNull | OnDeleteAction::SetDefault if audited => true,
                _ => continue,
            };
            graph.entry(to).or_default().push(Edge {
                child: entry,
                fk_column: field.column,
                on,
                sets,
            });
        }
    }
    // Tables whose cascade reaches an audited model, to a fixed point.
    let mut reaches: HashSet<&str> = entries
        .iter()
        .filter(|e| e.schema.audit_track.is_some())
        .map(|e| e.schema.table)
        .collect();
    reaches.extend(
        graph
            .iter()
            .filter(|(_, es)| es.iter().any(|e| e.sets))
            .map(|(t, _)| *t),
    );
    loop {
        let grown: Vec<&'static str> = graph
            .iter()
            .filter(|(t, es)| {
                !reaches.contains(*t)
                    && es
                        .iter()
                        .any(|e| !e.sets && reaches.contains(e.child.schema.table))
            })
            .map(|(t, _)| *t)
            .collect();
        if grown.is_empty() {
            break;
        }
        reaches.extend(grown);
    }
    for es in graph.values_mut() {
        es.retain(|e| e.sets || reaches.contains(e.child.schema.table));
    }
    graph.retain(|_, es| !es.is_empty());
    graph
}

/// The audited tables a truncate of `table` empties besides it: PG's
/// `TRUNCATE … CASCADE` follows every FK, a `DELETE` only `ON DELETE CASCADE`.
fn truncate_reach(table: &str, every_fk: bool) -> Vec<&'static ModelSchema> {
    let entries = canonical_entries();
    let mut out = Vec::new();
    let mut done: HashSet<&str> = HashSet::from([table]);
    let mut frontier = vec![table.to_owned()];
    while let Some(parent) = frontier.pop() {
        for entry in &entries {
            let schema = entry.schema;
            let by_field = schema.fields.iter().any(|f| {
                matches!(f.relation, Some(Relation::Fk { to, .. } | Relation::O2O { to, .. }) if to == parent)
                    && (every_fk || f.fk_on_delete == Some(OnDeleteAction::Cascade))
            });
            let by_composite = every_fk && schema.composite_relations.iter().any(|r| r.to == parent);
            if (by_field || by_composite) && done.insert(schema.table) {
                if schema.audit_track.is_some() {
                    out.push(schema);
                }
                frontier.push(schema.table.to_owned());
            }
        }
    }
    out
}

/// The audited tables a truncate of `table` reaches through its FKs, with
/// their row counts; taken before it runs, for [`emit_truncated`].
pub(crate) async fn truncate_counts(
    tx: &mut PoolTx<'_>,
    table: &str,
) -> Result<Vec<(&'static ModelSchema, i64)>, ExecError> {
    let every_fk = crate::sql::truncate_cascades_every_fk(tx.dialect());
    let mut out = Vec::new();
    for schema in truncate_reach(table, every_fk) {
        out.push((schema, row_count(tx, schema).await?));
    }
    Ok(out)
}

async fn row_count(tx: &mut PoolTx<'_>, schema: &'static ModelSchema) -> Result<i64, ExecError> {
    let query = CountQuery::from_select(SelectQuery::new(schema));
    crate::sql::count_rows_on(&mut *tx, &query).await
}

/// One bulk `delete` entry per audited table the truncate of `table` took
/// rows from. A MySQL/SQLite `SET NULL` / `SET DEFAULT` child is not audited.
pub(crate) async fn emit_truncated(
    tx: &mut PoolTx<'_>,
    table: &str,
    before: Vec<(&'static ModelSchema, i64)>,
) -> Result<(), ExecError> {
    let mut entries = Vec::new();
    for (schema, rows) in before {
        if rows > 0 && row_count(tx, schema).await? < rows {
            entries.push(PendingEntry {
                entity_table: schema.table,
                entity_pk: String::new(),
                operation: AuditOp::Delete,
                source: super::current_source(),
                changes: serde_json::json!({ "bulk": "truncate", "cascade_from": table }),
            });
        }
    }
    emit_many_tx(tx, Via::Scoped, &entries).await
}

/// Whether deleting `model` rows may cascade to an audited model.
pub(crate) fn reaches_audited(model: &ModelSchema) -> bool {
    !edges(model.table).is_empty()
}

/// Audited rows a delete will cascade to, with their entries, and the
/// audited rows whose FK it will set.
struct Pending {
    removed: Vec<(&'static ModelSchema, Vec<(SqlValue, PendingEntry)>)>,
    set: Vec<SetRows>,
}

/// Audited `child` rows `pks` whose `fk_column` holds one of `keys`.
struct SetRows {
    child: &'static ModelEntry,
    fk_column: &'static str,
    keys: Vec<SqlValue>,
    pks: Vec<SqlValue>,
}

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
    emit_set(tx, pending.set, seen).await?;
    emit_removed(tx, pending.removed).await?;
    Ok(affected)
}

/// Read, locked, every audited row deleting `query`'s rows would cascade to.
async fn snapshot(
    tx: &mut PoolTx<'_>,
    query: &DeleteQuery,
    seen: &mut HashSet<(&'static str, String)>,
) -> Result<Pending, ExecError> {
    let mut out: Vec<(&'static ModelSchema, Vec<(SqlValue, PendingEntry)>)> = Vec::new();
    let mut set = Vec::new();
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
                if edge.sets {
                    let pks = column_values(tx, child, child_rows, pk.column).await?;
                    if !pks.is_empty() {
                        set.push(SetRows {
                            child: edge.child,
                            fk_column: edge.fk_column,
                            keys: chunk.to_vec(),
                            pks,
                        });
                    }
                    continue;
                }
                let found: Vec<(SqlValue, Option<PendingEntry>)> = if child.audit_track.is_some() {
                    let read = edge
                        .child
                        .audited_cascade()
                        .ok_or(ExecError::AuditUnsupported {
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
    Ok(Pending { removed: out, set })
}

/// Emit an `update` entry for each audited row the delete set the FK of:
/// still there (no other edge removed it), no longer pointing at a key.
async fn emit_set(
    tx: &mut PoolTx<'_>,
    set: Vec<SetRows>,
    seen: &mut HashSet<(&'static str, String)>,
) -> Result<(), ExecError> {
    for rows in set {
        let child = rows.child.schema;
        let read = rows
            .child
            .audited_cascade()
            .ok_or(ExecError::AuditUnsupported {
                table: child.table,
                reason: "its registry entry has no cascade recorder",
            })?;
        let pk = child
            .primary_key()
            .ok_or(ExecError::MissingPrimaryKey { table: child.table })?;
        let mut entries = Vec::new();
        for part in rows.pks.chunks(BULK_AUDIT_CHUNK) {
            // A missing FK (or SQLite with `foreign_keys` off) sets nothing.
            let unchanged = WhereExpr::And(vec![
                pk_in(child, part.to_vec())?,
                WhereExpr::Predicate(Filter::new(
                    rows.fk_column,
                    Op::In,
                    SqlValue::List(rows.keys.clone()),
                )),
            ]);
            let left: HashSet<String> = column_values(tx, child, unchanged, pk.column)
                .await?
                .iter()
                .map(|k| format!("{k:?}"))
                .collect();
            for (key, mut entry) in read(tx, pk_in(child, part.to_vec())?).await? {
                let key = format!("{key:?}");
                if !left.contains(&key) && seen.insert((child.table, format!("set {key}"))) {
                    entry.operation = AuditOp::Update;
                    entries.push(entry);
                }
            }
        }
        emit_many_tx(tx, Via::Scoped, &entries).await?;
    }
    Ok(())
}

/// Emit the entries of the snapshot rows the delete removed.
async fn emit_removed(
    tx: &mut PoolTx<'_>,
    removed: Vec<(&'static ModelSchema, Vec<(SqlValue, PendingEntry)>)>,
) -> Result<(), ExecError> {
    for (model, rows) in removed {
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
    Ok(rows
        .into_iter()
        .filter_map(|r| r.into_iter().next())
        .collect())
}

/// Run `fut`, a delete through `pool`, so cascade entries keep the source.
pub(crate) async fn via_pool<F: std::future::Future>(pool: &crate::sql::Pool, fut: F) -> F::Output {
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
