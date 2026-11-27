//! Testing data steps (5.7): a database at the state just before a step,
//! then up, down and the checks a step must pass.

use std::collections::BTreeMap;
use std::path::Path;

use super::format::Op;
use super::replay::{apply, apply_file, Ctx};
use super::state::{State, Target};
use super::step::{run_step, HistRow, Scope, StepCtx, StepFn};
use crate::sql::Pool;

/// One data step, on a database built at the state before it.
pub struct StepTest {
    pool: Pool,
    scope: Scope,
    up: StepFn,
    down: Option<StepFn>,
}

/// Build the tables of the project around `dir` as they are just before
/// the `data_rust` op `step_id` of `app`, on the empty database of `pool`,
/// and bind that op's registered step.
///
/// # Errors
/// No such op, no registered step, a load error, or the driver.
pub async fn at(pool: &Pool, dir: &Path, app: &str, step_id: &str) -> Result<StepTest, String> {
    let project = super::load::load_project(dir).map_err(|e| e.to_string())?;
    let ctx = Ctx {
        target: Target::Tenant,
        registry: None,
    };
    let order = project
        .order
        .get(&Target::Tenant)
        .cloned()
        .unwrap_or_default();
    let mut state = State::default();
    let mut found = None;
    'files: for i in order {
        let f = &project.files[i];
        if f.file.app == app {
            for (n, env) in f.file.operations.iter().enumerate() {
                if let Op::DataRust { id, tables, .. } = &env.op {
                    if id == step_id {
                        found = Some(tables.clone());
                        break 'files;
                    }
                }
                let at = format!("{}#/operations/{n}", f.at);
                apply(&mut state, &env.op, &at, &ctx, &[]).map_err(|e| e.to_string())?;
            }
            continue;
        }
        apply_file(&mut state, &f.file, &f.at, &ctx).map_err(|e| e.to_string())?;
    }
    let footprint = found.ok_or_else(|| format!("no data_rust op `{app}:{step_id}`"))?;
    let step = super::step::registered(app, step_id)
        .into_iter()
        .next()
        .ok_or_else(|| format!("no `#[data_step(app = \"{app}\", id = \"{step_id}\")]`"))?;
    let b = super::run::backend_of(pool);
    let tables = super::run::fk_order(&state, "testing").map_err(|e| e.to_string())?;
    let mut externs = State::default();
    externs.externs.clone_from(&state.externs);
    for u in
        super::render::render_install(&externs, &tables, "testing", b).map_err(|e| e.to_string())?
    {
        for s in &u.sql {
            crate::sql::raw_execute_pool(pool, s, Vec::new())
                .await
                .map_err(|e| format!("{s}: {e}"))?;
        }
    }
    let scope = Scope {
        app: app.to_owned(),
        tables: footprint
            .tables()
            .filter_map(|t| state.tables.get(t).map(|ts| (t.to_owned(), (**ts).clone())))
            .collect(),
        footprint,
        undo: None,
    };
    Ok(StepTest {
        pool: pool.clone(),
        scope,
        up: step.up,
        down: step.down,
    })
}

impl StepTest {
    /// The database's pool, to seed rows and check them.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    async fn unit(&self, f: StepFn, commit: bool) -> Result<(), String> {
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        conn.close_on_drop();
        let tx = conn.begin_write().await.map_err(|e| e.to_string())?;
        let (tx, r) = run_step(tx, self.scope.clone(), move |ctx: &mut StepCtx<'_>| {
            Box::pin(async move { f(ctx, super::step::Batch::whole()).await.map(drop) })
        })
        .await;
        match r {
            Ok(()) if commit => tx.commit().await.map_err(|e| e.to_string()),
            Ok(()) => tx.rollback().await.map_err(|e| e.to_string()),
            Err(e) => {
                let _ = tx.rollback().await;
                Err(e.to_string())
            }
        }
    }

    /// Run `up` in one unit, as the runner does.
    ///
    /// # Errors
    /// The step's error; the unit is rolled back.
    pub async fn up(&self) -> Result<(), String> {
        self.unit(self.up, true).await
    }

    /// Run `down` in one unit.
    ///
    /// # Errors
    /// No `down`, or the step's error.
    pub async fn down(&self) -> Result<(), String> {
        let f = self.down.ok_or("this step has no `down`")?;
        self.unit(f, true).await
    }

    /// Every footprint table's rows, by key.
    ///
    /// # Errors
    /// The driver, or a bad cell.
    pub async fn dump(&self) -> Result<BTreeMap<String, Vec<HistRow>>, String> {
        let mut conn = self.pool.acquire().await.map_err(|e| e.to_string())?;
        let tx = conn.begin().await.map_err(|e| e.to_string())?;
        let mut ctx = StepCtx::new(tx, self.scope.clone());
        let mut out = BTreeMap::new();
        for (name, t) in &self.scope.tables {
            let mut q = ctx.table(name).map_err(|e| e.to_string())?.query();
            for k in t
                .primary_key()
                .chain(t.columns.iter().map(|c| c.column.as_str()))
            {
                q = q.order_by(k, false);
            }
            let rows = q.fetch(&mut ctx).await.map_err(|e| e.to_string())?;
            out.insert(name.clone(), rows);
        }
        ctx.tx.rollback().await.map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// Up then down leaves the footprint tables as they were.
    ///
    /// # Errors
    /// A step error, or the rows differ.
    pub async fn assert_prefix_down(&self) -> Result<(), String> {
        let before = self.dump().await?;
        self.up().await?;
        self.down().await?;
        let after = self.dump().await?;
        if before == after {
            Ok(())
        } else {
            Err(format!(
                "down did not restore the rows:\nbefore {before:?}\nafter  {after:?}"
            ))
        }
    }

    /// A run cut off before its commit, then run again, ends where one
    /// full run ends.
    ///
    /// # Errors
    /// A step error, or the rows differ.
    pub async fn assert_resume_equals_full(&self) -> Result<(), String> {
        self.up().await?;
        let full = self.dump().await?;
        self.down().await?;
        // Cut off: the unit runs and is rolled back, as a crash before commit.
        self.unit(self.up, false).await?;
        self.up().await?;
        let resumed = self.dump().await?;
        if full == resumed {
            Ok(())
        } else {
            Err(format!(
                "a resumed run differs:\nfull    {full:?}\nresumed {resumed:?}"
            ))
        }
    }
}

/// A step context on `pool` over `tables`, every one read and written,
/// outside any run: to exercise [`super::HistTable`] directly. Finish it
/// with [`end`].
///
/// # Errors
/// A table type this build cannot read, or the driver.
pub async fn session(
    pool: &Pool,
    app: &str,
    tables: &[crate::migrate::snapshot::TableSnapshot],
) -> Result<StepCtx<'static>, String> {
    let b = super::run::backend_of(pool);
    let snap = crate::migrate::snapshot::SchemaSnapshot::default();
    let mut scope = Scope {
        app: app.to_owned(),
        ..Scope::default()
    };
    for t in tables {
        let ts = super::make::table_of(t, &snap, &[b], "testing").map_err(|e| e.to_string())?;
        scope.footprint.read.push(t.name.clone());
        scope.footprint.write.push(t.name.clone());
        scope.tables.insert(t.name.clone(), ts);
    }
    let tx = crate::sql::transaction_pool(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(StepCtx::new(tx, scope))
}

/// Commit or roll back a [`session`].
///
/// # Errors
/// The driver.
pub async fn end(mut ctx: StepCtx<'static>, commit: bool) -> Result<(), String> {
    if commit {
        // As at a unit's end: PG sequences move past explicit auto keys.
        ctx.reset_sequences().await.map_err(|e| e.to_string())?;
        ctx.tx.commit().await.map_err(|e| e.to_string())
    } else {
        ctx.tx.rollback().await.map_err(|e| e.to_string())
    }
}
