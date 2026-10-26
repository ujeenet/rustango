//! Which database a URL or pool lands on, read from the server or the
//! file itself, so a host alias, a link or a mount cannot hide a twin.

use super::BackendKind;

/// An existing file: `(dev, ino)` on unix, the path SQLite opens elsewhere.
#[cfg(unix)]
type FileKey = (u64, u64);
#[cfg(not(unix))]
type FileKey = std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) enum DbIdentity {
    File(FileKey),
    Server {
        backend: BackendKind,
        key: String,
        database: String,
    },
}

/// How long a probe waits for a server: it only adds a check, so it is short.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

impl DbIdentity {
    /// Database `name` on the server behind an admin `pool`: known
    /// without the tenant's login, which a retried purge may have dropped.
    #[cfg_attr(not(feature = "mysql"), allow(dead_code))]
    pub(crate) async fn on_server(
        pool: &crate::sql::Pool,
        name: &str,
    ) -> Result<Option<Self>, String> {
        Self::server(pool, Some(name)).await
    }

    /// `Ok(None)` for an unknown scheme or a missing / in-memory SQLite file.
    pub(crate) async fn of_url(url: &str) -> Result<Option<Self>, String> {
        match BackendKind::of_url(url) {
            #[cfg(feature = "sqlite")]
            Some(BackendKind::Sqlite) => {
                let o = <sqlx::sqlite::SqliteConnectOptions as std::str::FromStr>::from_str(url)
                    .map_err(|e| e.to_string())?;
                Ok(Self::file(o.get_filename()))
            }
            #[allow(unreachable_patterns)]
            Some(BackendKind::Sqlite) | None => Ok(None),
            Some(_) => {
                let pool = crate::sql::Pool::connect_with_timeout(url, PROBE_TIMEOUT)
                    .await
                    .map_err(|e| e.to_string())?;
                let id = Self::of_pool(&pool).await;
                pool.close().await;
                id
            }
        }
    }

    pub(crate) async fn of_pool(pool: &crate::sql::Pool) -> Result<Option<Self>, String> {
        #[allow(unreachable_patterns)]
        match pool {
            #[cfg(feature = "sqlite")]
            crate::sql::Pool::Sqlite(p) => Ok(Self::file(p.connect_options().get_filename())),
            _ => Self::server(pool, None).await,
        }
    }

    /// Whether `url` may land on this database, read from its text: only a
    /// probe can tell an alias, but another database name rules one out.
    pub(crate) fn may_be(&self, url: &str) -> bool {
        match self {
            Self::File(_) => self.same_backend(url),
            Self::Server { database, .. } => {
                self.same_backend(url)
                    && url_database(url).is_none_or(|d| d.eq_ignore_ascii_case(database))
            }
        }
    }

    pub(crate) fn same_backend(&self, url: &str) -> bool {
        let kind = BackendKind::of_url(url);
        match self {
            Self::File(_) => kind == Some(BackendKind::Sqlite),
            Self::Server { backend, .. } => kind == Some(*backend),
        }
    }

    /// The file SQLite opens for `name`, if it exists.
    #[cfg(feature = "sqlite")]
    pub(crate) fn file(name: &std::path::Path) -> Option<Self> {
        let path = super::provision::sqlite_file(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            std::fs::metadata(path)
                .ok()
                .map(|m| Self::File((m.dev(), m.ino())))
        }
        #[cfg(not(unix))]
        path.is_file().then_some(Self::File(path))
    }

    /// The server's key and database through the dialect's emitter;
    /// `Ok(None)` where it has none.
    async fn server(pool: &crate::sql::Pool, db: Option<&str>) -> Result<Option<Self>, String> {
        let d = pool.dialect();
        let Some(version_sql) = d.server_version_sql() else {
            return Ok(None);
        };
        let version: Vec<(String,)> = crate::sql::raw_query_pool(version_sql, Vec::new(), pool)
            .await
            .map_err(|e| e.to_string())?;
        let version = version.into_iter().next().map(|(v,)| v).unwrap_or_default();
        let Some(stmt) = d.database_identity_sql(&version, db) else {
            return Ok(None);
        };
        let rows: Vec<(String, Option<String>)> =
            crate::sql::raw_query_pool(&stmt.sql, stmt.params, pool)
                .await
                .map_err(|e| e.to_string())?;
        let (key, database) = rows
            .into_iter()
            .next()
            .ok_or("the server sent no identity")?;
        let database = database.ok_or("the connection selects no database")?;
        Ok(Some(Self::Server {
            backend: BackendKind::of_pool(pool),
            key,
            database,
        }))
    }
}

/// The database a server URL names, as its driver reads it; `None` when
/// unreadable or another backend.
fn url_database(url: &str) -> Option<String> {
    match BackendKind::of_url(url)? {
        #[cfg(feature = "postgres")]
        BackendKind::Postgres => url
            .parse::<sqlx::postgres::PgConnectOptions>()
            .ok()
            .map(|o| o.get_database().unwrap_or(o.get_username()).to_owned()),
        #[cfg(feature = "mysql")]
        BackendKind::MySql => url
            .parse::<sqlx::mysql::MySqlConnectOptions>()
            .ok()?
            .get_database()
            .map(str::to_owned),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}
