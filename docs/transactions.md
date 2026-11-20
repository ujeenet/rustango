# Transactions

An `atomic` block runs a group of statements as one unit: they all commit, or
none of them do. It works on PostgreSQL, MySQL (and MariaDB) and SQLite. Where
the backends differ, this page says so.

For a plain transaction handle (`transaction_pool`) and row locks, see the
[ORM guide](orm.md).

> **New to a term here?** *Transaction*, *rollback* — see the
> [glossary](glossary.md).

## Table of contents

- [A first block](#a-first-block)
- [What runs in the block](#what-runs-in-the-block)
- [Nested blocks](#nested-blocks)
- [After the commit](#after-the-commit)
- [When a statement fails](#when-a-statement-fails)
- [SQLite takes the write lock at BEGIN](#sqlite-takes-the-write-lock-at-begin)
- [Isolation levels and settings](#isolation-levels-and-settings)
- [Is this code in a block?](#is-this-code-in-a-block)
- [Get or create in a transaction](#get-or-create-in-a-transaction)
- [The `_on` forms](#the-_on-forms)
- [DDL in a block](#ddl-in-a-block)
- [Iterating in a block](#iterating-in-a-block)
- [Cancellation](#cancellation)
- [One transaction per request](#one-transaction-per-request)
- [Errors](#errors)

## A first block

The examples use this model and a `pool: &Pool`:

```rust
use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "accounts")]
pub struct Account {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(unique, max_length = 64)]
    pub email: String,
    pub balance: i64,
}
```

`rustango::sql::atomic(&pool, closure)` begins a transaction, runs the
closure, and commits when it returns `Ok`. An `Err` rolls everything back.

```rust
use rustango::sql::{atomic, ExecError, Pool};

async fn transfer(pool: &Pool, from: i64, to: i64, amount: i64) -> Result<(), ExecError> {
    let p = pool.clone(); // the closure must own what it uses
    atomic(pool, move |_tx| {
        Box::pin(async move {
            let mut a = Account::find_or_fail(from, &p).await?;
            let mut b = Account::find_or_fail(to, &p).await?;
            a.balance -= amount;
            b.balance += amount;
            a.save(&p).await?;
            b.save(&p).await?;
            Ok(()) // Ok commits; an Err (or a failed `?`) rolls both saves back
        })
    })
    .await
}
```

The closure gets an owned clone of the pool, not a borrow: the block's future
must not borrow the caller. `Pool` is cheap to clone. The
`rustango::atomic!(&pool, |tx| { … })` macro writes the `Box::pin` for you.

## What runs in the block

Inside the closure, every ORM call given the same pool (or a clone of it) runs
on the block's transaction:

- reads: `fetch`, `find`, `count`, `get_or_create`, …
- writes: `insert`, `save`, `delete`, bulk writes, queryset updates
- raw SQL: `raw_query_pool`, `raw_execute_pool`
- DDL (see [DDL in a block](#ddl-in-a-block))
- framework writes: audit rows, fixtures, ViewSet and admin writes, media, SSO
  sign-up and invites

They see the block's uncommitted rows, and its rollback undoes them. On SQLite,
and on MySQL at its default level (repeatable read), all reads in the block
see one snapshot.

These run **outside** the block, on their own connection: `pool.acquire()`,
the cache, the job claim, tenancy registry changes, a task
started with `tokio::spawn`, and a pool connected on its own (even to the same
database). They do not see the block's writes, and they can wait on locks the
block holds.

The closure's argument is an `AtomicTx`. `tx.lock().await?` gives a `TxGuard`
that derefs to `PoolTx`, for the `_tx` helpers and the `_on` forms:
`acc.insert_tx(&mut *guard)`. Run one statement at a time: while a guard is
held, or when two statements run at once (`join!`), calls on the pool return
`ExecError::NestedAtomic`. Drop the guard before the next `&pool` call.

**Cost.** A read or raw statement costs one round trip. An ORM write costs
three, because it runs in its own savepoint (so a failed write leaves the block
usable). For many rows, write through the guard (`insert_tx`) or a bulk write.

## Nested blocks

`atomic` on the same pool inside a block opens a savepoint. Its `Err` rolls
back only its own writes. An `Err` from the outer block rolls back everything,
nested work included.

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        account("a@example.com", 0).insert(&p).await?;
        let q = p.clone();
        let inner = atomic(&p, move |_sp| {
            Box::pin(async move {
                account("b@example.com", 0).insert(&q).await?;
                // Stands in for any failure inside the nested block.
                Err::<(), _>(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound))
            })
        })
        .await;
        assert!(inner.is_err()); // b@ is rolled back, a@ is kept
        Ok(())
    })
})
.await?;
```

(`account(email, balance)` builds an `Account` with `id: Auto::Unset`.)

Nesting goes by pool object: pass the request's pool down rather than
connecting again. A dropped or panicking nested block is rolled back. If a
savepoint statement itself fails, the whole block rolls back with
`ExecError::AtomicAborted`. PostgreSQL slows down past 64 savepoints in one
transaction, and every ORM write is one, so keep blocks with many writes short.

## After the commit

`rustango::sql::on_commit(callback)` queues a callback to run after the
outermost block commits. It never runs on rollback, and a rolled-back nested
block drops its callbacks. Callbacks run in the order they were queued.

```rust
use rustango::sql::on_commit;

let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let mut acc = account("new@example.com", 0);
        acc.insert(&p).await?;
        on_commit(move || {
            tokio::spawn(async move { /* send the welcome email */ });
        });
        Ok(())
    })
})
.await?;
```

The callback is sync: spawn async work from it. A callback that panics is
logged and the others still run. `on_commit` outside a block panics.

## When a statement fails

| Backend | A failed read or raw statement | A failed ORM write |
|---|---|---|
| PostgreSQL | Aborts the block: later statements and `atomic` return `ExecError::AtomicAborted` | Undone alone (its savepoint); the block goes on |
| MySQL, MariaDB | Undone alone; the block goes on | Undone alone; the block goes on |
| SQLite | Undone alone; the block goes on | Undone alone; the block goes on |

A closure that ignores an error and returns `Ok` commits the rest of the block
(on PostgreSQL after an aborted read, `atomic` returns `AtomicAborted`
instead).

Some errors end the whole transaction on the server: a MySQL deadlock (1213), a
MySQL lock-wait timeout (1205) when the server rolls back on it, an SQLite
automatic rollback, a lost connection. Every later ORM statement in the block,
and `atomic` itself, then returns `ExecError::AtomicAborted`. MariaDB reports a
`nowait()` miss as 1205; with the default `innodb_rollback_on_timeout = OFF` it
undoes only the statement.

**Keep going after a failed read.** To try a step that may fail (a `nowait()`
lock, a probe of a table that may not exist), put it in a nested block. Its
failure then undoes only that block, on every backend:

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let q = p.clone();
        // A failed read in a nested block undoes only that block, even on PG.
        let locked = atomic(&p, move |_sp| {
            Box::pin(async move {
                Account::objects()
                    .filter("email", "a@example.com")
                    .nowait()
                    .fetch(&q)
                    .await
            })
        })
        .await;
        match locked {
            Ok(_rows) => { /* got the lock */ }
            Err(_) => { /* row busy: the outer block goes on */ }
        }
        Ok(())
    })
})
.await?;
```

Raw SQL that ends the transaction (`COMMIT`, `ROLLBACK`, or a statement MySQL
commits on) makes the block return `ExecError::AtomicEndedEarly`. Statements
after it ran without a transaction, so do not retry blindly.

## SQLite takes the write lock at BEGIN

On SQLite the outermost block begins with `BEGIN IMMEDIATE`. A block that reads
and then writes does not fail with `SQLITE_BUSY` at its first write; instead a
second writer waits at its own `BEGIN`. Other connections that write wait
`busy_timeout` (5 seconds by default) and then fail with `SQLITE_BUSY`; on an
in-memory database they wait with no timeout. Keep SQLite blocks short.

`AtomicOptions::deferred()` begins with a plain `BEGIN` instead: a block that
only reads takes no write lock, and its first write after a read fails at once
with `SQLITE_BUSY` if another connection wrote meanwhile. It has no effect on
PostgreSQL and MySQL, whose `BEGIN` never locks.

## Isolation levels and settings

`atomic_with(&pool, options, closure)` begins the outermost block with an
isolation level, PostgreSQL settings, or both:

```rust
use rustango::sql::{atomic_with, raw_query_pool, AtomicOptions, Isolation, TxSetting};

let opts = AtomicOptions::new()
    .isolation(Isolation::RepeatableRead)
    .setting(TxSetting::new("app.account_id", "42")?); // PostgreSQL only
let p = pool.clone();
let seen: Vec<(Option<String>,)> = atomic_with(pool, opts, move |_tx| {
    Box::pin(async move {
        raw_query_pool("SELECT current_setting('app.account_id', true)", vec![], &p).await
    })
})
.await?;
```

- **Levels:** `Isolation::ReadCommitted`, `RepeatableRead`, `Serializable`.
  PostgreSQL begins with `BEGIN ISOLATION LEVEL …`. MySQL sets the level for
  that transaction only; the next block on the connection is back to the
  server default. SQLite accepts any level: it is always serializable.
  Without a level, the block uses the server default (PostgreSQL: read
  committed; MySQL: repeatable read).
- **Settings** (`TxSetting`) are PostgreSQL only, for example `app.account_id`
  for row security policies. Each is set with `set_config(name, value, true)`
  and is gone when the block ends. The value is bound. The name must be dotted
  (`prefix.name`) with every part a plain identifier, so `search_path` or
  `role` return `ExecError::InvalidTxSetting`. On MySQL and SQLite, settings
  return `ExecError::Sql(SqlError::OperatorNotSupportedInDialect)` before the
  block begins.
- **Nested blocks** cannot change these: `atomic_with` with options inside a
  block on the same pool returns `ExecError::InAtomic`.

## Is this code in a block?

`rustango::sql::in_atomic(&pool)` is `true` while the running task is inside a
block on `pool` (or a clone of it), so ORM calls given `pool` run on its
transaction. It is `false` outside, and for any other pool. It stays `true` in
a block that failed or ended early.

## Get or create in a transaction

`get_or_create_tx` and `update_or_create_tx` take an open `PoolTx`. In a block,
lock the guard and pass it:

```rust
use rustango::sql::get_or_create_tx;

let email = "a@example.com".to_owned();
let (acc, created) = atomic(pool, move |tx| {
    Box::pin(async move {
        let mut g = tx.lock().await?;
        let filter = Account::objects().filter("email", email.clone());
        get_or_create_tx(
            filter,
            move |t| {
                Box::pin(async move {
                    let mut acc = Account { id: Auto::Unset, email, balance: 0 };
                    acc.insert_tx(t).await?;
                    Ok(acc)
                })
            },
            &mut *g,
        )
        .await
    })
})
.await?;
```

```rust
use rustango::sql::update_or_create_tx;

let (acc, created) = atomic(pool, move |tx| {
    Box::pin(async move {
        let mut g = tx.lock().await?;
        update_or_create_tx(
            Account::objects().filter("email", "b@example.com"),
            |t, mut acc: Account| {
                Box::pin(async move {
                    acc.balance += 10;
                    acc.save_tx(t).await?;
                    Ok(acc)
                })
            },
            |t| {
                Box::pin(async move {
                    let mut acc = Account {
                        id: Auto::Unset,
                        email: "b@example.com".into(),
                        balance: 10,
                    };
                    acc.insert_tx(t).await?;
                    Ok(acc)
                })
            },
            &mut *g,
        )
        .await
    })
})
.await?;
```

**Races.** The create closure runs in a savepoint. When two transactions create
the same row at once, the loser's insert hits the unique constraint: its
savepoint rolls back and it reads the winner's row (`created == false`;
`update_or_create_tx` then updates it). The transaction stays usable. This
needs a `UNIQUE` constraint on the filtered columns. A `get_or_create_tx` inside
another's create closure (a parent row, then the child) works the same way.

| Backend and level | The loser gets |
|---|---|
| PostgreSQL read committed (default) | The winner's row |
| PostgreSQL repeatable read | The unique violation error |
| PostgreSQL serializable | A serialization failure (40001) |
| MySQL, MariaDB | The winner's row (re-read with a locking read) |
| SQLite | No race: blocks run one at a time |

Under repeatable read or serializable on PostgreSQL, retry the whole block.

The pool forms `get_or_create` and `update_or_create` behave the same inside a
block: their reads join it and the create closure runs in a savepoint. Do not
hold the guard across them.

## The `_on` forms

The `_on` methods take any executor: a `&Pool`, a held connection
(`&mut PoolConn` from `pool.acquire()`), or an open transaction (`&mut PoolTx`
from `transaction_pool`, or `&mut *guard` in a block). The statement runs on
what you pass.

- `fetch_on`, `insert_on`, `save_on`, `delete_on`, `upsert_on`
- `Model::bulk_insert_on`, `Model::bulk_upsert_on`, `Model::bulk_insert_or_ignore_on`
- `rustango::sql::bulk_update_on`
- `rustango::sql::fetch_dates_on`, `rustango::sql::fetch_datetimes_on`
- `rustango::sql::fetch_with_prefetch_on`
- `rustango::sql::model_shortcuts::increment_one_on`
- queryset updates and deletes: `.update().execute_on(..)`, `.delete_on(..)`

```rust
use rustango::core::{BulkUpdateQuery, Model as _, SqlValue}; // `Model` brings `Account::SCHEMA` into scope
use rustango::sql::model_shortcuts::increment_one_on;
use rustango::sql::{bulk_update_on, transaction_pool};

let mut tx = transaction_pool(pool).await?;
let mut rows = vec![
    Account { id: Auto::Unset, email: "a@example.com".into(), balance: 0 },
    Account { id: Auto::Unset, email: "b@example.com".into(), balance: 0 },
];
Account::bulk_insert_on(&mut rows, &mut tx).await?;
// MySQL leaves `Auto` ids unset after a bulk insert: read them back.
let ids: Vec<i64> = Account::objects()
    .order_by(&[("id", false)])
    .fetch_on(&mut tx)
    .await?
    .iter()
    .filter_map(|a| a.id.get().copied())
    .collect();
let set_100 = BulkUpdateQuery::new(
    Account::SCHEMA,
    vec!["balance"],
    ids.iter()
        .map(|id| vec![SqlValue::from(*id), SqlValue::from(100_i64)])
        .collect(),
);
bulk_update_on(&mut tx, &set_100).await?;
increment_one_on::<Account, _>("id", SqlValue::from(ids[0]), "balance", 5, &mut tx).await?;
tx.commit().await?; // or drop `tx` to roll it all back
```

On an audited model the derived `_on` writes and `increment_one_on` return
`ExecError::AuditUnsupported`: they cannot write the audit rows. Use the pool
forms there.

## DDL in a block

- **PostgreSQL and SQLite:** DDL runs in the block's transaction and rolls back
  with it.
- **MySQL:** DDL commits the transaction. `run_ddl_idempotent` (and the
  `ensure_table` helpers built on it) refuses inside a block with an
  `sqlx::Error::Protocol` whose message starts `DDL on MySQL is refused inside
  an atomic block on its pool`. Raw DDL through `raw_execute_pool` runs, commits
  the work so far, and the block returns `ExecError::AtomicEndedEarly`.
- **Any backend:** a migration with `"atomic": false` returns
  `MigrateError::Exec(ExecError::InAtomic { .. })`.

Run DDL and migrations before or after the block.

## Iterating in a block

Inside a block on the same pool, `.iterator(n)` runs each chunk on the block's
transaction. It sees the block's writes, keeps row locks from
`select_for_update()`, and pages by primary key (`WHERE pk > last`) instead of
`OFFSET`. Rows the loop moves out of the filter are not skipped:

```rust
let p = pool.clone();
atomic(pool, move |_tx| {
    Box::pin(async move {
        let mut it = Account::objects()
            .filter("balance", 0_i64)
            .order_by(&[("id", false)]) // primary key only, or no order
            .iterator(500)?;
        while let Some(chunk) = it.next_chunk(&p).await? {
            for mut acc in chunk {
                acc.balance = 1; // leaves the filter; paging by key still reaches every row
                acc.save(&p).await?;
            }
        }
        Ok(())
    })
})
.await?;
```

The queryset must be ordered by the primary key (either direction) or not at
all, select the primary key, and not be a `union` / `intersection` /
`difference`. Otherwise `next_chunk` returns `ExecError::InAtomic`.

## Cancellation

Dropping a block's future (a `tokio::time::timeout`, a client that goes away)
rolls the block back, and its `on_commit` callbacks never run:

```rust
use std::time::Duration;

let p = pool.clone();
let slow = atomic(pool, move |_tx| {
    Box::pin(async move {
        Account { id: Auto::Unset, email: "late@example.com".into(), balance: 0 }
            .insert(&p)
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await; // stands in for slow work
        Ok(())
    })
});
// Dropping the block (here on timeout) rolls it back.
assert!(tokio::time::timeout(Duration::from_millis(100), slow).await.is_err());
```

If a statement was running when the block was dropped, the server does not
stop it. The connection stays busy until the server finishes that statement;
then the rollback runs and the connection goes back to the pool. Meanwhile a
small pool can be short of connections. A dropped nested block is rolled back
on the block's next use.

## One transaction per request

`atomic_requests` runs each write request to your app's routes in one `atomic`
block on the request's pool. A response that is not 2xx or 3xx, or a panic,
rolls back every write the handler made there.

```rust
use rustango::atomic_request::AtomicRequests;
use rustango::sql::{AtomicOptions, Isolation};

// Tenancy: each block runs on the request's tenant pool.
let app = rustango::server::Builder::from_env().await?
    .api(routes)
    .atomic_requests(AtomicRequests::new())
    .into_router()
    .await?;

// One database: each block runs on the builder's pool.
rustango::server::AppBuilder::from_env().await?
    .api(other_routes)
    .atomic_requests(AtomicRequests::new().options(
        AtomicOptions::new().isolation(Isolation::ReadCommitted),
    ))
    .serve("0.0.0.0:8080")
    .await?;
```

- Only POST, PUT, PATCH, DELETE and other unsafe methods. GET, HEAD, OPTIONS,
  TRACE and QUERY run without a transaction, so a GET route that writes (an
  SSO callback) keeps autocommit.
- Only your app's own routes: the tenant admin, login, SSO and operator
  console are not wrapped.
- Every tenant extractor in the request (`TenantScope`, `Tenant`) serves the
  pool the block runs on, even when the pool cache evicts it meanwhile. A
  request with no tenant passes through; a tenant whose pool cannot be found
  gets 500, never the registry pool.
- Each write request holds one pool connection until it ends. Boot refuses
  `max_cached_scoped_pools = 0` or `max_cached_database_pools = 0`, and warns
  when `scoped_pool_max_connections` is below 8.

**Order.** The builder puts the block inside its own layers (panic guard,
access log, tracing, tenant dispatch) and around your router. Layers on your
router (sessions, CSRF, auth, idempotency) run inside the block: what they
write on the pool commits or rolls back with the request. An
`IdempotencyLayer` inside the block stores a response only after the
`COMMIT`. On a hand-built router, add `AtomicRequestLayer::new(pool)` or
`AtomicRequestLayer::tenant()` before the layers that must see the final
response.

**A failed commit.** The handler's response is dropped, with its cookies and
redirect, and a new one is sent. Nothing is retried: handlers have side
effects.

| Cause | Response |
|---|---|
| Serialization failure (40001), deadlock, MySQL lock-wait timeout (1205), `SQLITE_BUSY`, no free connection | 503 with `Retry-After: 1` |
| `AtomicAborted`, `AtomicEndedEarly`, any other error | 500, logged with the request id |

**What escapes the block.**

- A `tokio::spawn`ed task commits on its own; spawn from `on_commit(|| ..)`.
- A streaming body runs after the `COMMIT`.
- Two statements at once in a handler (`join!`) return `NestedAtomic`.
- `outside_atomic(fut)` runs `fut` outside the block, for a write that must
  survive a rollback. The framework writes security audit rows and logout
  stamps this way. On the request's own pool it needs a second connection: on
  SQLite it waits for the block's write lock, on PostgreSQL it can wait on a
  row the block locked.

**`Tenant::conn`.** Inside a block on the tenant's pool, `t.conn()` lends the
block's own connection, so its statements commit or roll back with the
request. ORM calls on the pool return `NestedAtomic` while it is lent;
`t.pool()` gives it back. `t.pool_conn()` and `t.into_conn()` return
`InAtomic` there.

**Per backend.** PostgreSQL: a statement error the handler ignores aborts the
block, and the request answers 500. MySQL: `REPEATABLE READ` keeps one
snapshot for the whole request, so `Isolation::ReadCommitted` usually fits a
web request better; DDL commits the block early (500). SQLite: blocks begin
deferred, so a POST that only reads takes no write lock; a write after a read
fails at once with `SQLITE_BUSY` (503) when another connection wrote
meanwhile.

**Cost.** `BEGIN` and `COMMIT` per write request, a check before the commit,
and a savepoint around each ORM write. On a local PostgreSQL a one-write POST
adds about 1 ms at p99 alone, more under load.

## Errors

| Error | Meaning |
|---|---|
| `ExecError::NestedAtomic` | A statement ran while a `TxGuard` was held, or two ran at once (`join!`) |
| `ExecError::AtomicAborted` | The transaction was rolled back after a failed statement or savepoint |
| `ExecError::AtomicEndedEarly` | Raw SQL committed or ended the transaction; some writes may be committed |
| `ExecError::InAtomic` | The operation is refused inside a block on its pool |
| `ExecError::InvalidTxSetting` | A `TxSetting` name is not a dotted `prefix.name` |
