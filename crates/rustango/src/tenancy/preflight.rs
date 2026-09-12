//! Reaching a tenant's database *before* anything is written to the
//! registry.
//!
//! `create-tenant` used to take `--database-url` on faith: it inserted
//! the `Org` row and only then ran migrations against whatever the URL
//! pointed at. A typo'd host, a wrong password, or a database that did
//! not exist yet left a **half-provisioned tenant** — a live registry
//! row the resolver will happily match, in front of a database with no
//! schema in it.
//!
//! (Schema-mode dodged this by accident: it runs `CREATE SCHEMA` before
//! the `INSERT`, so a failed insert orphans nothing. Database-mode had
//! no equivalent.)
//!
//! ## Why `SELECT 1` is not enough
//!
//! A tenant database this role can *read* but not `CREATE TABLE` in
//! passes a connect-and-ping check and then dies twenty migrations
//! later, having already been registered. So the probe optionally
//! creates a table and drops it — the only check that actually proves
//! the thing migrations are about to need.
//!
//! That is a write, so it is a flag. It defaults **on** for
//! provisioning (where a migration run is seconds away and a table is
//! nothing next to it) and can be turned off for a read-only look.

use std::time::Duration;

use crate::sql::connect_diagnosis::{redact, ConnectDiagnosis, ConnectFault};
use crate::sql::Pool;

/// How long to wait for the database to answer.
///
/// Deliberately **not** the pool's `RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS`
/// (5s by default). That value is tuned for a request path, where
/// failing fast protects the server. A pre-flight is an operator
/// standing at a form waiting for an answer, often against a cold or
/// distant database, and a false "unreachable" is worse than a wait.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// What to check.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub timeout: Duration,
    /// Create and drop a table, proving this role can do what
    /// migrations will need. See the module docs.
    pub probe_writes: bool,
}

impl Default for Preflight {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            probe_writes: true,
        }
    }
}

impl Preflight {
    /// Connect and ping only — no writes.
    #[must_use]
    pub fn read_only() -> Self {
        Self {
            probe_writes: false,
            ..Self::default()
        }
    }
}

/// What a successful pre-flight found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reachable {
    /// Where we reached, password removed.
    pub endpoint: String,
    /// Whether the write probe ran and passed. `false` means it was
    /// switched off, not that it failed — a failure is an `Err`.
    pub writes_verified: bool,
}

/// Reach `url` and report whether it is usable as a tenant database.
///
/// # Errors
/// A [`ConnectDiagnosis`] naming the fault and what to change. Never a
/// raw driver string: the whole point is that "connection refused on
/// :5432" and "database `acme` does not exist" and "role `app` cannot
/// create tables" are three different operator actions.
pub async fn check(url: &str, opts: &Preflight) -> Result<Reachable, ConnectDiagnosis> {
    // A dedicated pool with its own timeout, closed before returning —
    // never the shared one. A probe that leaves a pool registered for a
    // tenant that then fails to provision is a leak, and one that
    // borrows the request path's 5s timeout reports healthy databases
    // as unreachable.
    let pool = Pool::connect_diagnosed(url, opts.timeout).await?;

    let result = probe(&pool, url, opts).await;
    // Close explicitly rather than letting the drop handler get to it
    // whenever: this function is called in a loop by an operator
    // retyping a URL, and each attempt should release its socket now.
    pool.close().await;
    result
}

async fn probe(pool: &Pool, url: &str, opts: &Preflight) -> Result<Reachable, ConnectDiagnosis> {
    // A real round-trip, not just a handshake: sqlx can hand back a
    // pooled connection that has not spoken to the server since it was
    // opened, so "connected" alone proves less than it looks.
    raw(pool, "SELECT 1").await.map_err(|e| diagnose(url, &e))?;

    if opts.probe_writes {
        verify_writes(pool, url).await?;
    }

    Ok(Reachable {
        endpoint: redact(url),
        writes_verified: opts.probe_writes,
    })
}

/// One raw statement against whichever backend this is.
async fn raw(pool: &Pool, sql: &str) -> Result<(), crate::sql::ExecError> {
    crate::sql::raw_execute_pool(pool, sql, Vec::new()).await?;
    Ok(())
}

/// Classify an `ExecError` the same way a connect failure is
/// classified, by reaching the `sqlx::Error` it wraps.
fn diagnose(url: &str, e: &crate::sql::ExecError) -> ConnectDiagnosis {
    match e {
        crate::sql::ExecError::Driver(inner) => ConnectDiagnosis::of(url, inner),
        other => ConnectDiagnosis::new(ConnectFault::Other, url, other.to_string()),
    }
}

/// Create a table and drop it.
///
/// The name is unique per process *and* per call: two pre-flights
/// running at once against one database — an operator on the console
/// while a webhook provisions, say — must not collide, or a perfectly
/// good database gets reported as broken.
async fn verify_writes(pool: &Pool, url: &str) -> Result<(), ConnectDiagnosis> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let table = format!(
        "rustango_preflight_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );

    if let Err(e) = raw(pool, &format!("CREATE TABLE {table} (id INTEGER)")).await {
        // A failed CREATE here is almost always a grant problem, but
        // the driver's code is what decides — a disk-full or a
        // read-only replica is not a permissions story and should not
        // be reported as one.
        let d = diagnose(url, &e);
        return Err(match d.fault {
            ConnectFault::Other => {
                ConnectDiagnosis::new(ConnectFault::PermissionDenied, url, d.detail)
            }
            _ => d,
        });
    }

    // Best-effort: the grant question is already answered, and leaving
    // a one-column table behind is a smaller problem than failing a
    // pre-flight that actually passed.
    let _ = raw(pool, &format!("DROP TABLE {table}")).await;
    Ok(())
}
