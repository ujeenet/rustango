# Background jobs

Some work shouldn't happen during a request — sending a welcome email, resizing
an upload, syncing a third-party API. Doing it inline makes the user wait and
couples the response to a flaky external call. A **background job** moves that
work onto a queue: the handler returns immediately, and a pool of workers runs
the job moments later, with **automatic retries** and a **dead-letter** path for
failures. This is Django-Q / Celery / Laravel queues, in Rust.

[![Background jobs in rustango: a handler dispatches a Job onto a queue, worker tasks run it, retryable failures back off and retry, fatal ones go to a dead-letter handler](img/jobs.png)](img/jobs.png)

> **New to a term here?** *queue*, *worker*, *retry/backoff*, *dead-letter* — see
> the [glossary](glossary.md).

> **Source:** `rustango::jobs` (`Job`, `JobQueue`, `InMemoryJobQueue`,
> `JobError`, `JobDeadLetter`) and `rustango::jobs::DatabaseJobQueue` (the
> table-backed queue, alias of `pg::PgJobQueue`) — behind the `jobs` feature (on
> by default).
>
> **Runnable version:** the in-memory, retry, and dead-letter snippets are
> copied from [`jobs_doc.rs`](../crates/rustango/tests/jobs_doc.rs)
> (`cargo test -p rustango --test jobs_doc`); the persistent queue is dogfooded
> on SQLite by
> [`jobs_sqlite_live.rs`](../crates/rustango/tests/jobs_sqlite_live.rs)
> (`cargo test -p rustango --features sqlite,jobs-postgres --test jobs_sqlite_live`).

## Table of contents

- [Step 1 — Define a job](#step-1--define-a-job)
- [Step 2 — Start a queue](#step-2--start-a-queue)
- [Step 3 — Dispatch from a handler](#step-3--dispatch-from-a-handler)
- [Step 4 — Wire it into your app](#step-4--wire-it-into-your-app) — the full example
- [Running the workers (CLI + production)](#running-the-workers-cli--production)
- [Retries and backoff](#retries-and-backoff)
- [The dead-letter handler](#the-dead-letter-handler)
- [The persistent queue (production)](#the-persistent-queue-production)
- [Reference](#reference)
- [See also](#see-also)

---

## Step 1 — Define a job

A job is a serializable struct that implements `Job`. The payload is what gets
queued (serialized to JSON); `run()` is the work. `NAME` routes a queued payload
back to its handler, so it must be unique.

Scaffold a skeleton with the CLI — `cargo run -- make:job WelcomeEmail` — or
write it by hand:

```rust
use rustango::jobs::{Job, JobError};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct WelcomeEmail {
    user_id: i64,
}

#[async_trait::async_trait]   // add `async-trait` to your Cargo.toml
impl Job for WelcomeEmail {
    const NAME: &'static str = "welcome_email";

    async fn run(&self) -> Result<(), JobError> {
        // ... look up the user and send the email
        Ok(())
    }
}
```

`run()` returns:

- `Ok(())` — done.
- `Err(JobError::Retryable(msg))` — transient; the worker retries with backoff.
- `Err(JobError::Fatal(msg))` — permanent; skip retries, dead-letter it now.

Override `const MAX_ATTEMPTS: u32 = 3;` on the impl to change the retry ceiling
(default 5).

---

## Step 2 — Start a queue

For a single process (and for dev and tests), `InMemoryJobQueue` runs jobs on
tokio worker tasks — no database. **Register every job type, then `start()` the
workers** (jobs aren't picked up until you do):

```rust
use rustango::jobs::{InMemoryJobQueue, JobQueue};
use std::sync::Arc;

let queue = Arc::new(InMemoryJobQueue::with_workers(4));   // 4 worker tasks
queue.register::<WelcomeEmail>().await;                    // before start/dispatch
queue.start().await;

// ... app runs ...

queue.shutdown().await;   // on shutdown: drain in-flight jobs, then stop
```

Keep the `Arc<InMemoryJobQueue>` in your app state so handlers can reach it.

> **In-memory means in-memory.** Jobs queued or in-flight are **lost on
> restart**. For anything you can't afford to drop, use the
> [persistent queue](#the-persistent-queue-production).

---

## Step 3 — Dispatch from a handler

Once the queue is running, dispatch is one call — it enqueues and returns
immediately, so the request doesn't wait for the work:

```rust
queue.dispatch(&WelcomeEmail { user_id: 42 }).await?;
```

A worker picks it up and runs `run()` moments later. Verified end to end: three
dispatched jobs all run on the workers.

```rust
for user_id in 1..=3 {
    queue.dispatch(&WelcomeEmail { user_id }).await.unwrap();
}
// → all three WelcomeEmail::run() calls execute on the worker pool
```

---

## Step 4 — Wire it into your app

Putting Steps 1–3 together: build the queue and register every job **once at
boot**, start the workers, hand the queue to your routes so handlers can reach
it, then drain on shutdown. The queue lives in your `main.rs`:

```rust
// src/main.rs
use std::sync::Arc;
use rustango::jobs::{InMemoryJobQueue, JobQueue};
use rustango::manage::Cli;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Build the queue + register every job type BEFORE starting workers.
    let queue = Arc::new(InMemoryJobQueue::with_workers(4));
    queue.register::<WelcomeEmail>().await;
    queue
        .on_dead_letter(|dl| async move {
            tracing::error!(job = dl.name, attempts = dl.attempts, error = %dl.error,
                            "job dead-lettered");
        })
        .await;

    // 2. Start the workers — tokio tasks that run inside THIS process.
    queue.start().await;

    // 3. Share the queue with handlers (axum extension/state).
    let app = urls::api().layer(axum::Extension(queue.clone()));

    // 4. Boot the server. Cli::run() blocks until Ctrl-C / SIGTERM.
    Cli::new().api(app).with_health().run().await?;

    // 5. On shutdown: drain in-flight jobs, then stop.
    queue.shutdown().await;
    Ok(())
}
```

A handler dispatches by reading the queue back out of the request:

```rust
use axum::Extension;
use std::sync::Arc;
use rustango::jobs::{InMemoryJobQueue, JobQueue};

async fn signup(Extension(queue): Extension<Arc<InMemoryJobQueue>>) -> StatusCode {
    // ... create the user ...
    queue.dispatch(&WelcomeEmail { user_id: 42 }).await.ok();  // returns instantly
    StatusCode::CREATED
}
```

> **Store the concrete queue type.** The `JobQueue` trait is **not object-safe**
> (its `register`/`dispatch` are generic), so you can't hold `Arc<dyn JobQueue>`
> in state — keep the concrete `Arc<InMemoryJobQueue>` (or `Arc<DatabaseJobQueue>`).

---

## Running the workers (CLI + production)

`start()` spawns the workers as **tokio tasks inside the current process**. So
when you launch your app with the CLI — `cargo run` (which runs the server) —
the workers run right alongside it. For most apps that's all you need: **one
process serves requests *and* drains the queue**; there's no separate worker
command.

```bash
cargo run                 # starts the server AND the in-process workers
cargo run -- make:job WelcomeEmail   # scaffold a new job type
```

### A dedicated worker process (production)

At scale you often want workers **separate** from the web tier — so a traffic
spike can't starve jobs, and you scale each independently. With the
[persistent queue](#the-persistent-queue-production) every process pulls from the
same `rustango_jobs` table, so just run a second, server-less binary that builds
the queue, starts it, and blocks until a signal:

```rust
// src/bin/worker.rs — run with `cargo run --bin worker`
use std::sync::Arc;
use rustango::jobs::{DatabaseJobQueue, JobQueue};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = /* connect your tri-dialect Pool */;
    DatabaseJobQueue::ensure_table_pool(&pool).await?;

    let queue = Arc::new(DatabaseJobQueue::with_workers_pool(pool, 8));
    queue.register::<WelcomeEmail>().await;   // register the SAME job types
    queue.start().await;

    tokio::signal::ctrl_c().await?;            // block until Ctrl-C / SIGTERM
    queue.shutdown().await;                     // drain in-flight, then exit
    Ok(())
}
```

Deploy it as its own container/service and scale to **N replicas** — they all
pull from the shared table safely. The web process then only needs to
`dispatch` (it doesn't have to `start()` workers). Pair the worker with a
periodic [`reclaim_stuck_jobs_pool`](#the-persistent-queue-production) sweep to
recover jobs from a crashed worker.

---

## Retries and backoff

A job that returns `Retryable` is re-queued with **exponential backoff** (1s, 2s,
4s, 8s, …) up to `MAX_ATTEMPTS`. Use it for transient failures — a timeout, a
rate-limited API, a deadlock:

```rust
#[async_trait::async_trait]
impl Job for FlakyImport {
    const NAME: &'static str = "flaky_import";
    async fn run(&self) -> Result<(), JobError> {
        match call_external_api().await {
            Ok(_)  => Ok(()),
            Err(e) => Err(JobError::Retryable(e.to_string())),   // try again later
        }
    }
}
```

The backing test dispatches a job that fails once then succeeds, and asserts it
**ran more than once and eventually succeeded** — the retry actually happens.

---

## The dead-letter handler

When a job exhausts its retries — or returns `Fatal` immediately — it's handed
to the **dead-letter** callback instead of vanishing. Register one (before
`start`) to log, alert, or persist the failure:

```rust
queue
    .on_dead_letter(|dl| async move {
        // dl: JobDeadLetter { name, payload, attempts, error }
        tracing::error!(job = dl.name, attempts = dl.attempts, error = %dl.error,
                        "job dead-lettered");
    })
    .await;
```

A `Fatal` error skips retries and lands here on the first attempt:

```rust
async fn run(&self) -> Result<(), JobError> {
    Err(JobError::Fatal("unprocessable payload".into()))   // → dead-letter now
}
```

The test confirms the callback fires exactly once for a `Fatal` job, with the
job's `name` and `error` intact.

---

## The persistent queue (production)

`InMemoryJobQueue` is single-process and forgets on restart. For real
deployments — multiple workers, multiple replicas, jobs that must survive a
crash — use **`DatabaseJobQueue`** (the same type as `PgJobQueue`). Jobs live in
a `rustango_jobs` table; workers pick them up with a transaction-bounded
`UPDATE … RETURNING`, so it's safe across processes. It's **tri-dialect**
(PostgreSQL, MySQL, SQLite). The `Job` definitions from Step 1 are unchanged.

```rust
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
queue.dispatch(&WelcomeEmail { user_id: 42 }).await?;
```

**Recovering crashed workers.** If a worker dies mid-job, the row stays locked.
Run a periodic sweep to release locks older than a threshold so another worker
picks the job back up:

```rust
// e.g. from a scheduled task every few minutes:
let reclaimed = DatabaseJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(300)).await?;
```

Both the dispatch-and-run flow and `reclaim_stuck_jobs_pool` are dogfooded
against SQLite in `jobs_sqlite_live.rs`.

---

## Reference

**Backends**

| Backend | Use for | Survives restart? |
|---|---|---|
| `InMemoryJobQueue` | single process, dev, tests | no |
| `DatabaseJobQueue` (`PgJobQueue`) | production; multi-worker / multi-replica | yes (`rustango_jobs` table) |

**`JobError`**

| Variant | Effect |
|---|---|
| `Retryable(String)` | re-queue with exponential backoff, up to `MAX_ATTEMPTS` |
| `Fatal(String)` | skip retries → dead-letter immediately |
| `Queue(String)` | internal queue error (serialization/registration) |

**`JobQueue` methods:** `register::<T>()` · `dispatch(&payload)` · `start()` ·
`shutdown()` · `pending_count()`. The `DatabaseJobQueue` adds
`ensure_table_pool`, `with_workers_pool`, `poll_interval`, and
`reclaim_stuck_jobs_pool`.

---

## See also

- [Scheduler](manage.md) — for *time-based* recurring work (cron-style), as
  opposed to on-demand jobs.
- [Email](email.md) — the canonical "do it in a job" workload.
- [Caching](caching.md) — the other way to keep request handlers fast.
- [Signals](orm.md) — fire-and-forget hooks that often *dispatch* a job.
