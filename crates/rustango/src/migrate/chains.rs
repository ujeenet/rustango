//! The one order every entry point applies the framework's system chain and
//! the project's own chain in: `manage migrate`, the `Cli` auto-migrate and
//! the tenancy runners all go through [`migrate_chains`].

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};

use super::diff::SchemaChange;
use super::file::{self, Migration, MigrationScope, Operation};
use super::make::SystemChain;
use super::progress::MigrationObserver;
use super::runner::{LockHeld, Signals};
use super::{ensure, runner, MigrateError, SchemaSnapshot};
use crate::sql::Pool;

/// What each chain applied.
pub(crate) struct Applied {
    pub(crate) system: Vec<Migration>,
    pub(crate) project: Vec<Migration>,
}

/// Apply `chain`'s `scope` steps, then the project chain via `project`, then
/// the system steps that waited for it, all under one migrate lock.
///
/// A project scaffolded before the system chain creates framework tables in
/// its own migrations. Those tables stay the project's: the system chain
/// never creates them, and its steps on them wait until the project chain
/// has run, after the columns each step expects are added.
///
/// # Errors
/// Either chain's error, a column convergence could not add, or a system
/// step still waiting after the project chain ran.
pub(crate) async fn migrate_chains<F, Fut>(
    pool: &Pool,
    chain: &SystemChain,
    scope: MigrationScope,
    project_dir: &Path,
    observer: Option<&dyn MigrationObserver>,
    signals: Signals,
    project: F,
) -> Result<Applied, MigrateError>
where
    F: FnOnce(LockHeld) -> Fut,
    Fut: Future<Output = Result<Vec<Migration>, MigrateError>>,
{
    let run = locked_chains(pool, chain, scope, project_dir, observer, project);
    match signals {
        Signals::Skip => run.await,
        Signals::Fire => {
            let names = |a: &Applied| a.project.iter().map(|m| m.name.clone()).collect();
            runner::with_migrate_signals(run, names).await
        }
    }
}

async fn locked_chains<F, Fut>(
    pool: &Pool,
    chain: &SystemChain,
    scope: MigrationScope,
    project_dir: &Path,
    observer: Option<&dyn MigrationObserver>,
    project: F,
) -> Result<Applied, MigrateError>
where
    F: FnOnce(LockHeld) -> Fut,
    Fut: Future<Output = Result<Vec<Migration>, MigrateError>>,
{
    runner::with_migrate_lock_held(pool, |held| async move {
        let tables = ProjectTables::read(pool, project_dir).await?;
        let run = Run {
            held,
            pool,
            chain,
            scope,
            owned: &tables.owned,
            observer,
        };
        // Boxed: inline, these futures overflow the stack of deep `manage` callers.
        let first = Box::pin(run.system_pass(&tables.pending)).await?;
        let project = Box::pin(project(held)).await?;
        let second = Box::pin(run.system_pass(&BTreeSet::new())).await?;
        if let Some(table) = second.waiting {
            return Err(MigrateError::Validation(format!(
                "a system migration needs `{table}`, which the project's migrations \
                 claim but did not create"
            )));
        }
        Box::pin(run.finish(&tables.claimed)).await?;
        let mut system = first.applied;
        system.extend(second.applied);
        Ok(Applied { system, project })
    })
    .await
}

/// The framework-relevant shape of the project chain.
struct ProjectTables {
    /// Tables the chain ends with: its ops replayed in order.
    owned: BTreeSet<String>,
    /// Tables any of its migrations creates, dropped later or not.
    claimed: BTreeSet<String>,
    /// Tables its pending migrations write. Schema ops only: raw SQL and
    /// callbacks are opaque, so a table they write is not counted busy.
    pending: BTreeSet<String>,
}

impl ProjectTables {
    async fn read(pool: &Pool, dir: &Path) -> Result<Self, MigrateError> {
        let migs = file::list_dir(dir)?;
        let applied = runner::applied_set_pool_with_ledger(pool, runner::LEDGER_TABLE)
            .await
            .unwrap_or_default();
        let mut out = Self {
            owned: BTreeSet::new(),
            claimed: BTreeSet::new(),
            pending: BTreeSet::new(),
        };
        for m in &migs {
            let is_pending = !applied.contains(&m.name);
            for c in schema_ops(m) {
                match c {
                    SchemaChange::CreateTable(t)
                    | SchemaChange::CreateM2MTable { through: t, .. } => {
                        out.owned.insert(t.clone());
                        out.claimed.insert(t.clone());
                    }
                    SchemaChange::DropTable(t) | SchemaChange::DropM2MTable { through: t } => {
                        out.owned.remove(t);
                    }
                    SchemaChange::RenameTable { old_name, new_name } => {
                        if out.owned.remove(old_name) {
                            out.owned.insert(new_name.clone());
                        }
                        if is_pending {
                            out.pending.insert(new_name.clone());
                        }
                    }
                    _ => {}
                }
                if is_pending {
                    out.pending.insert(c.table().to_owned());
                }
            }
        }
        Ok(out)
    }
}

fn schema_ops(m: &Migration) -> impl Iterator<Item = &SchemaChange> {
    m.forward.iter().filter_map(|op| match op {
        Operation::Schema(c) => Some(c),
        _ => None,
    })
}

fn created(m: &Migration) -> impl Iterator<Item = String> + '_ {
    schema_ops(m).filter_map(|c| match c {
        SchemaChange::CreateTable(t) | SchemaChange::CreateM2MTable { through: t, .. } => {
            Some(t.clone())
        }
        _ => None,
    })
}

/// Names of the indexes a step creates together with their table and no
/// later step drops: a re-created one is the system chain's again (#2139).
fn indexes_made_with_table(steps: &[Migration]) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    for m in steps {
        let made: BTreeSet<String> = created(m).collect();
        for c in schema_ops(m) {
            match c {
                SchemaChange::CreateIndex { name, table, .. } if made.contains(table) => {
                    names.insert(name.as_str());
                }
                SchemaChange::DropIndex { name, .. } => {
                    names.remove(name.as_str());
                }
                _ => {}
            }
        }
    }
    names
}

fn touches(step: &Migration, table: &str) -> bool {
    schema_ops(step).any(|c| c.touches(table, &step.snapshot))
}

/// Columns of `table` that `step`'s FKs point at.
fn fk_targets(step: &Migration, table: &str) -> BTreeSet<String> {
    let snap = &step.snapshot;
    let pk = || {
        snap.table(table)
            .into_iter()
            .flat_map(|t| t.fields.iter().filter(|f| f.primary_key))
            .map(|f| f.column.clone())
            .collect::<Vec<_>>()
    };
    let field_fk = |f: &super::snapshot::FieldSnapshot| {
        f.fk.as_ref()
            .filter(|r| r.to == table)
            .map(|r| r.on.clone())
    };
    let mut out = BTreeSet::new();
    for c in schema_ops(step) {
        match c {
            SchemaChange::CreateTable(t) => {
                if let Some(ts) = snap.table(t) {
                    for f in &ts.fields {
                        out.extend(field_fk(f));
                    }
                    for cf in ts.composite_fks.iter().filter(|cf| cf.to == table) {
                        out.extend(cf.on.iter().cloned());
                    }
                }
            }
            SchemaChange::AddColumn { table: t, column } => {
                if let Some(f) = snap.table(t).and_then(|ts| ts.field(column)) {
                    out.extend(field_fk(f));
                }
            }
            SchemaChange::AddCompositeFk { to, on, .. } if to == table => {
                out.extend(on.iter().cloned());
            }
            SchemaChange::CreateM2MTable {
                src_table,
                dst_table,
                ..
            } if src_table == table || dst_table == table => out.extend(pk()),
            _ => {}
        }
    }
    out
}

fn writes(step: &Migration, table: &str) -> bool {
    schema_ops(step).any(|c| op_writes(c, table))
}

fn op_writes(c: &SchemaChange, table: &str) -> bool {
    c.table() == table
        || matches!(c, SchemaChange::RenameTable { new_name, .. } if new_name == table)
}

/// Whether `c` must run after the waiting `d`: either writes or references
/// a table the other writes.
fn conflicts(
    c: &SchemaChange,
    snap: &SchemaSnapshot,
    d: &SchemaChange,
    at: &SchemaSnapshot,
) -> bool {
    let names = |x: &SchemaChange| {
        let renamed = match x {
            SchemaChange::RenameTable { new_name, .. } => Some(new_name.clone()),
            _ => None,
        };
        std::iter::once(x.table().to_owned()).chain(renamed)
    };
    names(d).any(|t| c.touches(&t, snap)) || names(c).any(|t| d.touches(&t, at))
}

/// A system step's schema op that waits, and the snapshot it runs against.
type Held<'m> = (SchemaChange, &'m SchemaSnapshot);

#[derive(Default)]
struct Pass {
    applied: Vec<Migration>,
    /// A table a pending step needs and that is not ready yet.
    waiting: Option<String>,
}

/// One locked run of the system chain for one scope.
struct Run<'a> {
    held: LockHeld,
    pool: &'a Pool,
    chain: &'a SystemChain,
    scope: MigrationScope,
    owned: &'a BTreeSet<String>,
    observer: Option<&'a dyn MigrationObserver>,
}

impl Run<'_> {
    /// The scope's system steps and the chain's file count, if any.
    fn wanted(&self) -> Result<Option<(Vec<Migration>, usize)>, MigrateError> {
        let dir = self.chain.dir();
        if !dir.is_dir() {
            return Ok(None);
        }
        let all = file::list_dir(dir)?;
        let total = all.len();
        let wanted: Vec<Migration> = all.into_iter().filter(|m| m.scope == self.scope).collect();
        Ok((!wanted.is_empty()).then_some((wanted, total)))
    }

    /// Apply pending steps in order. A step that needs a table the project
    /// chain owns but lacks, or that `busy` pending project migrations still
    /// write, waits with every later op it conflicts with; the rest run.
    async fn system_pass(&self, busy: &BTreeSet<String>) -> Result<Pass, MigrateError> {
        let mut pass = Pass::default();
        let Some((wanted, total)) = self.wanted()? else {
            return Ok(pass);
        };
        let declared: BTreeSet<String> = wanted.iter().flat_map(created).collect();
        let (mut live, mut absent) = (BTreeSet::new(), BTreeSet::new());
        for t in self.owned.intersection(&declared) {
            if runner::table_exists_here(self.pool, t).await {
                live.insert(t.clone());
            } else {
                absent.insert(t.clone());
            }
        }
        let ledger = runner::applied_set_pool_with_ledger(self.pool, runner::SYSTEM_LEDGER_TABLE)
            .await
            .unwrap_or_default();
        // Tables a waiting step's creations already made (#2083); a
        // regenerated chain's names miss the ledger, and its runner converges.
        let mut early = BTreeSet::new();
        if self.chain.origin() == runner::ChainOrigin::OnDisk {
            let pending = wanted.iter().filter(|m| !ledger.contains(&m.name));
            for t in pending.flat_map(created) {
                if !self.owned.contains(&t) && runner::table_exists_here(self.pool, &t).await {
                    early.insert(t);
                }
            }
        }
        if live.is_empty() && absent.is_empty() && early.is_empty() && busy.is_disjoint(&declared) {
            pass.applied = if wanted.len() == total {
                self.apply_dir(self.chain.dir()).await?
            } else {
                self.apply(&wanted).await?
            };
            return Ok(pass);
        }

        let skip: Vec<String> = live.iter().chain(&absent).cloned().collect();
        // Missing tables block any reference; a live owned one only its writes.
        let missing: Vec<&String> = absent
            .iter()
            .chain(busy.iter().filter(|t| !self.owned.contains(*t)))
            .collect();
        let in_use: Vec<&String> = live.iter().filter(|t| busy.contains(*t)).collect();
        let mut held: Vec<Held<'_>> = Vec::new();
        let waits = |c: &SchemaChange, snap: &SchemaSnapshot, held: &[Held<'_>]| {
            let missing = missing.iter().find(|t| c.touches(t, snap));
            let in_use = || in_use.iter().find(|t| op_writes(c, t));
            missing.or_else(in_use).map(|t| (*t).clone()).or_else(|| {
                held.iter()
                    .find(|(d, at)| conflicts(c, snap, d, at))
                    .map(|(d, _)| d.table().to_owned())
            })
        };
        let mut steps: Vec<Migration> = Vec::new();
        let mut before: Option<&SchemaSnapshot> = None;
        for m in &wanted {
            let mut step = runner::without_tables(m, &skip);
            if !ledger.contains(&m.name) {
                without_made(self.pool, &mut step, &live, &early).await?;
                let blocked = schema_ops(&step).find_map(|c| waits(c, &m.snapshot, &held));
                if let Some(t) = blocked {
                    pass.waiting.get_or_insert(t);
                    let mut groups =
                        early_creations(&step, |c| waits(c, &m.snapshot, &held).is_some());
                    for g in &mut groups {
                        if runner::table_exists_here(self.pool, g[0].table()).await {
                            g.clear();
                        }
                    }
                    groups.retain(|g| !g.is_empty());
                    held.extend(schema_ops(&step).map(|c| (c.clone(), &m.snapshot)));
                    if !groups.is_empty() {
                        pass.applied.extend(self.apply(&steps).await?);
                        converge(self.pool, &m.snapshot, groups).await?;
                    }
                    before = Some(&m.snapshot);
                    continue;
                }
                // A table the project chain still writes converges in `finish`.
                let on: Vec<String> = live
                    .iter()
                    .filter(|t| !busy.contains(*t) && touches(&step, t))
                    .cloned()
                    .collect();
                if !on.is_empty() {
                    // Run what precedes, then give `on` the columns this step expects.
                    pass.applied.extend(self.apply(&steps).await?);
                    if let Some(snap) = before {
                        let mut groups = missing_columns(self.pool, snap, &on).await?;
                        // A table the step only references needs just the FK targets.
                        groups.retain(|g| match g.first() {
                            Some(SchemaChange::AddColumn { table, column }) => {
                                writes(&step, table) || fk_targets(&step, table).contains(column)
                            }
                            _ => true,
                        });
                        converge(self.pool, snap, groups).await?;
                    }
                    for t in &on {
                        let have = ensure::live_columns(self.pool, t).await?;
                        step.forward.retain(|op| {
                            !matches!(op, Operation::Schema(SchemaChange::AddColumn { table, column })
                                if table == t && have.contains(column))
                        });
                    }
                }
            }
            before = Some(&m.snapshot);
            steps.push(step);
        }
        pass.applied.extend(self.apply(&steps).await?);
        Ok(pass)
    }

    /// After both chains: owned tables get the columns and later indexes they
    /// still lack, and framework tables the project created and later dropped are recreated.
    async fn finish(&self, claimed: &BTreeSet<String>) -> Result<(), MigrateError> {
        let Some((wanted, _)) = self.wanted()? else {
            return Ok(());
        };
        let Some(last) = wanted.last() else {
            return Ok(());
        };
        let snap = &last.snapshot;
        let declared: BTreeSet<String> = wanted.iter().flat_map(created).collect();
        let mut live = Vec::new();
        let mut groups = Vec::new();
        let (mut remade, mut junctions) = (BTreeSet::new(), Vec::new());
        for t in &declared {
            let exists = runner::table_exists_here(self.pool, t).await;
            if self.owned.contains(t) {
                if exists {
                    live.push(t.clone());
                }
            } else if claimed.contains(t) && !exists && snap.table(t).is_some() {
                let indexes = snap.indexes.iter().filter(|i| &i.table == t);
                groups.push(
                    std::iter::once(SchemaChange::CreateTable(t.clone()))
                        .chain(indexes.map(super::diff::create_index))
                        .collect(),
                );
                remade.insert(t.clone());
            } else if let Some(m2m) = snap.m2m_table(t).filter(|_| claimed.contains(t) && !exists) {
                // After the tables, which its FKs name (#2084).
                junctions.push(vec![super::diff::create_m2m(m2m)]);
            }
        }
        groups.extend(junctions);
        groups.extend(lost_fks(self.pool, snap, &remade).await?);
        groups.extend(missing_columns(self.pool, snap, &live).await?);
        // `without_tables` strips owned tables' indexes; add the ones a later step declares.
        let with_table = indexes_made_with_table(&wanted);
        groups.extend(
            snap.indexes
                .iter()
                .filter(|i| live.contains(&i.table) && !with_table.contains(i.name.as_str()))
                .map(|i| vec![super::diff::create_index(i)]),
        );
        converge(self.pool, snap, groups).await
    }

    /// Run `steps` from a scratch dir; applied ones are skipped by the ledger.
    async fn apply(&self, steps: &[Migration]) -> Result<Vec<Migration>, MigrateError> {
        if steps.is_empty() {
            return Ok(Vec::new());
        }
        let scratch = Scratch::write(steps)?;
        self.apply_dir(&scratch.0).await
    }

    async fn apply_dir(&self, dir: &Path) -> Result<Vec<Migration>, MigrateError> {
        runner::migrate_system_chain(self.held, self.pool, self.chain, dir, self.observer).await
    }
}

/// The tables, with their indexes, a waiting `step` creates and that need
/// nothing waiting: a pending project FK to one must not fail (#2083).
/// Everything else in the step waits, and so does a table any of it writes.
fn early_creations(
    step: &Migration,
    blocked: impl Fn(&SchemaChange) -> bool,
) -> Vec<Vec<SchemaChange>> {
    // Opaque ops may depend on anything before them.
    if step
        .forward
        .iter()
        .any(|op| !matches!(op, Operation::Schema(_)))
    {
        return Vec::new();
    }
    let snap = &step.snapshot;
    let (mut waiting, mut ready): (Vec<&SchemaChange>, Vec<&SchemaChange>) = schema_ops(step)
        .partition(|c| {
            let creates = matches!(
                c,
                SchemaChange::CreateTable(_)
                    | SchemaChange::CreateM2MTable { .. }
                    | SchemaChange::CreateIndex { .. }
            );
            !creates || blocked(c)
        });
    // To a fixpoint: a child may come before the parent it waits on.
    loop {
        let (stuck, free): (Vec<&SchemaChange>, Vec<&SchemaChange>) = ready
            .iter()
            .partition(|c| waiting.iter().any(|d| conflicts(c, snap, d, snap)));
        if stuck.is_empty() {
            break;
        }
        waiting.extend(stuck);
        ready = free;
    }
    let tables = ready.iter().filter_map(|c| match c {
        SchemaChange::CreateTable(t) | SchemaChange::CreateM2MTable { through: t, .. } => Some(t),
        _ => None,
    });
    tables
        .filter(|t| !waiting.iter().any(|d| op_writes(d, t)))
        .map(|t| {
            ready
                .iter()
                .filter(|c| c.table() == t.as_str())
                .map(|c| (*c).clone())
                .collect()
        })
        .collect()
}

/// `step` without what the database already shows: a drop of an index the
/// owned tables `live` lack, as `without_tables` never made it and MySQL has
/// no `DROP INDEX IF EXISTS` (#2094); and the `early` tables a waiting step
/// made, with their indexes that exist. A later index on them still runs.
async fn without_made(
    pool: &Pool,
    step: &mut Migration,
    live: &BTreeSet<String>,
    early: &BTreeSet<String>,
) -> Result<(), MigrateError> {
    let schema = ensure::creation_schema(pool).await?.unwrap_or_default();
    let mut made = BTreeSet::new();
    for (i, c) in schema_ops(step).enumerate() {
        let skip = match c {
            SchemaChange::DropIndex { name, table } if live.contains(table) => {
                !super::inspectdb::index_exists(pool, &schema, table, name).await?
            }
            SchemaChange::CreateIndex { name, table, .. } if early.contains(table) => {
                super::inspectdb::index_exists(pool, &schema, table, name).await?
            }
            SchemaChange::CreateTable(t) | SchemaChange::CreateM2MTable { through: t, .. } => {
                early.contains(t)
            }
            _ => false,
        };
        if skip {
            made.insert(i);
        }
    }
    let mut i = 0;
    step.forward.retain(|op| {
        let Operation::Schema(_) = op else {
            return true;
        };
        i += 1;
        !made.contains(&(i - 1))
    });
    Ok(())
}

/// The FKs into `remade` that live tables lost: PG's `DROP TABLE … CASCADE`
/// takes them with the table it recreates (#2084).
async fn lost_fks(
    pool: &Pool,
    snapshot: &SchemaSnapshot,
    remade: &BTreeSet<String>,
) -> Result<Vec<Vec<SchemaChange>>, MigrateError> {
    let schema = ensure::creation_schema(pool).await?.unwrap_or_default();
    let mut groups = Vec::new();
    for t in &snapshot.tables {
        let into = |to: &String| remade.contains(to);
        let wants = t
            .fields
            .iter()
            .any(|f| f.fk.as_ref().is_some_and(|r| into(&r.to)))
            || t.composite_fks.iter().any(|c| into(&c.to));
        if !wants || remade.contains(&t.name) || !runner::table_exists_here(pool, &t.name).await {
            continue;
        }
        let have = super::inspectdb::list_fks(pool, &schema, &t.name).await?;
        let has = |column: &str, to: &str| have.iter().any(|(c, r)| c == column && r == to);
        for f in &t.fields {
            if let Some(r) =
                f.fk.as_ref()
                    .filter(|r| into(&r.to) && !has(&f.column, &r.to))
            {
                if !orphans_allow(pool, &t.name, &f.column, &r.to).await? {
                    continue;
                }
                // Rendered as the FK it adds; the live column has none to drop.
                groups.push(vec![SchemaChange::AlterFkOnDelete {
                    table: t.name.clone(),
                    column: f.column.clone(),
                    from: r.on_delete.clone(),
                    to: r.on_delete.clone(),
                }]);
            }
        }
        for c in &t.composite_fks {
            let Some(first) = c.from.first() else {
                continue;
            };
            if into(&c.to)
                && !has(first, &c.to)
                && orphans_allow(pool, &t.name, first, &c.to).await?
            {
                groups.push(vec![super::diff::add_composite_fk(&t.name, c)]);
            }
        }
    }
    Ok(groups)
}

/// Whether an FK on `table.column` into the just-recreated, empty `to` can be
/// added: rows that point at the old table would make it fail on every run.
async fn orphans_allow(
    pool: &Pool,
    table: &str,
    column: &str,
    to: &str,
) -> Result<bool, MigrateError> {
    if ensure::has_no_values(pool, table, Some(column)).await? {
        return Ok(true);
    }
    tracing::warn!(
        target: "rustango::migrate",
        "`{table}.{column}` points at rows the recreated `{to}` lacks; its FK is not restored"
    );
    Ok(false)
}

/// An AddColumn for each column `snapshot` gives `tables` and the database lacks.
async fn missing_columns(
    pool: &Pool,
    snapshot: &SchemaSnapshot,
    tables: &[String],
) -> Result<Vec<Vec<SchemaChange>>, MigrateError> {
    let mut groups = Vec::new();
    for t in tables {
        let Some(snap) = snapshot.table(t) else {
            continue;
        };
        let have = ensure::live_columns(pool, t).await?;
        groups.extend(
            snap.fields
                .iter()
                .filter(|f| !have.contains(&f.column))
                .map(|f| {
                    vec![SchemaChange::AddColumn {
                        table: t.clone(),
                        column: f.column.clone(),
                    }]
                }),
        );
    }
    Ok(groups)
}

/// Apply `groups` against `snapshot`, failing with all that could not be added.
async fn converge(
    pool: &Pool,
    snapshot: &SchemaSnapshot,
    groups: Vec<Vec<SchemaChange>>,
) -> Result<(), MigrateError> {
    if groups.is_empty() {
        return Ok(());
    }
    let failed = ensure::converge_groups(pool, snapshot, &groups).await?;
    if failed.is_empty() {
        return Ok(());
    }
    Err(MigrateError::Validation(format!(
        "the framework schema is missing objects `migrate` cannot add; \
         add them by hand, then rerun `migrate`:\n  - {}",
        failed.join("\n  - ")
    )))
}

/// A temp dir holding the steps to run, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn write(steps: &[Migration]) -> Result<Self, MigrateError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rustango_system_scoped_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir)?;
        let scratch = Self(dir);
        let mut prev: Option<String> = None;
        for step in steps {
            // A waiting step is left out, so link each to the one before it here.
            let step = Migration {
                prev: prev.replace(step.name.clone()),
                ..step.clone()
            };
            file::write(&scratch.0.join(format!("{}.json", step.name)), &step)?;
        }
        Ok(scratch)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn mig(name: &str, prev: Option<&str>, snapshot: Value, ops: Vec<Value>) -> Migration {
        let forward: Vec<Value> = ops.into_iter().map(|op| json!({ "schema": op })).collect();
        serde_json::from_value(json!({
            "name": name, "created_at": "2026-01-01T00:00:00Z", "prev": prev,
            "snapshot": snapshot, "forward": forward,
        }))
        .unwrap()
    }

    fn table(name: &str, fk_to: Option<&str>) -> Value {
        let mut fields = vec![json!({ "name": "id", "column": "id", "ty": "i64",
                                      "nullable": false, "primary_key": true })];
        if let Some(to) = fk_to {
            fields.push(json!({ "name": "p_id", "column": "p_id", "ty": "i64", "nullable": true,
                                "primary_key": false, "fk": { "kind": "fk", "to": to, "on": "id" } }));
        }
        json!({ "name": name, "model": name, "fields": fields })
    }

    /// A child listed before the waiting parent it references waits too.
    #[test]
    fn early_creations_wait_for_a_later_parent() {
        let step = mig(
            "0001",
            None,
            json!({ "tables": [table("a_child", Some("z_parent")), table("z_parent", None)] }),
            vec![
                json!({ "CreateTable": "a_child" }),
                json!({ "CreateTable": "z_parent" }),
            ],
        );
        let parent = SchemaChange::CreateTable("z_parent".into());
        assert!(early_creations(&step, |c| *c == parent).is_empty());
    }

    fn write(dir: &Path, m: &Migration) {
        std::fs::create_dir_all(dir).unwrap();
        file::write(&dir.join(format!("{}.json", m.name)), m).unwrap();
    }

    /// #2084 — a framework junction the project created and dropped comes back.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn dropped_junction_is_recreated() {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite:{}?mode=rwc", tmp.path().join("db.sqlite").display());
        let pool = Pool::connect(&url).await.unwrap();
        let m2m = json!({ "through": "sc_links", "src_table": "sc_parent", "src_col": "from_id",
                          "dst_table": "sc_parent", "dst_col": "to_id" });
        let snap = json!({ "tables": [table("sc_parent", None)], "m2m_tables": [m2m.clone()] });
        let ops = vec![
            json!({ "CreateTable": "sc_parent" }),
            json!({ "CreateM2MTable": m2m }),
        ];
        let sys = tmp.path().join("system/migrations");
        write(&sys, &mig("0001_sys", None, snap.clone(), ops.clone()));
        let dir = tmp.path().join("migrations");
        write(&dir, &mig("0001_initial", None, snap, ops));
        let chain = SystemChain::shipped(sys);
        let run = |pool: Pool, dir: PathBuf, chain: SystemChain| async move {
            migrate_chains(
                &pool,
                &chain,
                MigrationScope::Tenant,
                &dir,
                None,
                Signals::Skip,
                |held| runner::migrate_pool_locked(held, &pool, &dir, None),
            )
            .await
            .map(drop)
        };
        run(pool.clone(), dir.clone(), chain.clone()).await.unwrap();
        let drop_links = json!({ "DropM2MTable": { "through": "sc_links" } });
        let after = json!({ "tables": [table("sc_parent", None)] });
        write(
            &dir,
            &mig("0002_drop", Some("0001_initial"), after, vec![drop_links]),
        );
        run(pool.clone(), dir, chain).await.unwrap();
        assert!(runner::table_exists_here(&pool, "sc_links").await);
    }
}
