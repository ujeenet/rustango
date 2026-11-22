//! SPIKE S4: the v2 tenant migrate connection (`open(Refuse)`, no `public`),
//! per-tenant locks from `LockScope`, and a fleet run where one tenant's
//! failure is recorded and does not stop the others (design 14.2). Throwaway.

use super::lock::{try_lock, Held, LockScope};
use super::tenant_sql::{check_placeholders, scan, Allowed};
use crate::core::SqlValue;
use crate::sql::{raw_execute_pool, raw_execute_tx, raw_query_pool, Pool};

/// One tenant migration file, reduced to what S4 needs.
#[derive(Debug, Clone)]
pub struct TenantFile {
    pub id: String,
    pub sql: Vec<String>,
    /// Extensions whose objects the file uses (from state: citext columns, trgm opclasses).
    pub needs_extensions: Vec<String>,
}

impl TenantFile {
    #[must_use]
    pub fn new(id: &str, sql: &[&str]) -> Self {
        Self {
            id: id.into(),
            sql: sql.iter().map(|s| (*s).to_owned()).collect(),
            needs_extensions: Vec::new(),
        }
    }

    #[must_use]
    pub fn needs(mut self, ext: &str) -> Self {
        self.needs_extensions.push(ext.into());
        self
    }
}

/// Load-time checks on every file: the qualifier scan and placeholders.
///
/// # Errors
/// The first `MV2-E031` / `MV2-E032`, prefixed with the file id.
pub fn load(files: &[TenantFile], allowed: &Allowed, tenant_tables: &[&str]) -> Result<(), String> {
    for f in files {
        for s in &f.sql {
            scan(s, allowed).map_err(|e| format!("{}: {e}", f.id))?;
            check_placeholders(s, tenant_tables).map_err(|e| format!("{}: {e}", f.id))?;
        }
    }
    Ok(())
}

/// The v2 tenant connection of a schema tenant: a one-connection pool whose
/// session runs `open(Refuse)`'s `SET search_path`, and the path it got.
///
/// Refuses when a needed extension is not installed, or sits in a schema the
/// path leaves off (`public` with tables): unqualified it would fail, and
/// qualified its operators silently fall back to `text` ones.
///
/// # Errors
/// The path refusal, or the unreachable extension with the fix.
#[cfg(all(feature = "postgres", feature = "tenancy"))]
pub async fn open_tenant(
    registry: &sqlx::PgPool,
    schema: &str,
    needs: &[String],
) -> Result<(Pool, crate::tenancy::spike_v2::V2Path), String> {
    let path = crate::tenancy::spike_v2::open_refuse(registry, schema)
        .await
        .map_err(|e| e.to_string())?;
    for ext in needs {
        let home: Option<String> = sqlx::query_scalar(
            "SELECT n.nspname::text FROM pg_catalog.pg_extension e \
             JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = $1",
        )
        .bind(ext)
        .fetch_optional(registry)
        .await
        .map_err(|e| e.to_string())?;
        match home {
            None => {
                return Err(format!(
                    "extension `{ext}` is not installed; the registry chain installs it \
                     into the extension schema"
                ))
            }
            Some(h) if !path.schemas.contains(&h) => {
                return Err(format!(
                    "extension `{ext}` is in `{h}`, which holds tables, so tenant `{schema}` \
                     cannot reach it; run `ALTER EXTENSION {ext} SET SCHEMA extensions`"
                ))
            }
            Some(_) => {}
        }
    }
    let set: std::sync::Arc<str> = path.set_sql.clone().into();
    let pg = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _| {
            let set = std::sync::Arc::clone(&set);
            Box::pin(async move {
                sqlx::query(&set).execute(conn).await?;
                Ok(())
            })
        })
        .connect_with((*registry.connect_options()).clone())
        .await
        .map_err(|e| e.to_string())?;
    Ok((Pool::Postgres(pg), path))
}

/// Wait up to `wait` for `scope`'s lock.
///
/// # Errors
/// A timeout, or the lock's driver error.
pub async fn lock(
    pool: &Pool,
    scope: &LockScope,
    wait: std::time::Duration,
) -> Result<Held, String> {
    let start = std::time::Instant::now();
    loop {
        if let Some(h) = try_lock(pool, scope).await? {
            return Ok(h);
        }
        if start.elapsed() > wait {
            return Err(format!("lock timeout on {scope:?}"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

const LEDGER: &str = "mv2_s4_ledger";

/// Apply pending `files` on a tenant `pool` (already scoped), one tx per file,
/// with an unqualified ledger in the tenant. Returns the ids applied.
///
/// # Errors
/// The failing file id, and the error class (never driver text).
pub async fn apply(pool: &Pool, files: &[TenantFile]) -> Result<Vec<String>, (String, String)> {
    let d = pool.dialect();
    let ledger = d.quote_ident(LEDGER);
    raw_execute_pool(
        pool,
        &format!("CREATE TABLE IF NOT EXISTS {ledger} (id VARCHAR(100) PRIMARY KEY)"),
        Vec::new(),
    )
    .await
    .map_err(|e| ("ledger".to_owned(), class(&e)))?;
    let done: Vec<(String,)> =
        raw_query_pool(&format!("SELECT id FROM {ledger}"), Vec::new(), pool)
            .await
            .map_err(|e| ("ledger".to_owned(), class(&e)))?;
    let mut applied = Vec::new();
    for f in files {
        if done.iter().any(|(id,)| *id == f.id) {
            continue;
        }
        let fail = |e: &crate::sql::ExecError| (f.id.clone(), class(e));
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| (f.id.clone(), class(&e)))?;
        let mut tx = conn.begin().await.map_err(|e| (f.id.clone(), class(&e)))?;
        for s in &f.sql {
            raw_execute_tx(&mut tx, s, Vec::new())
                .await
                .map_err(|e| fail(&e))?;
        }
        raw_execute_tx(
            &mut tx,
            &format!("INSERT INTO {ledger} (id) VALUES ({})", d.placeholder(1)),
            vec![SqlValue::String(f.id.clone())],
        )
        .await
        .map_err(|e| fail(&e))?;
        tx.commit().await.map_err(|e| (f.id.clone(), class(&e)))?;
        applied.push(f.id.clone());
    }
    Ok(applied)
}

/// An error's class: the SQLSTATE / server code, never the message.
fn class(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        // `ExecError::Driver` is `#[error(transparent)]`: its source skips the sqlx error.
        let driver = match err.downcast_ref::<crate::sql::ExecError>() {
            Some(crate::sql::ExecError::Driver(d)) => Some(d),
            _ => err.downcast_ref::<sqlx::Error>(),
        };
        if let Some(sqlx::Error::Database(db)) = driver {
            return format!("db:{}", db.code().unwrap_or_default());
        }
        cur = err.source();
    }
    "error".into()
}

/// One tenant of a fleet run: its lock scope and how to reach it.
pub struct FleetTenant {
    pub slug: String,
    pub scope: LockScope,
    /// The pool the lock connection is made from (registry DB for schema mode).
    pub lock_pool: Pool,
    /// The tenant's migrate pool, scoped (schema) or its own database.
    pub pool: Pool,
}

/// Per-tenant outcome of a fleet run.
#[derive(Debug, Default)]
pub struct FleetReport {
    pub applied: Vec<(String, Vec<String>)>,
    /// `(slug, file id, error class)`: what boot logs and the console lists.
    pub failed: Vec<(String, String, String)>,
}

/// Registry cache table (design 8.3, `__rustango_tenant_migrations__`).
pub const CACHE: &str = "mv2_s4_tenant_migrations";

/// Run `files` on every tenant, each under its own lock. A failure is
/// recorded per tenant in the registry cache and the run goes on (14.2).
///
/// # Errors
/// Only registry errors: a failed registry write stops the run.
pub async fn run_fleet(
    registry: &Pool,
    tenants: &[FleetTenant],
    files: &[TenantFile],
) -> Result<FleetReport, String> {
    let d = registry.dialect();
    let cache = d.quote_ident(CACHE);
    raw_execute_pool(
        registry,
        &format!(
            "CREATE TABLE IF NOT EXISTS {cache} (slug VARCHAR(100) PRIMARY KEY, \
             status VARCHAR(20) NOT NULL, file_id VARCHAR(100), error_class VARCHAR(40))"
        ),
        Vec::new(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut report = FleetReport::default();
    for t in tenants {
        let held = lock(&t.lock_pool, &t.scope, std::time::Duration::from_secs(10)).await;
        let outcome = match held {
            Ok(held) => {
                let r = apply(&t.pool, files).await;
                held.release().await;
                r
            }
            Err(_) => Err(("lock".to_owned(), "lock_timeout".to_owned())),
        };
        let (status, file, err) = match &outcome {
            Ok(_) => ("applied", None, None),
            Err((f, c)) => ("failed", Some(f.clone()), Some(c.clone())),
        };
        raw_execute_pool(
            registry,
            &format!("DELETE FROM {cache} WHERE slug = {}", d.placeholder(1)),
            vec![SqlValue::String(t.slug.clone())],
        )
        .await
        .map_err(|e| e.to_string())?;
        raw_execute_pool(
            registry,
            &format!(
                "INSERT INTO {cache} (slug, status, file_id, error_class) VALUES ({}, {}, {}, {})",
                d.placeholder(1),
                d.placeholder(2),
                d.placeholder(3),
                d.placeholder(4)
            ),
            vec![
                SqlValue::String(t.slug.clone()),
                SqlValue::String(status.into()),
                file.map_or(SqlValue::Null, SqlValue::String),
                err.map_or(SqlValue::Null, SqlValue::String),
            ],
        )
        .await
        .map_err(|e| e.to_string())?;
        match outcome {
            Ok(ids) => report.applied.push((t.slug.clone(), ids)),
            Err((f, c)) => report.failed.push((t.slug.clone(), f, c)),
        }
    }
    Ok(report)
}
