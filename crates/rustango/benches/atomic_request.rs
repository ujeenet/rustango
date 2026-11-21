//! Cost of `AtomicRequestLayer` per write request: p50/p99 with and without
//! it, in-process (no HTTP), on SQLite (file, WAL) and on Postgres / MySQL
//! when `BENCH_PG_URL` / `BENCH_MYSQL_URL` are set. Prints a markdown table.
//!
//! `BENCH_PG_URL=postgres://… cargo bench -p rustango --features sqlite,postgres,runserver --bench atomic_request`

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::Router;
use rustango::atomic_request::AtomicRequestLayer;
use rustango::sql::{atomic_with, AtomicOptions, Pool};
use rustango::Model;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "bench_ar_row")]
pub struct Row {
    #[rustango(primary_key)]
    pub id: i64,
    pub n: i64,
}

static NEXT_ID: AtomicI64 = AtomicI64::new(1);
static RETRIES: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct App {
    pool: Pool,
    writes: usize,
    pause: Duration,
}

async fn handler(State(app): State<App>) -> StatusCode {
    for _ in 0..app.writes {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Row { id, n: id }.insert(&app.pool).await.expect("insert");
    }
    if !app.pause.is_zero() {
        tokio::time::sleep(app.pause).await;
    }
    StatusCode::CREATED
}

fn router(app: App, layer: bool) -> Router {
    let pool = app.pool.clone();
    let r = Router::new().route("/w", post(handler)).with_state(app);
    if layer {
        r.layer(AtomicRequestLayer::new(pool))
    } else {
        r
    }
}

/// `n` requests from `conc` workers; sorted latencies.
async fn drive(app: &Router, n: usize, conc: usize) -> Vec<Duration> {
    let left = Arc::new(AtomicUsize::new(n));
    let workers = (0..conc).map(|_| {
        let (app, left) = (app.clone(), left.clone());
        tokio::spawn(async move {
            let mut out = Vec::new();
            while left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |l| l.checked_sub(1))
                .is_ok()
            {
                let req = Request::post("/w").body(Body::empty()).unwrap();
                let t = Instant::now();
                let resp = app.clone().oneshot(req).await.unwrap();
                assert_eq!(resp.status(), StatusCode::CREATED);
                out.push(t.elapsed());
            }
            out
        })
    });
    let mut all: Vec<Duration> = futures_util::future::join_all(workers)
        .await
        .into_iter()
        .flat_map(Result::unwrap)
        .collect();
    all.sort();
    all
}

/// `body` run `n` times from `conc` workers; sorted latencies.
async fn drive_fn<F, Fut>(n: usize, conc: usize, body: F) -> Vec<Duration>
where
    F: Fn() -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let left = Arc::new(AtomicUsize::new(n));
    let workers = (0..conc).map(|_| {
        let (body, left) = (body.clone(), left.clone());
        tokio::spawn(async move {
            let mut out = Vec::new();
            while left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |l| l.checked_sub(1))
                .is_ok()
            {
                let t = Instant::now();
                body().await;
                out.push(t.elapsed());
            }
            out
        })
    });
    let mut all: Vec<Duration> = futures_util::future::join_all(workers)
        .await
        .into_iter()
        .flat_map(Result::unwrap)
        .collect();
    all.sort();
    all
}

fn pct(v: &[Duration], p: f64) -> f64 {
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i].as_secs_f64() * 1e3
}

async fn pool_with(url: &str, max: u32) -> Pool {
    match url.split(':').next() {
        Some("postgres") => Pool::Postgres(
            rustango::sql::sqlx::postgres::PgPoolOptions::new()
                .max_connections(max)
                .acquire_timeout(Duration::from_secs(60))
                .connect(url)
                .await
                .unwrap(),
        ),
        #[cfg(feature = "mysql")]
        Some("mysql") => Pool::Mysql(
            rustango::sql::sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(max)
                .acquire_timeout(Duration::from_secs(60))
                .connect(url)
                .await
                .unwrap(),
        ),
        _ => {
            use std::str::FromStr as _;
            let opts = rustango::sql::sqlx::sqlite::SqliteConnectOptions::from_str(url)
                .unwrap()
                .create_if_missing(true)
                .journal_mode(rustango::sql::sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(30));
            Pool::Sqlite(
                rustango::sql::sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(max)
                    .connect_with(opts)
                    .await
                    .unwrap(),
            )
        }
    }
}

async fn fresh(pool: &Pool) {
    let q = pool.dialect().quote_ident("bench_ar_row");
    let _ =
        rustango::sql::raw_execute_pool(pool, &format!("DROP TABLE IF EXISTS {q}"), vec![]).await;
    rustango::sql::raw_execute_pool(
        pool,
        &format!("CREATE TABLE {q} (id BIGINT PRIMARY KEY, n BIGINT NOT NULL)"),
        vec![],
    )
    .await
    .unwrap();
}

fn row(backend: &str, case: &str, conc: usize, off: &[Duration], on: &[Duration]) {
    let (p50a, p99a, p50b, p99b) = (pct(off, 0.5), pct(off, 0.99), pct(on, 0.5), pct(on, 0.99));
    println!(
        "| {backend} | {case} | {conc} | {p50a:.2} | {p99a:.2} | {p50b:.2} | {p99b:.2} | {:+.2} |",
        p99b - p99a
    );
}

async fn layer_rows(backend: &str, url: &str) {
    for (slots, label) in [(16u32, "16-slot pool"), (3, "3-slot pool")] {
        let pool = pool_with(url, slots).await;
        fresh(&pool).await;
        let concs: &[usize] = if slots == 3 {
            &[20, 100]
        } else {
            &[1, 20, 100]
        };
        let writes: &[usize] = if slots == 3 { &[1] } else { &[1, 5, 50] };
        for &w in writes {
            for &conc in concs {
                let n = if w == 50 { 200 } else { 2000 };
                let app = App {
                    pool: pool.clone(),
                    writes: w,
                    pause: Duration::ZERO,
                };
                let (off, on) = (router(app.clone(), false), router(app, true));
                drive(&off, n / 10, conc).await; // warm up
                drive(&on, n / 10, conc).await;
                let a = drive(&off, n, conc).await;
                let b = drive(&on, n, conc).await;
                row(backend, &format!("{w} write(s), {label}"), conc, &a, &b);
            }
        }
        pool.close().await;
    }
}

/// A 2 s handler on a 3-slot pool: the layer holds a connection throughout.
async fn exhaustion_row(backend: &str, url: &str) {
    let pool = pool_with(url, 3).await;
    fresh(&pool).await;
    let app = App {
        pool: pool.clone(),
        writes: 1,
        pause: Duration::from_secs(2),
    };
    let a = drive(&router(app.clone(), false), 12, 12).await;
    let b = drive(&router(app, true), 12, 12).await;
    row(backend, "1 write + 2 s pause, 3-slot pool", 12, &a, &b);
    pool.close().await;
}

/// SQLite read-then-write blocks, 8 at once: deferred vs immediate begin.
async fn sqlite_begin_rows(url: &str) {
    let pool = pool_with(url, 8).await;
    fresh(&pool).await;
    let block = |opts: AtomicOptions, pool: Pool| {
        move || {
            let (opts, pool) = (opts.clone(), pool.clone());
            async move {
                loop {
                    let q = pool.clone();
                    let r = atomic_with(&pool, opts.clone(), move |_tx| {
                        Box::pin(async move {
                            use rustango::sql::FetcherPool as _;
                            let _ = Row::objects().filter("id", 0_i64).fetch(&q).await?;
                            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                            Row { id, n: id }.insert(&q).await?;
                            Ok(())
                        })
                    })
                    .await;
                    // Deferred upgrades lose to a concurrent writer: retry, as a client would.
                    match r {
                        Ok(()) => return,
                        Err(e) => {
                            assert!(
                                RETRIES.fetch_add(1, Ordering::Relaxed) < 100_000,
                                "no progress: {e}"
                            );
                        }
                    }
                }
            }
        }
    };
    let imm = drive_fn(500, 8, block(AtomicOptions::new(), pool.clone())).await;
    let imm_retries = RETRIES.swap(0, Ordering::Relaxed);
    let def = drive_fn(500, 8, block(AtomicOptions::new().deferred(), pool.clone())).await;
    let def_retries = RETRIES.swap(0, Ordering::Relaxed);
    row(
        "sqlite",
        &format!(
            "read+write block, immediate (off, {imm_retries} retries) vs deferred (on, {def_retries} retries)"
        ),
        8,
        &imm,
        &def,
    );
    pool.close().await;
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        println!("| backend | case | conc | p50 off | p99 off | p50 on | p99 on | added p99 |");
        println!("|---|---|---|---|---|---|---|---|");
        let dir = tempfile::tempdir().unwrap();
        let sqlite = format!("sqlite://{}", dir.path().join("bench.db").display());
        if let Ok(url) = std::env::var("BENCH_PG_URL") {
            layer_rows("postgres", &url).await;
            exhaustion_row("postgres", &url).await;
        }
        if let Ok(url) = std::env::var("BENCH_MYSQL_URL") {
            layer_rows("mysql", &url).await;
        }
        if std::env::var("BENCH_ONLY_BEGIN").is_err() {
            layer_rows("sqlite", &sqlite).await;
        }
        sqlite_begin_rows(&sqlite).await;
    });
}
