//! `AtomicRequestLayer` on one pool: a write request runs in one `atomic`
//! block, and only a 2xx/3xx response commits it. Rows are asserted, not
//! statuses. SQLite-only cases (deferred begin, a COMMIT held by a reader)
//! are plain tests at the end.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "runserver",
    feature = "testkit"
))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use rustango::atomic_request::AtomicRequestLayer;
use rustango::sql::{
    atomic, atomic_with, in_atomic, on_commit, outside_atomic, raw_execute_pool, AtomicOptions,
    DialectKind, ExecError, FetcherPool as _, Pool, SqlError,
};
use rustango::testkit::CaptureWriter;
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "areq_item")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: i64,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    #[cfg(feature = "tenancy")]
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

async fn items(pool: &Pool) -> Vec<i64> {
    let rows: Vec<Item> = Item::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.id).collect()
}

fn kind(pool: &Pool) -> DialectKind {
    pool.dialect().kind()
}

#[derive(Clone)]
struct Ctx {
    pool: Pool,
    /// Set when a `hold` handler has written and waits.
    entered: Arc<tokio::sync::Notify>,
    /// Set by an `on_commit` callback.
    committed: Arc<AtomicBool>,
}

#[derive(serde::Deserialize)]
struct Op {
    #[serde(default)]
    mode: String,
    #[serde(default = "created")]
    status: u16,
    #[serde(default)]
    base: i64,
}

fn created() -> u16 {
    201
}

async fn put(pool: &Pool, id: i64) -> Result<(), ExecError> {
    Item { id }.insert(pool).await
}

async fn handler(State(c): State<Ctx>, Query(op): Query<Op>) -> Response {
    let (p, b) = (&c.pool, op.base);
    let status = StatusCode::from_u16(op.status).unwrap();
    match op.mode.as_str() {
        "panic" => {
            put(p, b + 1).await.unwrap();
            put(p, b + 2).await.unwrap();
            panic!("handler panicked after two writes");
        }
        "in_atomic" => {
            put(p, b + 1).await.unwrap();
            return in_atomic(p).to_string().into_response();
        }
        "nested" => {
            put(p, b + 1).await.unwrap();
            let q = p.clone();
            let inner: Result<(), ExecError> = atomic(p, move |_tx| {
                Box::pin(async move {
                    put(&q, b + 2).await?;
                    Err(ExecError::Sql(SqlError::EmptyInList))
                })
            })
            .await;
            assert!(inner.is_err());
        }
        "spawn" => {
            // A spawned task escapes the block: its write commits on its own.
            let q = p.clone();
            tokio::spawn(async move { put(&q, b + 1).await })
                .await
                .unwrap()
                .unwrap();
            put(p, b + 2).await.unwrap();
        }
        "outside" => {
            outside_atomic(put(p, b + 1)).await.unwrap();
            put(p, b + 2).await.unwrap();
        }
        "raw_dup" => {
            let sql = format!(
                "INSERT INTO {} (id) VALUES ({})",
                p.dialect().quote_ident("areq_item"),
                b + 1
            );
            raw_execute_pool(p, &sql, vec![]).await.unwrap();
            // Swallowed: on PG it aborts the block.
            let _ = raw_execute_pool(p, &sql, vec![]).await;
        }
        "ddl" => {
            put(p, b + 1).await.unwrap();
            let sql = "CREATE TABLE IF NOT EXISTS areq_ddl (id INT)";
            raw_execute_pool(p, sql, vec![]).await.unwrap();
        }
        "hold" => {
            put(p, b + 1).await.unwrap();
            c.entered.notify_one();
            std::future::pending::<()>().await;
        }
        "callback" => {
            put(p, b + 1).await.unwrap();
            let flag = c.committed.clone();
            on_commit(move || flag.store(true, Ordering::SeqCst));
        }
        "cookie" => {
            put(p, b + 1).await.unwrap();
            put(p, b + 2).await.unwrap();
            return (status, [(header::SET_COOKIE, "sid=rolled-back")], "ok").into_response();
        }
        #[cfg(feature = "tenancy")]
        "logout" => {
            // Stamps outside the block, then deactivates inside it.
            let id = op.base;
            assert!(
                rustango::session::deactivate::<rustango::tenancy::Operator>(p, id)
                    .await
                    .unwrap()
            );
        }
        _ => {
            put(p, b + 1).await.unwrap();
            put(p, b + 2).await.unwrap();
        }
    }
    status.into_response()
}

fn ctx(pool: &Pool) -> Ctx {
    Ctx {
        pool: pool.clone(),
        entered: Arc::default(),
        committed: Arc::default(),
    }
}

/// The handler behind the layer, inside a panic guard.
fn app(c: Ctx) -> Router {
    let pool = c.pool.clone();
    let routes = Router::new()
        .route("/w", post(handler).get(handler))
        .with_state(c)
        .layer(AtomicRequestLayer::new(pool));
    rustango::server::catch_panics(routes)
}

fn req(method: Method, query: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("/w?{query}"))
        .body(Body::empty())
        .unwrap()
}

async fn send(app: &Router, method: Method, query: &str) -> Response {
    app.clone().oneshot(req(method, query)).await.unwrap()
}

/// Two writes, then 500 / 400 / 403 / panic: nothing stored. 201 and 303 keep both.
async fn a_failed_request_stores_nothing(pool: &Pool) {
    let app = app(ctx(pool));
    for (i, status) in [500u16, 400, 403, 409].into_iter().enumerate() {
        let base = 10 * i as i64;
        send(&app, Method::POST, &format!("status={status}&base={base}")).await;
        assert_eq!(
            items(pool).await,
            Vec::<i64>::new(),
            "{status} kept its writes"
        );
    }
    let resp = send(&app, Method::POST, "mode=panic&base=50").await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        items(pool).await,
        Vec::<i64>::new(),
        "a panic kept its writes"
    );
    send(&app, Method::POST, "status=201&base=100").await;
    send(&app, Method::POST, "status=303&base=200").await;
    assert_eq!(items(pool).await, vec![101, 102, 201, 202]);
}

/// Every SQL statement sqlx logs, from one global subscriber.
fn sql_log() -> &'static CaptureWriter {
    static LOG: OnceLock<CaptureWriter> = OnceLock::new();
    LOG.get_or_init(|| {
        let log = CaptureWriter::default();
        let sink = log.clone();
        let sub = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .with_env_filter("sqlx::query=debug")
            .finish();
        tracing::subscriber::set_global_default(sub).expect("one subscriber");
        log
    })
}

/// The SQL sqlx logged while `f` ran.
async fn logged<F: std::future::Future>(f: F) -> (F::Output, String) {
    let log = sql_log();
    let from = log.contents().len();
    let out = f.await;
    (out, log.contents()[from..].to_owned())
}

/// GET runs in autocommit: no transaction statement logged; POST is the control.
async fn a_safe_method_sends_no_begin(pool: &Pool) {
    let app = app(ctx(pool));
    let (resp, get_sql) = logged(send(&app, Method::GET, "mode=in_atomic&base=0")).await;
    let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
    assert_eq!(&body[..], b"false", "GET ran in a block");
    assert!(
        get_sql.contains("INSERT"),
        "the capture saw nothing: {get_sql}"
    );
    for word in ["BEGIN", "SAVEPOINT", "COMMIT"] {
        assert!(!get_sql.contains(word), "GET sent {word}: {get_sql}");
    }
    let (resp, post_sql) = logged(send(&app, Method::POST, "mode=in_atomic&base=10")).await;
    let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
    assert_eq!(&body[..], b"true", "POST ran outside a block");
    // sqlx logs no BEGIN / COMMIT; the block shows as the write's savepoint.
    assert!(post_sql.contains("SAVEPOINT"), "{post_sql}");
    assert_eq!(items(pool).await, vec![1, 11]);
}

/// PG: a swallowed raw failure aborts the block, so the 200 becomes a fresh
/// 500 and nothing is stored. MySQL and SQLite undo only the statement.
async fn a_swallowed_failure_is_not_committed(pool: &Pool) {
    let app = app(ctx(pool));
    let resp = send(&app, Method::POST, "mode=raw_dup&status=200").await;
    if kind(pool) == DialectKind::Postgres {
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(items(pool).await, Vec::<i64>::new());
    } else {
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(items(pool).await, vec![1]);
    }
}

/// `areq_item.id` references an empty `areq_parent`, checked at COMMIT.
async fn deferred_fk(pool: &Pool) {
    for sql in [
        "DROP TABLE IF EXISTS areq_item",
        "DROP TABLE IF EXISTS areq_parent",
        "CREATE TABLE areq_parent (id BIGINT PRIMARY KEY)",
        "CREATE TABLE areq_item (id BIGINT PRIMARY KEY \
         REFERENCES areq_parent (id) DEFERRABLE INITIALLY DEFERRED)",
    ] {
        raw_execute_pool(pool, sql, vec![]).await.unwrap();
    }
}

/// A failed COMMIT drops the handler's response, its cookie included: a
/// constraint checked at COMMIT (PG deferred unique, SQLite deferred FK).
/// MySQL has no deferred constraint to fail a COMMIT with.
async fn a_failed_commit_drops_the_response(pool: &Pool) {
    if kind(pool) == DialectKind::Mysql {
        return;
    }
    deferred_fk(pool).await;
    let app = app(ctx(pool));
    let resp = send(&app, Method::POST, "mode=cookie&status=200").await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        resp.headers().get(header::SET_COOKIE).is_none(),
        "a rolled-back response's cookie was sent"
    );
    assert_eq!(items(pool).await, Vec::<i64>::new());
}

/// An idempotency layer inside the block stores a 2xx only once it commits:
/// the retry after a failed COMMIT runs again instead of replaying.
#[cfg(all(feature = "admin", feature = "cache"))]
async fn a_failed_commit_is_never_replayed(pool: &Pool) {
    use rustango::idempotency::{IdempotencyLayer, IdempotencyRouterExt as _};
    if kind(pool) == DialectKind::Mysql {
        return;
    }
    deferred_fk(pool).await;
    let cache: rustango::cache::BoxedCache = Arc::new(rustango::cache::InMemoryCache::new());
    let routes = Router::new()
        .route("/w", post(handler))
        .with_state(ctx(pool))
        .idempotency(IdempotencyLayer::new(cache))
        .layer(AtomicRequestLayer::new(pool.clone()));
    let keyed = || {
        Request::post("/w?status=200")
            .header("idempotency-key", "k")
            .body(Body::empty())
            .unwrap()
    };
    let first = routes.clone().oneshot(keyed()).await.unwrap();
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);
    raw_execute_pool(pool, "INSERT INTO areq_parent (id) VALUES (1), (2)", vec![])
        .await
        .unwrap();
    let again = routes.clone().oneshot(keyed()).await.unwrap();
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(
        items(pool).await,
        vec![1, 2],
        "the retry replayed a rolled-back 200"
    );
}

#[cfg(not(all(feature = "admin", feature = "cache")))]
async fn a_failed_commit_is_never_replayed(_: &Pool) {}

/// MySQL DDL commits the block early: a 500, logged, never a silent 200.
async fn mysql_ddl_in_a_handler_is_a_500(pool: &Pool) {
    if kind(pool) != DialectKind::Mysql {
        return;
    }
    use tracing::instrument::WithSubscriber as _;
    let log = CaptureWriter::default();
    let sink = log.clone();
    let sub = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    let app = app(ctx(pool));
    let resp = send(&app, Method::POST, "mode=ddl&status=200")
        .with_subscriber(sub)
        .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        log.contents().contains("request transaction failed"),
        "{}",
        log.contents()
    );
    let _ = raw_execute_pool(pool, "DROP TABLE IF EXISTS areq_ddl", vec![]).await;
}

/// PG serializable write skew: the losing COMMIT is a 503 with
/// `Retry-After`, and its writes are gone.
async fn a_serialization_failure_is_a_503(pool: &Pool) {
    if kind(pool) != DialectKind::Postgres {
        return;
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let skew = move |pool: Pool, id: i64, barrier: Arc<tokio::sync::Barrier>| {
        let opts = AtomicOptions::new().isolation(rustango::sql::Isolation::Serializable);
        let routes = Router::new()
            .route(
                "/w",
                post(move |State(p): State<Pool>| {
                    let barrier = barrier.clone();
                    async move {
                        let seen = Item::objects().fetch(&p).await.unwrap().len();
                        barrier.wait().await;
                        put(&p, id + seen as i64).await.unwrap();
                        barrier.wait().await;
                        StatusCode::CREATED
                    }
                }),
            )
            .with_state(pool.clone())
            .layer(AtomicRequestLayer::new(pool).options(opts));
        async move { send(&routes, Method::POST, "").await }
    };
    let (a, b) = tokio::join!(
        skew(pool.clone(), 1, barrier.clone()),
        skew(pool.clone(), 100, barrier)
    );
    let mut statuses = [a.status(), b.status()];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::CREATED, StatusCode::SERVICE_UNAVAILABLE]
    );
    let lost = if a.status() == StatusCode::SERVICE_UNAVAILABLE {
        &a
    } else {
        &b
    };
    assert_eq!(lost.headers()[header::RETRY_AFTER], "1");
    assert_eq!(items(pool).await.len(), 1);
}

/// A client gone mid-handler: the future is dropped and the block rolls back.
async fn a_dropped_request_rolls_back(pool: &Pool) {
    let c = ctx(pool);
    let entered = c.entered.clone();
    let app = app(c);
    let task = tokio::spawn(async move { send(&app, Method::POST, "mode=hold").await });
    entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // The next write on the pool reuses the dropped connection.
    put(pool, 99).await.unwrap();
    assert_eq!(items(pool).await, vec![99]);
}

/// A nested `atomic` in a handler is a savepoint of the request's block.
async fn a_nested_block_is_a_savepoint(pool: &Pool) {
    let app = app(ctx(pool));
    let resp = send(&app, Method::POST, "mode=nested").await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        items(pool).await,
        vec![1],
        "the nested block's write stayed"
    );
}

/// Pinned: a spawned task escapes the block and keeps its write.
async fn a_spawned_write_escapes_the_block(pool: &Pool) {
    let app = app(ctx(pool));
    send(&app, Method::POST, "mode=spawn&status=500").await;
    assert_eq!(items(pool).await, vec![1]);
}

/// `outside_atomic` commits on its own; the rest rolls back with the 500.
async fn an_outside_write_survives_a_500(pool: &Pool) {
    let app = app(ctx(pool));
    send(&app, Method::POST, "mode=outside&status=500").await;
    assert_eq!(items(pool).await, vec![1]);
}

/// `session::deactivate` stamps the logout outside the block: a 500 keeps
/// the account active and its sessions ended.
#[cfg(feature = "tenancy")]
async fn a_logout_stamp_survives_a_500(pool: &Pool) {
    use rustango::tenancy::Operator;
    let mut op = Operator {
        id: rustango::sql::Auto::default(),
        username: format!("areq-{}", uuid::Uuid::new_v4().simple()),
        password_hash: "x".into(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert(pool).await.unwrap();
    let id = *op.id.get().unwrap();
    let app = app(ctx(pool));
    send(
        &app,
        Method::POST,
        &format!("mode=logout&status=500&base={id}"),
    )
    .await;
    let got: Vec<Operator> = Operator::objects()
        .filter("id", id)
        .fetch(pool)
        .await
        .unwrap();
    assert!(got[0].active, "the deactivation committed");
    assert!(
        got[0].sessions_revoked_at.is_some(),
        "the logout rolled back"
    );
}

#[cfg(not(feature = "tenancy"))]
async fn a_logout_stamp_survives_a_500(_: &Pool) {}

/// `deferred()` keeps the isolation level where BEGIN is always deferred.
async fn deferred_still_sets_the_level(pool: &Pool) {
    let opts = AtomicOptions::new()
        .deferred()
        .isolation(rustango::sql::Isolation::Serializable);
    let q = pool.clone();
    let level = atomic_with(pool, opts, move |_tx| {
        Box::pin(async move {
            let sql = match q.dialect().kind() {
                DialectKind::Postgres => "SHOW transaction_isolation",
                DialectKind::Mysql => "SELECT @@transaction_isolation",
                _ => return Ok(String::new()),
            };
            let rows: Vec<(String,)> = rustango::sql::raw_query_pool(sql, vec![], &q).await?;
            Ok(rows[0].0.to_lowercase())
        })
    })
    .await
    .unwrap();
    match kind(pool) {
        DialectKind::Postgres => assert_eq!(level, "serializable"),
        // MySQL's variable shows the session level; the block's own is proven
        // by behaviour in `atomic_options_tri`. Here: accepted, not refused.
        _ => {}
    }
    // Nested, it is refused like the other options.
    let q = pool.clone();
    let nested = rustango::atomic!(pool, |_tx| {
        atomic_with(&q, AtomicOptions::new().deferred(), |_tx| {
            Box::pin(async { Ok(()) })
        })
        .await
    })
    .await;
    assert!(
        matches!(nested, Err(ExecError::InAtomic { .. })),
        "{nested:?}"
    );
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        a_failed_request_stores_nothing,
        a_safe_method_sends_no_begin,
        a_swallowed_failure_is_not_committed,
        a_failed_commit_drops_the_response,
        a_failed_commit_is_never_replayed,
        mysql_ddl_in_a_handler_is_a_500,
        a_serialization_failure_is_a_503,
        a_dropped_request_rolls_back,
        a_nested_block_is_a_savepoint,
        a_spawned_write_escapes_the_block,
        an_outside_write_survives_a_500,
        a_logout_stamp_survives_a_500,
        deferred_still_sets_the_level,
    ],
}

#[cfg(feature = "sqlite")]
mod sqlite_only {
    use super::*;
    use rustango::sql::sqlx;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::str::FromStr as _;
    use std::time::Duration;

    fn file(dir: &tempfile::TempDir) -> String {
        format!("sqlite://{}", dir.path().join("areq.db").display())
    }

    async fn pool_with(url: &str, mode: SqliteJournalMode, busy: Duration) -> Pool {
        let opts = SqliteConnectOptions::from_str(url)
            .unwrap()
            .create_if_missing(true)
            .journal_mode(mode)
            .busy_timeout(busy);
        Pool::Sqlite(SqlitePoolOptions::new().connect_with(opts).await.unwrap())
    }

    /// A pool on `url` in rollback-journal mode, where COMMIT waits for readers.
    async fn journal_pool(url: &str, busy: Duration) -> Pool {
        pool_with(url, SqliteJournalMode::Delete, busy).await
    }

    /// A read transaction held open on its own connection: its SHARED lock
    /// keeps a writer's COMMIT waiting.
    async fn reader(url: &str) -> sqlx::pool::PoolConnection<sqlx::Sqlite> {
        let pool = journal_pool(url, Duration::ZERO).await;
        let mut conn = pool.as_sqlite().unwrap().acquire().await.unwrap();
        use sqlx::Executor as _;
        conn.execute("BEGIN").await.unwrap();
        conn.execute("SELECT COUNT(*) FROM areq_item")
            .await
            .unwrap();
        conn
    }

    /// Whether a writer holds PENDING, i.e. its COMMIT waits: a new reader is refused.
    async fn commit_waiting(url: &str) -> bool {
        use sqlx::Executor as _;
        let pool = journal_pool(url, Duration::ZERO).await;
        let mut conn = pool.as_sqlite().unwrap().acquire().await.unwrap();
        let refused = conn
            .execute("BEGIN; SELECT COUNT(*) FROM areq_item")
            .await
            .is_err();
        let _ = conn.execute("ROLLBACK").await;
        refused
    }

    /// A client gone during COMMIT: the commit and its `on_commit` callback
    /// still run, so the data never lands without its follow-up work.
    #[tokio::test]
    async fn a_drop_during_commit_still_runs_on_commit() {
        let dir = tempfile::tempdir().unwrap();
        let url = file(&dir);
        let pool = journal_pool(&url, Duration::from_secs(10)).await;
        setup(&pool).await;
        let mut held = reader(&url).await;
        let c = ctx(&pool);
        let committed = c.committed.clone();
        let app = app(c);
        let task = tokio::spawn(async move { send(&app, Method::POST, "mode=callback").await });
        while !commit_waiting(&url).await {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        use sqlx::Executor as _;
        held.execute("ROLLBACK").await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !committed.load(Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "on_commit never ran"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(items(&pool).await, vec![1]);
    }

    /// A COMMIT that times out on `SQLITE_BUSY` is a 503 with `Retry-After`.
    #[tokio::test]
    async fn a_busy_commit_is_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let url = file(&dir);
        let pool = journal_pool(&url, Duration::from_millis(200)).await;
        setup(&pool).await;
        let mut held = reader(&url).await;
        let resp = send(&app(ctx(&pool)), Method::POST, "").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers()[header::RETRY_AFTER], "1");
        use sqlx::Executor as _;
        held.execute("ROLLBACK").await.unwrap();
        assert_eq!(items(&pool).await, Vec::<i64>::new());
    }

    /// The layer begins SQLite blocks deferred: a POST that only reads
    /// holds no write lock, so another connection writes meanwhile.
    #[tokio::test]
    async fn a_read_only_post_takes_no_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        let url = file(&dir);
        // WAL: a reader never blocks a writer, so only a write lock could.
        let pool = pool_with(&url, SqliteJournalMode::Wal, Duration::from_millis(100)).await;
        setup(&pool).await;
        let other = pool_with(&url, SqliteJournalMode::Wal, Duration::from_millis(100)).await;
        let routes = Router::new()
            .route(
                "/w",
                post(move |State(p): State<Pool>| {
                    let other = other.clone();
                    async move {
                        assert!(in_atomic(&p));
                        let _ = Item::objects().fetch(&p).await.unwrap();
                        put(&other, 7)
                            .await
                            .map_or(StatusCode::CONFLICT, |()| StatusCode::OK)
                    }
                }),
            )
            .with_state(pool.clone())
            .layer(AtomicRequestLayer::new(pool.clone()));
        assert_eq!(
            send(&routes, Method::POST, "").await.status(),
            StatusCode::OK
        );
        assert_eq!(items(&pool).await, vec![7]);
    }

    /// `deferred()` begins with a plain BEGIN and works on a read-only
    /// database. (SQLite takes `BEGIN IMMEDIATE` there too; pinned only.)
    #[tokio::test]
    async fn deferred_works_on_a_read_only_database() {
        let dir = tempfile::tempdir().unwrap();
        let url = file(&dir);
        setup(&journal_pool(&url, Duration::ZERO).await).await;
        let opts = SqliteConnectOptions::from_str(&url)
            .unwrap()
            .read_only(true);
        let ro = Pool::Sqlite(SqlitePoolOptions::new().connect_with(opts).await.unwrap());
        let read = |opts: AtomicOptions| {
            let q = ro.clone();
            atomic_with(&ro, opts, move |_tx| {
                Box::pin(async move { Ok(Item::objects().fetch(&q).await?.len()) })
            })
        };
        assert_eq!(read(AtomicOptions::new().deferred()).await.unwrap(), 0);
    }
}
