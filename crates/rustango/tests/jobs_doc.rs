//! Backing test for `docs/jobs.md` — the in-memory queue, retry-with-backoff,
//! and the dead-letter handler. The persistent (DB-backed) path is dogfooded
//! separately by `jobs_sqlite_live.rs` / `jobs_pg_live.rs`. The Step 1 and
//! Step 4 files, the `main.rs` and the worker examples compile here.
//!
//! Run: `cargo test -p rustango --test jobs_doc`

#![cfg(feature = "jobs")]

#[path = "support/doc_snippet.rs"]
mod doc_snippet;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustango::jobs::{InMemoryJobQueue, Job, JobError, JobQueue};
use serde::{Deserialize, Serialize};

/// Poll `pred` until true or a generous timeout (jobs run on worker tasks).
async fn wait_until(pred: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !pred() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ----------------------------------------------------------- 1. dispatch + run

static WELCOME_RUNS: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct WelcomeEmail {
    to: String,
}

#[async_trait::async_trait]
impl Job for WelcomeEmail {
    const NAME: &'static str = "doc:welcome_email";
    async fn run(&self) -> Result<(), JobError> {
        assert!(self.to.ends_with("@example.com"), "payload round-tripped");
        WELCOME_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn dispatched_jobs_run_on_workers() {
    let queue = Arc::new(InMemoryJobQueue::with_workers(4));
    queue.register::<WelcomeEmail>().await; // register before start/dispatch
    queue.start().await;

    dispatch_three(&queue).await;

    wait_until(|| WELCOME_RUNS.load(Ordering::SeqCst) == 3).await;
    queue.shutdown().await; // drains in-flight, then stops workers
    assert_eq!(WELCOME_RUNS.load(Ordering::SeqCst), 3);
}

/// Step 3's loop, as the page prints it.
#[rustfmt::skip]
async fn dispatch_three(queue: &InMemoryJobQueue) {
    // doc-snippet:start
    for to in ["a@example.com", "b@example.com", "c@example.com"] {
        queue.dispatch(&WelcomeEmail { to: to.into() }).await.unwrap();
    }
    // → all three WelcomeEmail::run() calls execute on the worker pool
    // doc-snippet:end
}

// ------------------------------------------------ 2. retryable failure + backoff

static FLAKY_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static FLAKY_SUCCEEDED: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct FlakyImport;

#[async_trait::async_trait]
impl Job for FlakyImport {
    const NAME: &'static str = "doc:flaky_import";
    async fn run(&self) -> Result<(), JobError> {
        let attempt = FLAKY_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt < 2 {
            // Transient failure → the worker retries with exponential backoff.
            Err(JobError::Retryable(format!("attempt {attempt} failed")))
        } else {
            FLAKY_SUCCEEDED.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
}

#[tokio::test]
async fn retryable_failure_is_retried_until_it_succeeds() {
    let queue = Arc::new(InMemoryJobQueue::with_workers(2));
    queue.register::<FlakyImport>().await;
    queue.start().await;

    queue.dispatch(&FlakyImport).await.unwrap();

    wait_until(|| FLAKY_SUCCEEDED.load(Ordering::SeqCst) == 1).await;
    queue.shutdown().await;
    assert_eq!(
        FLAKY_SUCCEEDED.load(Ordering::SeqCst),
        1,
        "eventually succeeded"
    );
    assert!(
        FLAKY_ATTEMPTS.load(Ordering::SeqCst) >= 2,
        "ran more than once (retried)"
    );
}

// ----------------------------------------------------- 3. fatal → dead letter

#[derive(Serialize, Deserialize)]
struct BadPayload;

#[async_trait::async_trait]
impl Job for BadPayload {
    const NAME: &'static str = "doc:bad_payload";
    async fn run(&self) -> Result<(), JobError> {
        // Permanent failure → skip retries, go straight to dead-letter.
        Err(JobError::Fatal("unprocessable".into()))
    }
}

#[tokio::test]
async fn fatal_job_goes_to_the_dead_letter_handler() {
    let dead = Arc::new(AtomicUsize::new(0));

    let queue = Arc::new(InMemoryJobQueue::with_workers(2));
    // Register the dead-letter sink BEFORE starting workers.
    let dead_for_cb = dead.clone();
    queue
        .on_dead_letter(move |dl| {
            let counter = dead_for_cb.clone();
            async move {
                assert_eq!(dl.name, "doc:bad_payload");
                assert!(dl.error.contains("unprocessable"));
                counter.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
    queue.register::<BadPayload>().await;
    queue.start().await;

    queue.dispatch(&BadPayload).await.unwrap();

    wait_until(|| dead.load(Ordering::SeqCst) == 1).await;
    queue.shutdown().await;
    assert_eq!(
        dead.load(Ordering::SeqCst),
        1,
        "fatal job was dead-lettered"
    );
}

// ------------------------------------------------- the page's app examples
//
// Each marked block is a code example of `docs/jobs.md`, copied verbatim;
// `every_locale_publishes_the_tested_snippets` keeps them in sync. `myblog`
// in the `main.rs` and worker examples is the module the job is in.

#[test]
fn every_locale_publishes_the_tested_snippets() {
    doc_snippet::assert_published(include_str!("jobs_doc.rs"), "jobs.md");
}

/// Step 1's `src/welcome_email.rs`, and the Step 3 dispatch line.
#[allow(dead_code)]
#[rustfmt::skip]
mod step1 {
    pub mod welcome_email {
        // doc-snippet:start
        // src/welcome_email.rs
        use rustango::jobs::{Job, JobError};
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        pub struct WelcomeEmail {
            pub to: String,
        }

        #[async_trait::async_trait]   // the scaffolded Cargo.toml has `async-trait`
        impl Job for WelcomeEmail {
            const NAME: &'static str = "welcome_email";

            async fn run(&self) -> Result<(), JobError> {
                tracing::info!(to = %self.to, "welcome email job ran");
                Ok(())
            }
        }
        // doc-snippet:end
    }

    use rustango::jobs::{InMemoryJobQueue, JobError, JobQueue};
    use welcome_email::WelcomeEmail;

    async fn dispatch(queue: &InMemoryJobQueue) -> Result<(), JobError> {
        // doc-snippet:start
        queue.dispatch(&WelcomeEmail { to: "ada@example.com".into() }).await?;
        // doc-snippet:end
        Ok(())
    }

    /// "The persistent queue (production)".
    #[cfg(feature = "jobs-postgres")]
    async fn persistent(pool: rustango::sql::Pool) -> Result<(), Box<dyn std::error::Error>> {
        // doc-snippet:start
        use rustango::jobs::DatabaseJobQueue;   // = rustango::jobs::pg::PgJobQueue
        use rustango::jobs::JobQueue;
        use std::sync::Arc;
        use std::time::Duration;

        // 1. Create the jobs table once at boot (idempotent, tri-dialect).
        DatabaseJobQueue::ensure_table_pool(&pool).await?;

        // 2. Build the queue over your pool + N workers, and start it.
        let queue = Arc::new(
            DatabaseJobQueue::with_workers_pool(pool.clone(), 4)
                .poll_interval(Duration::from_millis(50)),
        );
        queue.register::<WelcomeEmail>().await;
        queue.start().await;

        // 3. Dispatch exactly as before — now it's durable.
        queue.dispatch(&WelcomeEmail { to: "ada@example.com".into() }).await?;
        // doc-snippet:end
        Ok(())
    }
}

/// Step 4's `src/welcome_email.rs`.
#[cfg(feature = "email")]
#[rustfmt::skip]
mod app {
    /// `use myblog::urls;` in `main.rs`.
    #[cfg(feature = "batteries")]
    pub(crate) use crate::urls;

    pub mod welcome_email {
        // doc-snippet:start
        // src/welcome_email.rs
        use std::sync::Arc;

        use axum::{http::StatusCode, routing::post, Extension, Json, Router};
        use rustango::email::{BoxedMailer, Email};
        use rustango::jobs::{InMemoryJobQueue, Job, JobError, JobQueue};
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        pub struct WelcomeEmail {
            pub to: String,
        }

        #[async_trait::async_trait]
        impl Job for WelcomeEmail {
            const NAME: &'static str = "welcome_email";

            // Not called here: `register` below gives the queue a handler instead.
            async fn run(&self) -> Result<(), JobError> {
                tracing::info!(to = %self.to, "welcome email job ran");
                Ok(())
            }
        }

        /// Register the job with a handler that holds the mailer. Call it once at
        /// boot, before `start()`.
        pub async fn register<Q: JobQueue>(queue: &Q, mailer: BoxedMailer) {
            queue
                .register_with(move |job: WelcomeEmail| {
                    // The closure runs once per job, so each run takes its own copy.
                    let mailer = mailer.clone();
                    async move {
                        Email::new()
                            .to(job.to)
                            .subject("Welcome")
                            .body("Thanks for signing up.")
                            .send(&*mailer)
                            .await
                            .map_err(|e| {
                                if e.is_retryable() {
                                    JobError::Retryable(e.to_string()) // mail server down
                                } else {
                                    JobError::Fatal(e.to_string()) // bad address: no retry
                                }
                            })
                    }
                })
                .await;
        }

        #[derive(Deserialize)]
        pub struct Signup {
            pub email: String,
        }

        async fn signup(
            Extension(queue): Extension<Arc<InMemoryJobQueue>>,
            Json(form): Json<Signup>,
        ) -> StatusCode {
            // ... create the user here ...
            // `dispatch` returns at once; a worker sends the email.
            match queue.dispatch(&WelcomeEmail { to: form.email }).await {
                Ok(()) => StatusCode::CREATED,
                Err(e) => {
                    tracing::error!(error = %e, "could not queue the welcome email");
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }
        }

        pub fn router(queue: Arc<InMemoryJobQueue>) -> Router {
            Router::new()
                .route("/signup", post(signup))
                .layer(Extension(queue))
        }
        // doc-snippet:end
    }

    /// The `Post` model from Getting started, which the worker reads.
    #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
    pub mod blog {
        pub mod models {
            use rustango::sql::Auto;
            use rustango::Model;

            #[derive(Model, Debug, Clone)]
            #[rustango(table = "posts")]
            pub struct Post {
                #[rustango(primary_key)]
                pub id: Auto<i64>,
                #[rustango(max_length = 200)]
                pub title: String,
            }
        }
    }
}

#[cfg(feature = "batteries")]
#[path = "support/scaffold_urls.rs"]
mod urls;

/// Step 4's `src/main.rs`.
#[cfg(feature = "batteries")]
#[allow(dead_code)]
#[rustfmt::skip]
mod app_main {
    use super::app as myblog;

    // doc-snippet:start
    // src/main.rs: keep the scaffolded main(), which calls run()
    use std::sync::Arc;

    use myblog::urls;
    use rustango::jobs::{InMemoryJobQueue, JobQueue}; // NEW

    async fn run() -> Result<(), Box<dyn std::error::Error>> {
        let _ = dotenvy::dotenv();
        let loaded = rustango::config::Settings::load_from_env();
        let settings = loaded.as_ref().cloned().unwrap_or_default();
        let admin_settings = settings.clone();

        // NEW 1. The mailer, from the [mail] settings (see the email guide).
        let mailer = rustango::email::from_settings(&settings.mail)?;

        // NEW 2. Build the queue and register every job type BEFORE start().
        let queue = Arc::new(InMemoryJobQueue::with_workers(4));
        myblog::welcome_email::register(&*queue, mailer).await;
        queue
            .on_dead_letter(|dl| async move {
                tracing::error!(job = dl.name, attempts = dl.attempts, error = %dl.error,
                                "job dead-lettered");
            })
            .await;

        // NEW 3. Start the workers — tokio tasks that run inside THIS process.
        queue.start().await;
        let draining = Arc::clone(&queue);

        rustango::manage::Cli::new()
            // NEW 4. Mount the /signup route, which holds the queue.
            .api(urls::api().merge(myblog::welcome_email::router(queue)))
            .try_nest_with(urls::admin_prefix(&settings), move |pool| {
                urls::admin_router(pool, &admin_settings)
            })
            .with_health()
            .with_loaded_settings(loaded)
            // NEW 5. Drain the queue after the server stops (SIGINT and SIGTERM).
            .on_shutdown(move || async move { draining.shutdown().await })
            .run()
            .await
    }
    // doc-snippet:end
}

/// "A dedicated worker process (production)".
#[cfg(all(
    feature = "batteries",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
#[allow(dead_code)]
#[rustfmt::skip]
mod worker {
    use super::app as myblog;

    // doc-snippet:start
    // src/bin/worker.rs — run with `cargo run --bin worker`
    use std::sync::Arc;

    use myblog::blog::models::Post;
    use myblog::welcome_email::WelcomeEmail;
    use rustango::email::Email;
    use rustango::jobs::{DatabaseJobQueue, JobError, JobQueue};
    use rustango::sql::{FetcherPool, Pool};

    #[rustango::main]
    async fn main() -> Result<(), Box<dyn std::error::Error>> {
        // A worker never runs main.rs, so it loads `.env` and the settings itself.
        let _ = dotenvy::dotenv();
        let settings = rustango::config::Settings::load_from_env()?;
        let mailer = rustango::email::from_settings(&settings.mail)?;
        let pool = Pool::connect_from_env().await?; // DATABASE_URL
        DatabaseJobQueue::ensure_table_pool(&pool).await?;

        let queue = Arc::new(DatabaseJobQueue::with_workers_pool(pool.clone(), 8));
        // Register the SAME job types the web process dispatches.
        queue
            .register_with(move |job: WelcomeEmail| {
                let pool = pool.clone();
                let mailer = mailer.clone();
                async move {
                    let posts = Post::objects()
                        .fetch(&pool)
                        .await
                        .map_err(|e| JobError::Retryable(e.to_string()))?;
                    Email::new()
                        .to(job.to)
                        .subject("Welcome")
                        .body(format!("There are {} posts waiting for you.", posts.len()))
                        .send(&*mailer)
                        .await
                        .map_err(|e| JobError::Retryable(e.to_string()))
                }
            })
            .await;
        queue.start().await;

        // Blocks until Ctrl-C (SIGINT) *or* SIGTERM. `tokio::signal::ctrl_c()`
        // alone is SIGINT-only on Unix, so a worker waiting on it is killed
        // outright by `docker stop` / a pod eviction and drains nothing.
        rustango::shutdown::shutdown_signal().await;
        queue.shutdown().await;                     // drain in-flight, then exit
        Ok(())
    }
    // doc-snippet:end
}

#[cfg(feature = "email")]
#[tokio::test]
async fn register_with_hands_the_job_to_the_mailer() {
    use rustango::email::InMemoryMailer;

    let mailer = Arc::new(InMemoryMailer::new());
    let queue = Arc::new(InMemoryJobQueue::with_workers(1));
    app::welcome_email::register(&*queue, mailer.clone()).await;
    queue.start().await;

    // The /signup route queues the job and answers at once.
    let req = axum::http::Request::post("/signup")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"email":"ada@example.com"}"#))
        .unwrap();
    let res = tower::ServiceExt::oneshot(app::welcome_email::router(queue.clone()), req)
        .await
        .unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::CREATED);

    wait_until(|| mailer.sent().len() == 1).await;
    queue.shutdown().await;
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1, "the register_with closure sent the email");
    assert_eq!(sent[0].to, ["ada@example.com"]);
    assert_eq!(sent[0].subject, "Welcome");
}
