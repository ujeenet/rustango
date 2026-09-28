# The commerce soak

A Docker fleet that runs the two `platform_commerce*` examples across
all three dialects, under sustained load, and asserts one named check
per behaviour change since 0.57.0.

It exists because every fix in the 0.57.x train was verified **in
isolation**, by a test written for that one issue. Nothing had run them
together, under load, against real infrastructure.

## Running it

```bash
export RUSTANGO_SESSION_SECRET="$(openssl rand -base64 32)"
export RUSTANGO_SECRET_KEY="$(openssl rand -base64 32)"        # SSO secrets at rest
export SOAK_SERVICE_TOKEN_SECRET="$(openssl rand -base64 32)"  # #1538 probe

# ~25 min of image builds the first time; the BuildKit cache is shared
# across all four, so later builds are minutes.
docker compose -f docker/soak/docker-compose.yml build

docker compose -f docker/soak/docker-compose.yml up -d
docker compose -f docker/soak/docker-compose.yml --profile driver up driver

# Log scan (#1610), ERROR lines, bootstrap exits, Playwright merge:
python3 docker/soak/finalize.py --out /tmp/soak-report
```

`finalize.py` merges `playwright.json` from the browser agent if it finds
one (`--playwright PATH`, `docker/soak/soak-results/`, or the
`soak-results` volume), and prints one table: every 0.58.0 fix, its
checks, and the verdict per instance. A fix with no check row prints as
`MISSING` and fails the run.

`up -d` gives you a running fleet to poke at. The driver sits behind a
profile so bringing the stack up does not start a 30-minute run.

Knobs: `SOAK_DURATION_SECS` (1800), `SOAK_CONCURRENCY` (24),
`SOAK_TENANTS` (20), `SOAK_FAIL_RATIO_PCT` (2). Every published port
takes an override (`SOAK_PG_PORT`, `SOAK_MY_PORT`, `SOAK_REDIS_PORT`,
`SOAK_MINIO_PORT`, `SOAK_MINIO_CONSOLE_PORT`, `SOAK_IDP_PORT`,
`SOAK_SINGLE_{PG,MY,SQ}_PORT`, `SOAK_SAAS_{PG,MY,SQ}_PORT`), e.g.
`SOAK_MY_PORT=3506` when the root compose holds 3406. `SOAK_INSTANCES=single-sq,saas-sq`
runs the driver against a subset (the edge instances are
`saas-pg-edge` and `single-pg-edge`).

## Reading the log

`INFO` is the default and tells the story — what each instance is,
which tenant queues started, every order queued, every job completed:

```
web-saas-pg   INFO platform_commerce_saas: starting tenants=20 fail_ratio_pct=2
web-saas-pg   INFO supervisor: tenant queue started tenant=t01 workers=2
web-saas-pg   INFO urls: order queued for fulfilment order=8123 tenant=t07
worker-pg     INFO jobs: order confirmed tenant=t07 order=8123
```

`WARN` is for failures the soak **injected on purpose** — they carry
`expected=true`, and they are how the run proves `MAX_ATTEMPTS` is a
total-attempt ceiling (`attempts=4`) and that `JobError::Fatal` bypasses
retry (`attempts=1`).

`ERROR` is reserved for a failure nobody asked for. If the log has one,
that is the finding.

That split is deliberate. At a 10% injection rate and ERROR severity the
fleet produced **6308 ERROR lines in five minutes** — every one an
assertion passing, and a real failure in that stream would have been
invisible. A soak whose output cannot be read is worse at its job than
no soak.

For per-attempt detail — each retry of an injected failure, each
storefront render, each supervisor tick:

```bash
RUST_LOG='info,platform_commerce=debug,platform_commerce_saas=debug' \
  docker compose -f docker/soak/docker-compose.yml up -d
```

`sqlx=warn` stays set: at these volumes sqlx's own DEBUG is a line per
statement and drowns everything else.

The report lands in the `soak-results` volume as `report.json` and is
printed. **Exit code is non-zero only on FAIL.** NOT-COVERED does not
fail the run: an honest gap is not a regression, and the whole point is
that a check which cannot run must never report a pass.

## Four images, not six

`platform_commerce` compiles with all three backend features at once and
picks its dialect at run time from `DATABASE_URL`, so one image covers
the matrix.

`platform_commerce_saas` cannot. `DefaultTenantDb` resolves to
`sqlx::Postgres` whenever the `postgres` feature is on
(`tenancy/pools.rs`), so a tenancy build's *registry* dialect is fixed
at compile time — hence one SaaS image per dialect, each built with
exactly one backend feature. That is a real property of the framework,
not a packaging choice.

## Things that will bite, and why they are set up this way

**Connections.** `TENANT_POOL_*` sizes each tenant pool (6) and caps
the cache at 8, below the 20 tenants on purpose so the run forces pool
eviction (#1527). `max_connections=600` on `pg` and
`--max-connections=1200` on `my` stay well above the draw, so an
exhaustion failure is the app's, not this file's.

**Shutdown grace.** `stop_grace_period: 45s`, because Docker's default
is 10s, `PgJobQueue::shutdown` gives each in-flight job a hard-coded 5s,
and `Cli::on_shutdown` runs *after* the server drains. Ten seconds
reliably SIGKILLs mid-drain and loses exactly the jobs being counted.

**Tenant databases.** rustango does not create them — `provision.rs`
says "database-mode tenants bring their own database", and preflight
creates and drops a table to prove migrations can run, so a missing
database fails provisioning outright. `bootstrap.sh` pre-creates the
MySQL ones with a single wildcard grant. Postgres schema mode and
SQLite's `?mode=rwc` need nothing.

**Resolver convergence.** Sleep, do not poll. Negative results are
cached for 30s, so probing a tenant host too early poisons the cache and
makes a working tenant look broken for a minute. The driver waits and
then probes once, and asserts on the tenant in the *response* rather
than the Host it sent.

**Health checks do not use `/health`.** `.with_health()` mounts it on a
Postgres build and silently does nothing on SQLite or MySQL (#1457).
On the SaaS app every app route lives on the tenant router, so a probe
with no tenant Host lands on the operator console anyway. The check asks
the only question it can answer for both apps on all three dialects: is
the process listening and routing?

**SQLite runs its workers in-process.** Two containers sharing one
SQLite file need POSIX advisory locks plus WAL shared memory across the
container boundary, which is unreliable on Docker Desktop. The files
live on a named volume, never a bind mount.

**Postgres and MySQL get a separate worker container**, which is the
point of having one: `rustango_jobs` rows are claimed with three
different per-dialect strategies, and only two processes competing for
the same rows exercises that. A MySQL clause-order bug in v0.38 meant
workers silently never picked anything up; a single-process queue would
not have found it.

## Ports

Shifted, because this stack is meant to run *alongside* the repo-root
`docker-compose.yml`. Container-internal ports are the standard ones.

| | host | container |
|---|---|---|
| postgres | 5433 | 5432 |
| mysql | 3406 | 3306 |
| redis | 6479 | 6379 |
| minio | 9100 / 9101 | 9000 / 9001 |
| web-single-{pg,my,sq} | 18081–18083 | 8080 |
| web-saas-{pg,my,sq} | 18084–18086 | 8080 |
| idp (fake OIDC + webhook sink) | 19000 | 9000 |

`web-saas-pg-edge` and `web-single-pg-edge` publish nothing: they are
second processes on the Postgres databases with the settings a shared
instance cannot carry (HTTPS redirect, access log off, 1 ms hash wait,
20 s lock, admin global login limit 30). Only the driver talks to them.

Browser logins through SSO: `/authorize` is served on
`http://localhost:19000`, shows a form for the identity to assert, and
sends the browser back over `http` (the apps build an `https` callback
from `Host`). Tenant `t03` has the providers `idp-strict` and `idp-link`
after the driver's first run (`POST /_soak/sso/seed`).

MySQL publishes 3406 to match the root compose, which picked it to dodge
a local MySQL. CI and the scaffolder use 3306 because they run in
isolated environments. That discrepancy has confused people; the
container port is 3306 everywhere and is the only one that matters
inside the network.

## What the driver cannot check

Stated because a harness that silently scores these as passes would be
the exact failure this release was about.

- **#1450 is Postgres-only.** The MySQL and SQLite binders were
  deliberately left unchanged, so those two arms are controls. The
  report says so per instance rather than printing three passes.
- **#1412** is a dropped column plus a `tracing::warn!`. HTTP cannot see
  it; it needs a subscriber capturing the event.
- **#1410** (backoff timing, `MAX_ATTEMPTS` as a total ceiling) is
  in-process. The deterministic flaky job makes the dead-letter count
  exactly predictable, which is the closest an HTTP harness gets.
- **#1422** has no runtime behaviour at all — the only assertion
  available is `--locked` resolving, which `lockfiles` already does.
- **#1440** is best proven by pointing a suite at a dead port, which is
  a test-suite property rather than an application one.
- **#1528** (schema mode) was unbounded connections, which HTTP cannot
  count; on `saas-pg` the pool-cap check only proves no tenant is refused.
- **Browser `Origin`**: the driver sets `Origin` itself, so it checks the
  preset's `Referrer-Policy` on an app route instead (`no-referrer` makes browsers
  send `Origin: null`).
- **#1661** (`#[non_exhaustive]` structs and enums) is compile-time only;
  its `compile_fail` doctests and clippy lints are the check.
- **#1663** `intcomma(i64::MIN)` is an in-process helper no page renders.
- **#1702** is scaffolder output. The soak checks the settings tier
  reaches the tenancy server (HSTS, `allowed_hosts`), not the template.
- **#1626** is a loader-time refusal and the soak's migrations carry no
  callbacks; `finalize.py` only checks every bootstrap `migrate` exited 0.
- **#1672 fixed lockout window**: the failure-counter window is 1 h and
  not settable from `[auth]`. TOTP single use is a browser flow, left to
  the Playwright agent.
- **Session extractors on SQLite/MySQL**: the bug needs a hand-built
  `server::Builder<Sqlite|MySql>` with the `postgres` feature on. Every
  soak image goes through `Cli`, so the `-my`/`-sq` `SessionUser` checks
  are controls.
- **#1645** needs a schema-mode tenant *without* `rustango_users`; every
  soak tenant has one, so the catalog check (no FK leaves its schema) is
  a control.
- **#1673 `ip_filter` on a dual-stack listener** only bites on
  `web-single-sq`, which binds `[::]:8080`; the other instances are
  IPv4-only controls.
- **Tenancy login forms share one IP bucket.** The tenancy server has no
  `RealIpLayer` hook, so the tenant and console login forms key every
  driver request on the driver's own address. The login-limit checks run
  last and the load phase outlasts the 60 s window; JWT and HTTP Basic
  sit behind the API router's `RealIpLayer` and get one address each.

## Known gaps

A `KNOWN-GAP` verdict is a behaviour that was exercised, is **wrong**,
has an issue, and is being shipped anyway on purpose. It is printed in
its own block on every run and never counted as a pass — the point of a
fourth verdict is that "broken and known" and "working" must not look
alike in a report.

None at present.

**#1464 was the last one**, and the shape of its removal is the point.
SQLite's `auto_now_add` columns were written by `DEFAULT
CURRENT_TIMESTAMP` as `2026-09-15 02:53:25` while sqlx bound RFC3339,
and `' '` (0x20) sorts before `'T'` (0x54) — so `WHERE placed_at <
$cursor` was true for every row and cursor pagination served page one
forever. This soak is what found it.

The fix landed across seven PRs, and the `KNOWN-GAP` branch in
`driver.py` came out with them. That order matters: an excuse left
behind after its bug is fixed makes the leg unable to fail, which makes
it unable to prove anything, and a regression would read as "known gap"
indefinitely. A gap entry earns its keep only while the bug is real.
