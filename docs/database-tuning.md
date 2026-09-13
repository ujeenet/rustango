# Database tuning

Every database connection your app makes comes from a **pool** — a set of
connections kept open and handed out to requests. The pool's defaults are
chosen by the driver, not by your workload, and the defaults are wrong for
most production deployments in at least one way.

This page explains what each knob does, what goes wrong when it is left
alone, and where to set it.

## Why tune at all

Four failures, each caused by a different default:

**Your tenth concurrent request waits.** The driver's default pool holds
**10** connections. Request eleven does not fail — it *queues*, and your
p99 latency climbs while CPU sits idle. Nothing in the logs says "pool
exhausted"; you see slow requests.

**One unreachable database takes down the whole server.** When a
connection cannot be acquired, the caller waits. The driver's default is
30 seconds, which is a batch-tool number: on a request path it means a
worker is pinned for half a minute per request, so an outage of one
database saturates every worker and takes down surfaces that never touch
it. Rustango defaults this to **5 seconds** for that reason.

**Connections die quietly and the next request pays.** A load balancer,
a firewall, or PostgreSQL's own `idle_in_transaction_session_timeout`
will close a connection that has sat idle. Your pool does not know. The
next request to borrow it gets a broken pipe — intermittently, under low
traffic, which is the hardest kind of bug to reproduce.

**A failover or a credential rotation does not take effect.** Pooled
connections are long-lived by design. After a failover, or after a
rotated password, a pool can keep using connections opened against the
old server or the old credentials until something forces them closed.

## Where to set it

Three places. **Higher in this list wins**, so a deploy-time emergency
override never needs a config push and a restart of your config pipeline.

| | where | use it for |
|---|---|---|
| 1 | environment variables | per-deployment overrides, secrets managers, emergency retuning |
| 2 | `[database]` in your settings tier | the values your project normally runs with, in version control |
| 3 | framework defaults | what you get when you set nothing |

### In a settings tier

Settings live in `config/*_settings.toml`, one file per environment.
Put the values your project normally runs with in the tier they belong
to — development values in `dev_settings.toml`, production values in
`prod_settings.toml`:

```toml
[database]
pool_max_size             = 50
pool_min_size             = 5
pool_acquire_timeout_secs = 5
pool_idle_timeout_secs    = 600
pool_max_lifetime_secs    = 1800
```

### As environment variables

Every knob has an environment override, which takes precedence over the
TOML:

```bash
RUSTANGO_DB_MAX_CONNECTIONS=50
RUSTANGO_DB_MIN_CONNECTIONS=5
RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS=5
RUSTANGO_DB_IDLE_TIMEOUT_SECS=600
RUSTANGO_DB_MAX_LIFETIME_SECS=1800
```

A value that is not a positive whole number is **ignored with a warning**
rather than obeyed — a typo should not silently take a bound to zero, and
should not fail a boot either.

### One ordering rule

Settings are applied to pools by `Cli::with_settings(...)`. Call it
**before** anything opens a database connection. A pool built earlier
runs on environment defaults, and the framework logs a warning naming how
many pools that happened to, rather than leaving you to discover it under
load.

## The parameters

| setting | environment variable | default | what it does |
|---|---|---|---|
| `pool_max_size` | `RUSTANGO_DB_MAX_CONNECTIONS` | 10 (driver) | Most connections the pool will open. Requests beyond this queue. |
| `pool_min_size` | `RUSTANGO_DB_MIN_CONNECTIONS` | 0 (driver) | Connections kept open even when idle. |
| `pool_acquire_timeout_secs` | `RUSTANGO_DB_ACQUIRE_TIMEOUT_SECS` | 5 | How long a caller waits for a connection before erroring. |
| `pool_idle_timeout_secs` | `RUSTANGO_DB_IDLE_TIMEOUT_SECS` | driver default | Close a connection that has sat idle this long. |
| `pool_max_lifetime_secs` | `RUSTANGO_DB_MAX_LIFETIME_SECS` | driver default | Close a connection this old regardless of use. |

Leaving a knob unset is not the same as setting it to zero. Unset means
*the driver's own default applies* — the behaviour you had before the
knob existed.

### `pool_max_size`

The ceiling on concurrent database work. Raise it when requests are
queueing for connections; the symptom is latency that grows with traffic
while the database itself is not busy.

**It is a ceiling, not a target** — the pool opens connections on demand
and only up to this number.

The important constraint is on the *other* side: your database server has
its own connection limit (PostgreSQL's `max_connections`, typically 100).
The sum of every pool across every replica must stay under it, or new
connections are refused. Ten pods with `pool_max_size = 50` is 500
connections against a server that allows 100.

### `pool_min_size`

Connections kept open even when nothing is using them. The point is
latency: with `0`, the first request after a quiet period pays the full
TCP, TLS and authentication round-trip before any query runs.

Set it to cover your idle baseline, not your peak. Connections held open
cost resources on the server too.

### `pool_acquire_timeout_secs`

How long a caller waits for a connection before giving up — covering
**both** dialling a new connection and queueing for a free one.

This is the knob that decides how your app behaves when the database is
unreachable. Too high and workers pin waiting on a database that will
never answer; too low and a legitimate traffic spike, where queueing is
real and productive work, turns into errors.

The default of 5 seconds is deliberately tighter than the driver's 30.
Raise it if you have long-running queries and a saturated-but-healthy
pool; lower it if you would rather shed load quickly.

### `pool_idle_timeout_secs`

Closes connections that have sat unused. Set this **below** whatever the
shortest idle timeout is on the path between your app and the database —
a load balancer, a proxy, a firewall's connection table, or the database
server's own idle timeouts. If something else closes the connection
first, your pool hands out a dead one.

10 minutes is a common starting point.

### `pool_max_lifetime_secs`

Closes connections after a fixed age, whether or not they are healthy.
This is the knob that makes failovers and credential rotations actually
take effect: without it, a pool can keep talking to the server it
connected to at boot.

30 minutes is a common starting point. Shorter if you lease credentials
from a secrets manager with a short TTL.

## Backends differ

**SQLite is not a server**, and pool sizing does not mean the same thing.
SQLite serialises writers globally — one writer at a time, whatever the
pool size — so raising `pool_max_size` adds read concurrency but never
write concurrency. With an in-memory database, additional connections are
worse than useless: each one is a *separate empty database* unless the
URL sets `cache=shared`.

**PostgreSQL and MySQL** both enforce a server-side connection limit.
Size your pools against that budget across all replicas, and remember
that connection poolers such as PgBouncer change the arithmetic.

## Connection options, which are a different thing

The knobs above are about the *pool*. Options about a single
*connection* — TLS, connection timeouts, the application name the server
sees — travel in the connection URL:

```
postgres://user:pw@host:5432/db?sslmode=require&connect_timeout=10&application_name=myapp
mysql://user:pw@host:3306/db?ssl-mode=REQUIRED
sqlite://./dev.db?mode=rwc
```

These are passed through to the driver, so anything the driver's URL
parser accepts works.

## Tenant pools

In a multi-tenant app, each database-mode tenant gets its own pool, and
those are configured separately through `TenantPoolsConfig` — see
[Tenant-pool tuning](manage.md) in the `manage` guide. Their defaults
differ from the primary pool's; in particular the tenant acquire timeout
is more generous, which is worth reviewing if you run many tenants
against databases that can become unreachable independently.
