//! The DDL session of an autocommit unit (6.3) and reaping it on resume
//! (6.5): a dead client does not stop its DDL, so the next run ends the
//! recorded session before it probes.
//!
//! Hand-written per backend, as the ledger: these are server catalog calls.

use crate::migrate::MigrateError;
use crate::sql::Pool;
use crate::sql::{PoolConn, PooledConn};

/// The server session an autocommit unit ran on; stored in the ledger's
/// `session` column while the unit runs. SQLite has none.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub(crate) enum Session {
    /// `pid` and `backend_start` together: a pid alone is reused.
    Postgres { pid: i64, backend_start: String },
    /// `CONNECTION_ID()`, with what proves a live thread is still this one:
    /// the server (`@@server_uuid`, start time from `Uptime`), the user and
    /// the unit's statement, which names its table.
    Mysql {
        id: u64,
        server_uuid: String,
        server_started: i64,
        user: String,
        statement: String,
    },
}

impl std::fmt::Display for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Postgres { pid, .. } => write!(f, "pid {pid}"),
            Self::Mysql { id, .. } => write!(f, "thread {id}"),
        }
    }
}

/// A runner-owned connection for one autocommit unit, closed after it, and
/// its session. PG asks the server to check for a gone client every second.
pub(crate) struct DdlConn {
    conn: PoolConn,
    pub(crate) session: Option<Session>,
}

impl DdlConn {
    /// `statement` is what the unit runs on it (MySQL proves a thread by it).
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn open(
        pool: &Pool,
        statement: &str,
        lock: std::time::Duration,
    ) -> Result<Self, MigrateError> {
        let mut conn = pool.acquire().await?;
        conn.close_on_drop();
        let session = match conn.conn() {
            #[cfg(feature = "postgres")]
            PooledConn::Postgres(c) => {
                use sqlx::Executor as _;
                // The DDL lock timeout (6.6); session-wide, the connection closes after.
                (&mut **c)
                    .execute(sqlx::raw_sql(&format!(
                        "SET lock_timeout = {}",
                        lock.as_millis()
                    )))
                    .await?;
                // PG 14+; older servers lack the setting and keep the default.
                let _ = (&mut **c)
                    .execute(sqlx::raw_sql("SET client_connection_check_interval = 1000"))
                    .await;
                let (pid, backend_start): (i32, String) = sqlx::query_as(
                    "SELECT pid, backend_start::text FROM pg_stat_activity \
                     WHERE pid = pg_backend_pid()",
                )
                .fetch_one(&mut **c)
                .await?;
                Some(Session::Postgres {
                    pid: i64::from(pid),
                    backend_start,
                })
            }
            #[cfg(feature = "mysql")]
            PooledConn::Mysql(c) => {
                use sqlx::Executor as _;
                (&mut **c)
                    .execute(sqlx::raw_sql(&format!(
                        "SET SESSION lock_wait_timeout = {}",
                        lock.as_secs().max(1)
                    )))
                    .await?;
                let (id, user): (u64, String) = sqlx::query_as(
                    "SELECT CONNECTION_ID(), SUBSTRING_INDEX(CURRENT_USER(), '@', 1)",
                )
                .fetch_one(&mut **c)
                .await?;
                let (server_uuid, server_started) = mysql_server(&mut **c).await?;
                Some(Session::Mysql {
                    id,
                    server_uuid,
                    server_started,
                    user,
                    statement: statement.to_owned(),
                })
            }
            #[cfg(feature = "sqlite")]
            PooledConn::Sqlite(_) => None,
        };
        Ok(Self { conn, session })
    }

    /// One statement, unprepared, committing on its own.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn execute(&mut self, sql: &str) -> Result<(), sqlx::Error> {
        execute(&mut self.conn, sql).await
    }
}

/// MySQL server identity: `@@server_uuid` and its start time (now - Uptime).
#[cfg(feature = "mysql")]
async fn mysql_server(c: &mut sqlx::MySqlConnection) -> Result<(String, i64), sqlx::Error> {
    sqlx::query_as(
        "SELECT @@server_uuid, CAST(UNIX_TIMESTAMP() - VARIABLE_VALUE AS SIGNED) \
         FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Uptime'",
    )
    .fetch_one(c)
    .await
}

/// Start times read twice can differ by the second that ticked between.
const START_SLACK_SECS: i64 = 2;

async fn execute(conn: &mut PoolConn, sql: &str) -> Result<(), sqlx::Error> {
    use sqlx::Executor as _;
    match conn.conn() {
        #[cfg(feature = "postgres")]
        PooledConn::Postgres(c) => (&mut **c).execute(sqlx::raw_sql(sql)).await.map(drop),
        #[cfg(feature = "mysql")]
        PooledConn::Mysql(c) => (&mut **c).execute(sqlx::raw_sql(sql)).await.map(drop),
        #[cfg(feature = "sqlite")]
        PooledConn::Sqlite(c) => (&mut **c).execute(sqlx::raw_sql(sql)).await.map(drop),
    }
}

/// Why a recorded session could not be ended.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReapError {
    #[error(
        "cannot end the previous DDL session ({0}), which belongs to another role: run as that \
         role, or {1}"
    )]
    NoRights(Session, &'static str),
    #[error("previous DDL session {0} still running after {1:?}; the ledger is untouched")]
    StillRunning(Session, std::time::Duration),
    #[error(
        "the recorded DDL session ({0}) is not provably the one there now: {1}; it was not \
         ended, and the ledger is untouched"
    )]
    Mismatch(Session, String),
    #[error(transparent)]
    Db(#[from] MigrateError),
}

/// End `s` if it still runs, then wait until it is gone, at most `wait`.
///
/// # Errors
/// Missing rights to end it, a session that outlives `wait`, or a driver failure.
pub(crate) async fn reap(
    pool: &Pool,
    s: &Session,
    wait: std::time::Duration,
) -> Result<(), ReapError> {
    match s {
        Session::Postgres { pid, backend_start } => {
            let binds = || {
                vec![
                    crate::core::SqlValue::I64(*pid),
                    crate::core::SqlValue::String(backend_start.clone()),
                ]
            };
            const PG_GRANT: &str = "GRANT pg_signal_backend, pg_read_all_stats TO the migrate role";
            // Another role's session shows its pid but hides its start: it
            // cannot be told from a reused pid, so it is never taken as gone.
            let seen: Vec<(Option<String>,)> = crate::sql::raw_query_pool(
                "SELECT backend_start::text FROM pg_stat_activity WHERE pid = $1",
                vec![crate::core::SqlValue::I64(*pid)],
                pool,
            )
            .await
            .map_err(MigrateError::from)?;
            match seen.first() {
                None => return Ok(()),
                Some((None,)) => return Err(ReapError::NoRights(s.clone(), PG_GRANT)),
                Some((Some(start),)) if start != backend_start => return Ok(()),
                Some(_) => {}
            }
            // Only when pid and start still match: a pid alone may be reused.
            let ended: Result<Vec<(bool,)>, _> = crate::sql::raw_query_pool(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE pid = $1 AND backend_start::text = $2",
                binds(),
                pool,
            )
            .await;
            if let Err(e) = ended {
                if sqlstate(&e).as_deref() == Some("42501") {
                    return Err(ReapError::NoRights(s.clone(), PG_GRANT));
                }
                return Err(MigrateError::from(e).into());
            }
            gone(wait, s, || async {
                let n: Vec<(i64,)> = crate::sql::raw_query_pool(
                    "SELECT COUNT(*) FROM pg_stat_activity \
                     WHERE pid = $1 AND backend_start::text = $2",
                    binds(),
                    pool,
                )
                .await
                .map_err(MigrateError::from)?;
                Ok(n.first().map_or(0, |r| r.0) == 0)
            })
            .await
        }
        Session::Mysql {
            id,
            server_uuid,
            server_started,
            user,
            statement,
        } => {
            let mut conn = pool.acquire().await.map_err(MigrateError::from)?;
            let (uuid_now, started_now, user_now, seen) = mysql_now(&mut conn, *id)
                .await
                .map_err(MigrateError::Driver)?;
            // Another server, or the same one restarted: thread ids restart too.
            if &uuid_now != server_uuid || (started_now - server_started).abs() > START_SLACK_SECS {
                return Err(ReapError::Mismatch(
                    s.clone(),
                    format!(
                        "the server is {uuid_now} started at {started_now}, the session ran on \
                         {server_uuid} started at {server_started}"
                    ),
                ));
            }
            match seen {
                // Gone, if it was ours to see; another user's thread needs PROCESS.
                None if &user_now == user => return Ok(()),
                None => {
                    return Err(ReapError::NoRights(
                        s.clone(),
                        "GRANT CONNECTION_ADMIN, PROCESS ON *.* TO the migrate user",
                    ))
                }
                Some((u, info))
                    if &u != user || info.as_deref().map(str::trim) != Some(statement.trim()) =>
                {
                    return Err(ReapError::Mismatch(
                        s.clone(),
                        format!(
                            "thread {id} now runs `{}` as `{u}`, not the unit's statement as `{user}`",
                            info.as_deref().unwrap_or("nothing")
                        ),
                    ));
                }
                Some(_) => {}
            }
            match execute(&mut conn, &format!("KILL {id}")).await {
                Ok(()) => {}
                // 1094: no such thread, it is already gone.
                Err(e) if mysql_code(&e) == Some(1094) => return Ok(()),
                // 1095: not the owner, and no CONNECTION_ADMIN.
                Err(e) if mysql_code(&e) == Some(1095) => {
                    return Err(ReapError::NoRights(
                        s.clone(),
                        "GRANT CONNECTION_ADMIN, PROCESS ON *.* TO the migrate user",
                    ))
                }
                Err(e) => return Err(MigrateError::Driver(e).into()),
            }
            drop(conn);
            gone(wait, s, || async {
                let n: Vec<(i64,)> = crate::sql::raw_query_pool(
                    "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID = ?",
                    vec![crate::core::SqlValue::I64(
                        i64::try_from(*id).unwrap_or(i64::MAX),
                    )],
                    pool,
                )
                .await
                .map_err(MigrateError::from)?;
                Ok(n.first().map_or(0, |r| r.0) == 0)
            })
            .await
        }
    }
}

/// The server identity, the current user and what thread `id` runs, if visible.
async fn mysql_now(
    conn: &mut PoolConn,
    id: u64,
) -> Result<(String, i64, String, Option<(String, Option<String>)>), sqlx::Error> {
    #[cfg(feature = "mysql")]
    if let PooledConn::Mysql(c) = conn.conn() {
        let (uuid, started) = mysql_server(&mut **c).await?;
        let user: String = sqlx::query_scalar("SELECT SUBSTRING_INDEX(CURRENT_USER(), '@', 1)")
            .fetch_one(&mut **c)
            .await?;
        let seen: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT USER, INFO FROM information_schema.PROCESSLIST WHERE ID = ?")
                .bind(id)
                .fetch_optional(&mut **c)
                .await?;
        return Ok((uuid, started, user, seen));
    }
    let _ = (conn, id);
    Err(sqlx::Error::Protocol(
        "a MySQL session on another backend".into(),
    ))
}

async fn gone<F, Fut>(wait: std::time::Duration, s: &Session, is_gone: F) -> Result<(), ReapError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<bool, MigrateError>>,
{
    let until = std::time::Instant::now() + wait;
    loop {
        if is_gone().await? {
            return Ok(());
        }
        if std::time::Instant::now() >= until {
            return Err(ReapError::StillRunning(s.clone(), wait));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn sqlstate(e: &crate::sql::ExecError) -> Option<String> {
    match e {
        crate::sql::ExecError::Driver(sqlx::Error::Database(d)) => d.code().map(|c| c.into_owned()),
        _ => None,
    }
}

fn mysql_code(e: &sqlx::Error) -> Option<u16> {
    #[cfg(feature = "mysql")]
    if let Some(d) = e.as_database_error() {
        if let Some(m) = d.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
            return Some(m.number());
        }
    }
    let _ = e;
    None
}
