//! SPIKE S4: the migrate lock key built from the target (`LockScope`), never
//! from session state, and computed the way v1 computes it so both exclude
//! each other. Throwaway.

use crate::sql::Pool;

/// What a migrate run locks. The key comes from this, not from the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockScope {
    /// The registry chain, on the registry database (name for MySQL, file for SQLite).
    Registry(String),
    /// A whole database: a single-database project or a DB-mode tenant.
    Database(String),
    /// A schema-mode tenant inside the registry database (PG only).
    Schema { database: String, schema: String },
}

impl LockScope {
    /// PG advisory key. Locks are per database, so the database name is not mixed in.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn pg_key(&self) -> i64 {
        match self {
            Self::Registry(_) | Self::Database(_) => {
                crate::migrate::runner::lock_key_for_schema(None)
            }
            Self::Schema { schema, .. } => {
                crate::migrate::runner::lock_key_for_schema(Some(schema))
            }
        }
    }

    /// The database whose name or file the lock is tied to.
    #[must_use]
    pub fn database(&self) -> &str {
        match self {
            Self::Registry(d) | Self::Database(d) => d,
            Self::Schema { database, .. } => database,
        }
    }
}

/// v1's PG key for `pool`: `current_schema()` of a pool connection.
///
/// # Errors
/// Driver errors.
#[cfg(feature = "postgres")]
pub async fn v1_pg_key(pool: &sqlx::PgPool) -> Result<i64, sqlx::Error> {
    crate::migrate::runner::pg_migrate_lock_key(pool).await
}

/// A held v2 lock: its own connection, released when closed or dropped.
pub enum Held {
    #[cfg(feature = "postgres")]
    Pg(sqlx::PgConnection),
    #[cfg(feature = "mysql")]
    Mysql(sqlx::MySqlConnection),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqliteConnection),
}

impl Held {
    pub async fn release(self) {
        use sqlx::Connection as _;
        match self {
            #[cfg(feature = "postgres")]
            Self::Pg(c) => drop(c.close().await),
            #[cfg(feature = "mysql")]
            Self::Mysql(c) => drop(c.close().await),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(c) => drop(c.close().await),
        }
    }
}

/// Take `scope`'s lock without waiting, on a new connection made from `pool`'s
/// options (PG: `pool` must be on the scope's database). `None` when held elsewhere.
///
/// # Errors
/// Connect or driver errors, and a PG pool on another database.
pub async fn try_lock(pool: &Pool, scope: &LockScope) -> Result<Option<Held>, String> {
    use sqlx::ConnectOptions as _;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut conn = pg
                .connect_options()
                .connect()
                .await
                .map_err(|e| e.to_string())?;
            let db: String = sqlx::query_scalar("SELECT current_database()::text")
                .fetch_one(&mut conn)
                .await
                .map_err(|e| e.to_string())?;
            if db != scope.database() {
                return Err(format!(
                    "lock for `{}` asked on a connection to `{db}`",
                    scope.database()
                ));
            }
            let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(scope.pg_key())
                .fetch_one(&mut conn)
                .await
                .map_err(|e| e.to_string())?;
            Ok(got.then_some(Held::Pg(conn)))
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            // The name v1 builds from DATABASE(), here from the bound target name.
            let mut conn = my
                .connect_options()
                .connect()
                .await
                .map_err(|e| e.to_string())?;
            let got: Option<i64> = sqlx::query_scalar("SELECT GET_LOCK(CONCAT(?, SHA1(?)), 0)")
                .bind("rustango_migrate_")
                .bind(scope.database())
                .fetch_one(&mut conn)
                .await
                .map_err(|e| e.to_string())?;
            match got {
                Some(1) => Ok(Some(Held::Mysql(conn))),
                Some(_) => Ok(None),
                None => Err("GET_LOCK returned NULL".into()),
            }
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(_) => {
            // v1's file: `<main file>-migrate.lock`; the target path is canonicalised.
            let file = std::fs::canonicalize(scope.database()).map_err(|e| e.to_string())?;
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(format!("{}-migrate.lock", file.display()))
                .create_if_missing(true)
                .busy_timeout(std::time::Duration::ZERO);
            let mut conn = opts.connect().await.map_err(|e| e.to_string())?;
            match sqlx::query("BEGIN IMMEDIATE").execute(&mut conn).await {
                Ok(_) => Ok(Some(Held::Sqlite(conn))),
                Err(e)
                    if e.as_database_error()
                        .and_then(|d| d.code()?.parse::<i32>().ok())
                        .is_some_and(|c| c & 0xff == 5) =>
                {
                    Ok(None)
                }
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

/// v1's SQLite lock file for `pool` (`pragma_database_list`), for comparison.
///
/// # Errors
/// Driver errors.
#[cfg(feature = "sqlite")]
pub async fn v1_sqlite_lock_file(pool: &sqlx::SqlitePool) -> Result<Option<String>, String> {
    let opts = crate::migrate::runner::sqlite_lock_options(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(opts.map(|o| o.get_filename().display().to_string()))
}

/// Hold v1's migrate lock on `pool` (the real `with_migrate_lock_held`) until
/// `release` fires; `taken` fires once it is held.
///
/// # Errors
/// v1's error, as text.
pub async fn v1_hold(
    pool: Pool,
    taken: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), String> {
    crate::migrate::runner::with_migrate_lock_held(&pool, |_held| async move {
        let _ = taken.send(());
        let _ = release.await;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())
}

/// Try v1's lock on `pool` for `wait`. `Ok(false)` on v1's `LockTimeout`.
///
/// # Errors
/// Any other v1 error, as text.
pub async fn v1_try(pool: &Pool, wait: std::time::Duration) -> Result<bool, String> {
    let r = crate::migrate::runner::with_lock_timeout(
        wait,
        crate::migrate::runner::with_migrate_lock_held(pool, |_held| async { Ok(()) }),
    )
    .await;
    match r {
        Ok(()) => Ok(true),
        Err(crate::migrate::MigrateError::LockTimeout(_)) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}
