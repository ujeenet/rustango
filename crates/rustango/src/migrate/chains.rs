//! The one order every entry point applies the framework's system chain and
//! the project's own chain in: `manage migrate`, the `Cli` auto-migrate and
//! the tenancy runners all go through [`migrate_chains`].

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};

use super::diff::SchemaChange;
use super::file::{self, Migration, MigrationScope, Operation};
use super::make::SystemChain;
use super::progress::MigrationObserver;
use super::{ensure, runner, MigrateError};
use crate::sql::Pool;

/// What each chain applied.
pub(crate) struct Applied {
    pub(crate) system: Vec<Migration>,
    pub(crate) project: Vec<Migration>,
}

/// Apply `chain`'s `scope` steps, then the project chain via `project`, then
/// the system steps that waited for a table the project chain creates.
///
/// A project scaffolded before the system chain creates framework tables in
/// its own migrations. Those tables stay the project's: the system chain
/// never creates them, waits for them, and then adds the columns they lack.
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
    project: F,
) -> Result<Applied, MigrateError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<Migration>, MigrateError>>,
{
    let owned = project_tables(project_dir);
    // Boxed: inline, these futures overflow the stack of deep `manage` callers.
    let first = Box::pin(system_pass(pool, chain, scope, &owned, observer)).await?;
    let project = Box::pin(project()).await?;
    let mut system = first.applied;
    if first.again {
        let second = Box::pin(system_pass(pool, chain, scope, &owned, observer)).await?;
        if let Some(table) = second.waiting {
            return Err(MigrateError::Validation(format!(
                "a system migration needs `{table}`, which the project's migrations \
                 claim but did not create"
            )));
        }
        system.extend(second.applied);
    }
    Ok(Applied { system, project })
}

/// Every table the project's migrations create, applied or not.
fn project_tables(dir: &Path) -> HashSet<String> {
    file::list_dir(dir)
        .unwrap_or_default()
        .iter()
        .flat_map(created)
        .collect()
}

fn created(m: &Migration) -> impl Iterator<Item = String> + '_ {
    m.forward.iter().filter_map(|op| match op {
        Operation::Schema(
            SchemaChange::CreateTable(t) | SchemaChange::CreateM2MTable { through: t, .. },
        ) => Some(t.clone()),
        _ => None,
    })
}

#[derive(Default)]
struct Pass {
    applied: Vec<Migration>,
    /// An owned table a pending step needs and the database lacks.
    waiting: Option<String>,
    /// Some owned table was missing, so run again after the project chain.
    again: bool,
}

async fn system_pass(
    pool: &Pool,
    chain: &SystemChain,
    scope: MigrationScope,
    project_owned: &HashSet<String>,
    observer: Option<&dyn MigrationObserver>,
) -> Result<Pass, MigrateError> {
    let mut pass = Pass::default();
    let system_dir = chain.dir();
    if !system_dir.is_dir() {
        return Ok(pass);
    }
    let all = file::list_dir(system_dir)?;
    let wanted: Vec<&Migration> = all.iter().filter(|m| m.scope == scope).collect();
    if wanted.is_empty() {
        return Ok(pass);
    }
    let declared: HashSet<String> = wanted.iter().flat_map(|m| created(m)).collect();
    let (mut live, mut absent) = (Vec::new(), Vec::new());
    for t in project_owned.intersection(&declared) {
        if runner::table_exists_here(pool, t).await {
            live.push(t.clone());
        } else {
            absent.push(t.clone());
        }
    }
    pass.again = !absent.is_empty();
    let owned: Vec<String> = live.iter().chain(&absent).cloned().collect();

    let steps: Vec<Migration> = if owned.is_empty() {
        wanted.iter().map(|m| (*m).clone()).collect()
    } else {
        let applied = runner::applied_set_pool_with_ledger(pool, runner::SYSTEM_LEDGER_TABLE)
            .await
            .unwrap_or_default();
        let mut steps = Vec::new();
        for m in &wanted {
            let step = without_owned(m, &owned);
            if !applied.contains(&m.name) {
                let needs = |t: &&String| {
                    step.forward.iter().any(
                        |op| matches!(op, Operation::Schema(c) if c.touches(t, &step.snapshot)),
                    )
                };
                if let Some(t) = absent.iter().find(needs) {
                    // Order matters: everything after it waits too.
                    pass.waiting = Some(t.clone());
                    break;
                }
            }
            steps.push(step);
        }
        steps
    };

    let scratch = if owned.is_empty() && wanted.len() == all.len() {
        None
    } else {
        Some(Scratch::write(&steps)?)
    };
    let run_dir = scratch.as_ref().map_or(system_dir, |s| s.0.as_path());
    pass.applied = runner::migrate_system_chain(pool, chain, run_dir, observer).await?;
    drop(scratch);

    // Every run, so a column that failed once is retried.
    if let Some(last) = steps.last() {
        converge_columns(pool, &last.snapshot, &live).await?;
    }
    Ok(pass)
}

/// `mig` without what creates or extends a project-owned table; columns come
/// from [`converge_columns`] instead.
fn without_owned(mig: &Migration, owned: &[String]) -> Migration {
    let mut step = runner::without_tables(mig, owned);
    step.forward.retain(|op| {
        !matches!(op, Operation::Schema(SchemaChange::AddColumn { table, .. }) if owned.contains(table))
    });
    step
}

/// Add the columns `snapshot` gives `tables` and the database lacks.
async fn converge_columns(
    pool: &Pool,
    snapshot: &super::SchemaSnapshot,
    tables: &[String],
) -> Result<(), MigrateError> {
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
        for step in steps {
            file::write(&scratch.0.join(format!("{}.json", step.name)), step)?;
        }
        Ok(scratch)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
