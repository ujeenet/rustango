//! `flush`.

use super::*;

#[derive(Debug, Default, PartialEq)]
pub(super) struct FlushArgs {
    /// `--yes` confirms data destruction. Without it, the command
    /// prints what would happen (dry-run) and exits.
    pub(super) yes: bool,
    /// Limit to a specific app or set of apps.
    pub(super) apps: Vec<String>,
    /// Limit to a specific model or set of models.
    pub(super) models: Vec<String>,
    pub(super) help: bool,
}

pub(super) fn parse_flush_args(args: &[String]) -> Result<FlushArgs, MigrateError> {
    let mut out = FlushArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                out.help = true;
                return Ok(out);
            }
            "--yes" => out.yes = true,
            "--app" => {
                let v = iter.next().ok_or_else(|| missing_value("--app"))?;
                out.apps.push(v.clone());
            }
            "--model" => {
                let v = iter.next().ok_or_else(|| missing_value("--model"))?;
                out.models.push(v.clone());
            }
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                return Err(unexpected_positional(other));
            }
        }
    }
    Ok(out)
}

/// Which models `flush` may touch.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FlushScope<'a> {
    /// Every managed model: single-tenant `manage flush`.
    All,
    /// Tenant-scoped models only (#2284). `schema` names a schema-mode
    /// tenant; PG names are qualified so `search_path` never falls through to `public`.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Tenant { schema: Option<&'a str> },
}

/// The DETAIL line of a Postgres error, if there is one.
pub(super) fn pg_error_detail(e: &crate::sql::ExecError) -> Option<String> {
    #[cfg(feature = "postgres")]
    if let crate::sql::ExecError::Driver(sqlx::Error::Database(db)) = e {
        return db
            .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .and_then(|pg| pg.detail())
            .map(str::to_owned);
    }
    let _ = e;
    None
}

/// `flush` may touch this model: managed, not a view (#2285), in scope (#2284).
pub(super) fn flush_eligible(s: &crate::core::ModelSchema, scope: FlushScope<'_>) -> bool {
    s.managed
        && !s.is_view
        && (matches!(scope, FlushScope::All) || s.scope == crate::core::ModelScope::Tenant)
}

/// Auto-created M2M junctions whose source or destination is a target.
pub(super) fn flush_junctions(
    targets: &[&'static crate::core::ModelSchema],
    scope: FlushScope<'_>,
) -> Vec<&'static crate::core::ModelSchema> {
    let is_target = |t: &str| targets.iter().any(|s| s.table == t);
    let mut out: Vec<&'static crate::core::ModelSchema> = Vec::new();
    for entry in inventory::iter::<crate::core::ModelEntry>() {
        let s = entry.schema;
        if !flush_eligible(s, scope) {
            continue;
        }
        for m in s.m2m {
            if m.auto_create
                && (is_target(s.table) || is_target(m.to))
                && !is_target(m.through)
                && !out.iter().any(|j| j.table == m.through)
            {
                out.push(crate::sql::m2m::junction(
                    m.through,
                    &[m.src_col, m.dst_col],
                ));
            }
        }
    }
    out
}

/// FK and one-to-one links out of `s`: the target table, and the column
/// when it may be set to NULL. Composite links never can.
pub(super) fn fk_edges(
    s: &crate::core::ModelSchema,
) -> impl Iterator<Item = (&'static str, Option<&'static str>)> + '_ {
    use crate::core::Relation;
    let single = s.fields.iter().filter_map(|f| match f.relation {
        Some(Relation::Fk { to, .. } | Relation::O2O { to, .. }) => {
            Some((to, f.nullable.then_some(f.column)))
        }
        _ => None,
    });
    single.chain(s.composite_relations.iter().map(|c| (c.to, None)))
}

/// `nodes` with each table after every node that references it through an
/// edge `binds` keeps (self-links never count). Returns the order and the
/// nodes left in a cycle.
pub(super) fn children_first(
    nodes: &[&'static crate::core::ModelSchema],
    binds: impl Fn(&crate::core::ModelSchema, Option<&'static str>) -> bool,
) -> (
    Vec<&'static crate::core::ModelSchema>,
    Vec<&'static crate::core::ModelSchema>,
) {
    let links = |from: &crate::core::ModelSchema, to: &str| {
        from.table != to && fk_edges(from).any(|(t, col)| t == to && binds(from, col))
    };
    let mut done = vec![false; nodes.len()];
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(i) = (0..nodes.len()).find(|&i| {
        !done[i] && !(0..nodes.len()).any(|j| !done[j] && links(nodes[j], nodes[i].table))
    }) {
        done[i] = true;
        order.push(nodes[i]);
    }
    let stuck = (0..nodes.len())
        .filter(|&i| !done[i])
        .map(|i| nodes[i])
        .collect();
    (order, stuck)
}

/// How `flush` empties `targets` without a table-wide FK bypass.
pub(super) struct FlushPlan {
    /// Nullable links to clear first: self-links, and links inside a cycle.
    pub(super) unlink: Vec<(&'static crate::core::ModelSchema, Vec<&'static str>)>,
    /// Children before parents.
    pub(super) order: Vec<&'static crate::core::ModelSchema>,
    /// Tables in a cycle of NOT NULL links: no delete order exists.
    pub(super) stuck: Vec<&'static str>,
}

pub(super) fn flush_plan(targets: &[&'static crate::core::ModelSchema]) -> FlushPlan {
    let (mut order, cyclic) = children_first(targets, |_, _| true);
    // Inside a cycle only NOT NULL links bind; nullable ones are cleared first.
    let (rest, stuck) = children_first(&cyclic, |_, col| col.is_none());
    order.extend(rest);
    order.extend(&stuck);
    let in_cycle = |t: &str| cyclic.iter().any(|c| c.table == t);
    let unlink = targets
        .iter()
        .map(|s| {
            let cols: Vec<&'static str> = fk_edges(s)
                .filter(|(to, _)| *to == s.table || (in_cycle(s.table) && in_cycle(to)))
                .filter_map(|(_, col)| col)
                .collect();
            (*s, cols)
        })
        .filter(|(_, cols)| !cols.is_empty())
        .collect();
    FlushPlan {
        unlink,
        order,
        stuck: stuck.iter().map(|s| s.table).collect(),
    }
}

/// Run `plan` after clearing `junctions`, in one transaction; on error nothing commits.
pub(super) async fn delete_all_in_tx(
    pool: &Pool,
    junctions: &[&'static crate::core::ModelSchema],
    plan: &FlushPlan,
) -> Result<(), (String, String)> {
    use crate::core::{Assignment, DeleteQuery, SqlValue, UpdateQuery, WhereExpr};
    let defer = pool.dialect().defer_foreign_keys_sql();
    if defer.is_none() && !plan.stuck.is_empty() {
        let tables = plan.stuck.join(", ");
        let why = "they reference each other through NOT NULL foreign keys; no delete order exists";
        return Err((tables, why.to_owned()));
    }
    let mut tx = crate::sql::write_transaction(pool)
        .await
        .map_err(|e| ("BEGIN".to_owned(), e.to_string()))?;
    // SQLite: FK checks wait for COMMIT, for this transaction only.
    if let Some(sql) = defer {
        crate::sql::raw_execute_tx(&mut tx, sql, Vec::new())
            .await
            .map_err(|e| ("BEGIN".to_owned(), e.to_string()))?;
    }
    // InnoDB checks each row as it goes: links inside a cycle, or to the
    // row's own table, must go before the DELETEs.
    for (schema, cols) in &plan.unlink {
        let unlink = UpdateQuery {
            model: schema,
            set: cols
                .iter()
                .map(|c| Assignment::new(c, SqlValue::Null))
                .collect(),
            where_clause: WhereExpr::And(Vec::new()),
        };
        crate::sql::update_tx(&mut tx, &unlink)
            .await
            .map_err(|e| (schema.table.to_owned(), e.to_string()))?;
    }
    for schema in junctions.iter().chain(&plan.order) {
        let all = DeleteQuery {
            model: schema,
            where_clause: WhereExpr::And(Vec::new()),
        };
        crate::sql::delete_tx(&mut tx, &all)
            .await
            .map_err(|e| (schema.table.to_owned(), e.to_string()))?;
    }
    tx.commit()
        .await
        .map_err(|e| ("COMMIT".to_owned(), e.to_string()))
}

/// `manage flush [--yes] [--app <label>] [--model <name>]` — wipe
/// all rows from registered model tables.
/// Without `--yes`, prints what would happen and exits without
/// touching the database (dry-run by default — a hand-typed
/// `manage flush` doesn't accidentally nuke production).
///
/// On PG, emits `TRUNCATE table1, table2, ... RESTART IDENTITY` in a
/// single statement so FK constraints resolve and sequences reset. On
/// MySQL/SQLite, emits per-table `DELETE FROM <table>` in one
/// transaction, children before parents; sequences are NOT reset
/// (caller can `DROP SEQUENCE` + `CREATE SEQUENCE` manually if
/// they need that). The migrations ledger is left untouched —
/// flush wipes data, not schema or schema history.
///
/// `--app <label>` / `--model <name>` filters narrow the wipe.
/// Pass either flag multiple times to limit to a set.
pub(crate) async fn flush_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    scope: FlushScope<'_>,
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_flush_args(args)?;
    if parsed.help {
        writeln!(w, "flush [--yes] [--app <label>] [--model <name>]")?;
        writeln!(w)?;
        writeln!(
            w,
            "  Wipe all rows from registered model tables. Schema + migrations ledger"
        )?;
        writeln!(w, "  stay intact. Unmanaged models and views are skipped.")?;
        writeln!(w)?;
        writeln!(
            w,
            "  --yes              Confirm. Without this, prints the planned action"
        )?;
        writeln!(w, "                     and exits without touching the DB.")?;
        writeln!(w, "  --app <label>      Limit to one app (repeatable).")?;
        writeln!(w, "  --model <name>     Limit to one model (repeatable).")?;
        writeln!(w)?;
        writeln!(
            w,
            "  Postgres runs TRUNCATE … RESTART IDENTITY: ids restart. A table outside"
        )?;
        writeln!(
            w,
            "  the filter that references a target makes the flush fail; nothing is cleared."
        )?;
        writeln!(
            w,
            "  MySQL / SQLite delete the rows in one transaction and keep their id counters."
        )?;
        return Ok(());
    }

    // Collect target tables in inventory order.
    let mut targets: Vec<&'static crate::core::ModelSchema> = Vec::new();
    for entry in inventory::iter::<crate::core::ModelEntry>() {
        let schema = entry.schema;
        if !flush_eligible(schema, scope) {
            continue;
        }
        let app = entry.resolved_app_label().unwrap_or("");
        let dotted = if app.is_empty() {
            schema.name.to_owned()
        } else {
            format!("{app}.{}", schema.name)
        };
        if !parsed.apps.is_empty() && !parsed.apps.iter().any(|a| a == app) {
            continue;
        }
        if !parsed.models.is_empty()
            && !parsed
                .models
                .iter()
                .any(|m| m == &dotted || m == schema.name)
        {
            continue;
        }
        targets.push(schema);
    }
    if targets.is_empty() {
        writeln!(w, "flush: no tables match the filter (nothing to do)")?;
        return Ok(());
    }
    // PG will not truncate a table an auto-created junction still references.
    let junctions = flush_junctions(&targets, scope);

    if !parsed.yes {
        writeln!(
            w,
            "flush: would clear {} table(s) (run with --yes to execute):",
            junctions.len() + targets.len()
        )?;
        for t in junctions.iter().chain(&targets) {
            writeln!(w, "  - {}", t.table)?;
        }
        return Ok(());
    }

    // Execute. Per-dialect strategy:
    let dialect = pool.dialect().name();
    let mut cleared = 0_usize;
    let mut failures: Vec<(String, String)> = Vec::new();
    if dialect == "postgres" {
        // One big TRUNCATE — atomic, FK-aware, sequence-resetting.
        let d = pool.dialect();
        let prefix = match scope {
            FlushScope::Tenant { schema: Some(s) } => format!("{}.", d.quote_ident(s)),
            _ => String::new(),
        };
        let quoted: Vec<String> = junctions
            .iter()
            .chain(&targets)
            .map(|t| format!("{prefix}{}", d.quote_ident(t.table)))
            .collect();
        // No CASCADE: a table outside the targets that references one makes PG refuse.
        let sql = format!("TRUNCATE TABLE {} RESTART IDENTITY", quoted.join(", "));
        match crate::sql::raw_execute_pool(pool, &sql, Vec::new()).await {
            Ok(_) => cleared = quoted.len(),
            // PG's DETAIL names the blocking table; the message alone does not.
            Err(e) => failures.push((
                "TRUNCATE".to_owned(),
                match pg_error_detail(&e) {
                    Some(detail) => format!("{e} ({detail})"),
                    None => format!("{e} (tables: {})", quoted.join(", ")),
                },
            )),
        }
    } else {
        // MySQL / SQLite: one transaction, junctions then children before
        // parents, so a failure leaves every table as it was (#2285).
        match delete_all_in_tx(pool, &junctions, &flush_plan(&targets)).await {
            Ok(()) => cleared = junctions.len() + targets.len(),
            Err(failed) => failures.push(failed),
        }
    }

    writeln!(w, "flush: cleared {cleared} table(s)")?;
    if !failures.is_empty() {
        writeln!(w, "flush: {} table(s) failed:", failures.len())?;
        for (t, e) in &failures {
            writeln!(w, "  - {t}: {e}")?;
        }
        // Surface as an error so the caller's exit code reflects partial failure.
        let what: Vec<String> = failures.iter().map(|(t, e)| format!("{t}: {e}")).collect();
        return Err(MigrateError::Validation(format!(
            "flush completed with {} failure(s): {}",
            failures.len(),
            what.join("; ")
        )));
    }
    Ok(())
}
