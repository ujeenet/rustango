# Logging

Logs are how a running app tells you what it did. **Rustango** builds on
[`tracing`](https://docs.rs/tracing) — the same shape as Django's `LOGGING`
setting or Laravel's channels, but structured: an event carries named fields
(`status=500`, `tenant=acme`) rather than a formatted sentence, so a log
aggregator can filter on them.

A scaffolded project already logs. This page is about the part you tune: what
level, which subsystems, what format, where it goes, and how to tell one
tenant's traffic from another's.

> **New to **Rustango**?** The [glossary](glossary.md) covers the framework's
> building blocks. The logging vocabulary — *level*, *target*, *span* — is
> defined on this page as it comes up.

> **Source:** `rustango::logging` (`setup`, `setup_for_env`, `Setup`,
> `Rotation`, `DEFAULT_FILTER`) — the module is ungated, but every installer
> needs the `runtime` feature. `Setup::from_settings` also needs `config`.
> `rustango::access_log` and `rustango::tenant_log` need `admin` **or**
> `tenancy`; `rustango::tracing_layer` needs `admin`.
>
> **Runnable version:** the settings and defaults here are pinned by
> [`logging_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/logging_doc.rs)
> (`cargo test -p rustango --test logging_doc`), the file sink by
> [`logging_file_appender_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/logging_file_appender_live.rs),
> and the access log's tenant field by
> [`access_log_tenant_sqlite_live.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/access_log_tenant_sqlite_live.rs).

## Table of contents

- [What you already have](#what-you-already-have)
- [Levels, and the filter that picks them](#levels-and-the-filter-that-picks-them)
- [Targets — naming the subsystem](#targets--naming-the-subsystem)
- [Choosing a format](#choosing-a-format)
- [Configuring logging from settings](#configuring-logging-from-settings)
- [Writing to a file](#writing-to-a-file)
- [The access log](#the-access-log)
- [Which tenant was that?](#which-tenant-was-that)
- [Request spans and OpenTelemetry](#request-spans-and-opentelemetry)
- [Logging in tests](#logging-in-tests)
- [Nothing is coming out](#nothing-is-coming-out)

---

## What you already have

`#[rustango::main]` installs a subscriber before your code runs. A scaffolded
project gets it for free, which is why `cargo run` prints logs without any
setup call:

```rust,ignore
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A `tracing_subscriber::fmt` subscriber is already installed here.
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

It uses `RUST_LOG` when set, and `info,sqlx=warn` when not — the value of
`rustango::logging::DEFAULT_FILTER`. That default is deliberate: sqlx logs
every statement at `info`, so an unfiltered `info` buries your own events under
SQL.

To configure anything beyond the level, install a subscriber yourself. Every
installer is idempotent (`try_init` underneath), so an extra call is a no-op
rather than a panic:

```rust,ignore
fn main() {
    rustango::logging::setup();   // full, env-filter, "info,sqlx=warn"
    // ...
}
```

## Levels, and the filter that picks them

Five levels, quietest last: `error`, `warn`, `info`, `debug`, `trace`. A filter
names the maximum verbosity you want, globally or per module:

```sh
RUST_LOG=info                                  # everything at info and above
RUST_LOG=debug,sqlx=warn,hyper=warn            # debug for you, quiet deps
RUST_LOG=warn,rustango::tenancy=debug          # one subsystem, loudly
RUST_LOG=rustango=info                         # only the framework
```

Set the fallback for when `RUST_LOG` is absent — which is most production
deployments, where the filter belongs in config rather than the environment:

```rust,ignore
rustango::logging::Setup::new()
    .with_default_env_filter("info,sqlx=warn,hyper=warn")
    .install();
```

`RUST_LOG` always wins over that fallback. There is no way to configure a
filter that the environment cannot override, by design — an operator
debugging a live incident should not have to ship a build.

**Which level to expect where.** Framework `warn` events tend to share a shape
worth alerting on: *rustango did something other than what you asked*. An
unknown `format` in your settings, an index clause dropped because the backend
can't express it, an admin route that collided and was skipped, an `update`
call with an empty field list — each of those continues, and the `warn` is the
only sign it did.

## Targets — naming the subsystem

Every event carries a **target**, which is what `RUST_LOG=<target>=<level>`
matches on. Framework events live under the `rustango::` root, so
`RUST_LOG=rustango=warn` reaches all of them:

| Target | What it covers |
|---|---|
| `rustango::admin` | Admin routing and registration |
| `rustango::admin::audit` | Audit-log writes |
| `rustango::admin::sso` | Admin SSO |
| `rustango::cache` | Cache backends |
| `rustango::cache_page` | Page-cache middleware |
| `rustango::cors` | CORS policy decisions |
| `rustango::email` | Mail dispatch |
| `rustango::email::smtp` | SMTP transport |
| `rustango::error` | The cause behind a 5xx, which the response body withholds |
| `rustango::humanize` | Humanize filters |
| `rustango::jobs` | Background job queues |
| `rustango::logging` | This subsystem's own warnings |
| `rustango::manage` | `manage` verbs |
| `rustango::media::auth` | Media-router authorization refusals |
| `rustango::messages` | Flash messages |
| `rustango::migrate` | Migration runner |
| `rustango::rate_limit` | Rate limiting |
| `rustango::request_timeout` | Per-request timeout |
| `rustango::scheduler` | Cron / scheduled tasks |
| `rustango::server` | Server boot and shutdown |
| `rustango::shutdown` | Signal handling and shutdown hooks |
| `rustango::sql` | Query execution |
| `rustango::sql::lock` | Row-lock clauses |
| `rustango::template_views` | Template-backed views |
| `rustango::tenancy` | Tenancy, general |
| `rustango::tenancy::admin` | Tenant admin |
| `rustango::tenancy::migrate_run` | Per-tenant migrations |
| `rustango::tenancy::operator_console` | Operator console |
| `rustango::tenancy::pools` | Tenant pool lifecycle |
| `rustango::tenancy::provision` | Tenant provisioning |
| `rustango::tenancy::provision_webhook` | Provisioning webhooks |
| `rustango::tenancy::resolver` | Tenant resolution |
| `rustango::tenancy::sso` | Tenant SSO |
| `rustango::tenancy::sweep` | Retention sweeps |

Those are the targets the framework names explicitly. Events that don't name
one inherit their module path, which gives the same shape —
`rustango::access_log` and `rustango::tracing_layer` are reachable exactly like
the rows above.

> **A target is a string, not a path.** `target: "crate::cache"` compiles
> happily and then sits in a namespace no filter matches. Forty-eight call
> sites had drifted that way before
> [`tracing_targets.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/tracing_targets.rs)
> started failing the build over it. If you add targets in your own code, use
> your crate's real name.

## Choosing a format

| Format | Use for | How |
|---|---|---|
| `full` | The default — one line per event, with span context | default |
| `pretty` | Development — multi-line, one field per line, source location | `.with_format(Format::Pretty)` |
| `compact` | Development — terser single line, span fields at the end | `.with_format(Format::Compact)` |
| `json` | Production — one object per event, for Loki / CloudWatch / Datadog | `.json()` |

All four render differently, which has not always been true: before
#1480 `install()` called neither `.pretty()` nor `.compact()`, so both
values produced `full` output.

### Colour

Colour is on when stdout is a terminal, and off when it is not — piping
to a file or running under CI needs no configuration. `NO_COLOR` is
honoured. `json` and the file sink are never coloured.

| `color` | Behaviour |
|---|---|
| `auto` | Colour iff stdout is a terminal and `NO_COLOR` is unset. The default |
| `always` | Colour even when piped |
| `never` | Never colour |

```rust,ignore
rustango::logging::Setup::new()
    .json()
    .with_default_env_filter("info")
    .install();
```

Or let the tier decide. `setup_for_env()` reads `RUSTANGO_ENV` and picks JSON
when it is `prod` or `production`, `full` otherwise:

```rust,ignore
rustango::logging::setup_for_env();
```

Two display knobs are worth knowing: `.with_line_numbers()` adds source
locations (useful in dev, noisy in prod), and `.without_targets()` hides the
target column — do that only if you have given up on filtering by it.

> Each format value reaches its own formatter. `compact` used to be
> accepted and then render as the default — that is fixed (#1480).

## Configuring logging from settings

Everything above has a TOML equivalent, so a deployment can change its logging
without a rebuild. The section is `[logging]`:

```toml
# config/dev_settings.toml
[logging]
level             = "info,sqlx=warn"
format            = "pretty"
color             = "auto"
with_line_numbers = true
```

```toml
# config/prod_settings.toml
[logging]
level         = "info"
format        = "json"
file_dir      = "/var/log/myapp"
file_prefix   = "app"
file_rotation = "daily"
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `level` | string | `info,sqlx=warn` | `RUST_LOG` syntax. Used only when `RUST_LOG` is unset |
| `format` | string | `full` | `full` / `pretty` / `compact` / `json`. Unknown values fall back to `full` with a `warn` |
| `color` | string | `auto` | `auto` / `always` / `never`. `auto` colours only a terminal, and honours `NO_COLOR` |
| `access_log` | bool | `true` | One line per request, plus the span that carries `tenant` into handler events |
| `with_thread_ids` | bool | `false` | Thread id on every event |
| `with_line_numbers` | bool | `false` | Source line on every event |
| `without_targets` | bool | `false` | Hide the target column |
| `file_dir` | string | unset | Set it to turn the file sink on |
| `file_prefix` | string | `app` | Filename stem |
| `file_rotation` | string | `daily` | `daily` / `hourly` / `minutely` / `never`. Unknown values fall back to `daily` with a `warn` |
| `file_only` | bool | `false` | Drop stdout. No-op unless `file_dir` is set |

Apply it with one call on the `Cli`:

```rust,ignore
rustango::manage::Cli::new()
    .with_settings_from_env()
    .with_logging()               // installs from Settings.logging
    .api(urls::api())
    .run()
    .await
```

`with_logging()` is opt-in — default off, so a project that calls
`logging::setup()` itself doesn't get a second installer. Order in the chain
doesn't matter: the install happens at `run()`, against the final settings.

> **Tell `#[rustango::main]` to step aside.** The macro installs a subscriber
> before the runtime is even built, and every installer uses `try_init`, so the
> first one wins. Pass `logging = false` and yours installs first:
>
> ```rust,ignore
> #[rustango::main(logging = false)]
> async fn main() -> Result<(), Box<dyn std::error::Error>> {
>     rustango::manage::Cli::new()
>         .with_settings_from_env()
>         .with_logging()
>         .run().await
> }
> ```
>
> Without it the `[logging]` section is read and discarded. That used to be
> silent; `install()` now warns on stderr and through `tracing` when it finds a
> subscriber already in place ([#1465]).

[#1465]: https://github.com/ujeenet/rustango/issues/1465

Any key can be overridden per-deployment with an environment variable, using
the section and key as path segments:

```sh
RUSTANGO__LOGGING__LEVEL=debug
RUSTANGO__LOGGING__FORMAT=json
```

To build the subscriber yourself from the same section — when you need the
returned guard, see below — use `Setup::from_settings`:

```rust,ignore
let settings = rustango::config::Settings::load_from_env()?;
let _guard = rustango::logging::Setup::from_settings(&settings.logging).install();
```

## Writing to a file

Logs go to stdout unless you ask otherwise, which is right for a container.
When you need files, `with_file` tees to a rolling appender:

```rust,ignore
use rustango::logging::{Rotation, Setup};

let _guard = Setup::new()
    .json()
    .with_file("/var/log/myapp", "app", Rotation::Daily)
    .install();
```

Files land at `{dir}/{prefix}.YYYY-MM-DD` for `Daily`, and the directory is
created on first write. `Rotation` is `Daily`, `Hourly`, `Minutely` or `Never`.
Add `.file_only()` to drop the stdout layer — for a headless worker or a
daemonized process, where nothing reads stdout anyway.

> **Keep the guard alive.** `install()` returns
> `Option<tracing_appender::non_blocking::WorkerGuard>` — `Some` when a file
> sink is configured. The file writer is non-blocking, so a stalled disk can't
> pause request handling; the cost is that buffered events are flushed when the
> guard drops. Bind it for the life of the process (a `static`, a `OnceLock`,
> or a `let` in `main` that outlives everything). `let _ = ...install();`
> drops it immediately and you lose writes. `Cli::with_logging()` holds it for
> you.

## The access log

One event per completed request, with the fields an operator greps:

```rust,ignore
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};

let app = router.access_log(AccessLogLayer::default());
```

```text
INFO rustango::access_log: http.request.method=GET url.path=/api/posts url.query=page=2 http.response.status_code=200 duration_ms=12 client.address=192.0.2.1 tenant=acme
```

The level carries meaning, so alerting can key on it:

| Condition | Level |
|---|---|
| Normal response | `info` |
| Status >= 400 | `warn` |
| Slower than `slow_threshold_ms` (default 1000) | `warn`, message `slow request` |

Tuning:

```rust,ignore
AccessLogLayer::default()
    .errors_only()               // skip 2xx/3xx entirely
    .slow_threshold_ms(250)      // what counts as slow
    .without_ip()                // omit the client IP
    .trust_proxy_headers(true)   // X-Forwarded-For, behind a trusted proxy only
```

Credential-bearing query params are masked with `[redacted]` before the line is
written — `password`, `passwd`, `token`, `secret`, `api_key`, `apikey`,
`access_token`, `refresh_token`, `signature`, `auth`. Extend with
`.redact_additional("session_id")`, or replace the whole list with
`.redact(vec![...])`. This covers **query strings only**; see
[security.md](security.md#keeping-secrets-out-of-your-logs) for the rest.

## Which tenant was that?

`tenant` names the tenant the request resolved to, and is `-` when none did —
an apex-domain or operator-console request, or a single-tenant app. It is never
blank, so "no tenant" reads differently from a field that went missing.

Nothing to wire: `ChainResolver` publishes the identity when it resolves one,
and the access log reads it back. The mechanism is
[`rustango::tenant_log`](https://docs.rs/rustango/latest/rustango/tenant_log/),
a per-request slot that exists because tenant identity lives in an extractor —
inside the handler, below the middleware that needs to log it.

The slug is operator-chosen and is often the customer's name. When logs leave
your infrastructure, label by id instead:

```rust,ignore
use rustango::access_log::{AccessLogLayer, TenantField};

AccessLogLayer::default().tenant_field(TenantField::Id)     // tenant=42
AccessLogLayer::default().tenant_field(TenantField::Both)   // tenant=acme#42
AccessLogLayer::default().tenant_field(TenantField::Off)    // tenant=-
```

> **The request path only.** A background job runs outside the request that
> enqueued it and has no tenant to log — the slot is per-task and
> `tokio::spawn` does not inherit it. Tracked in
> [#1229](https://github.com/ujeenet/rustango/issues/1229) /
> [#1223](https://github.com/ujeenet/rustango/issues/1223).

## Request spans and OpenTelemetry

`TracingLayer` wraps each request in a span using the
[OpenTelemetry v1.30 semantic conventions](https://opentelemetry.io/docs/specs/semconv/http/http-spans/),
so a collector needs no attribute-renaming rules:

```rust,ignore
use rustango::tracing_layer::TracingLayer;
use tower::ServiceBuilder;

let app = ServiceBuilder::new().layer(TracingLayer::new()).service(router);
```

The span carries `http.request.method`, `url.path`, `url.query`,
`network.protocol.version`, `user_agent.original`,
`http.response.status_code`, `http.response.body.size`, `duration_ms`, and
`tenant` / `org_id` once a tenant resolves. When the request arrives with a W3C
`traceparent` header, `trace_id`, `parent_span_id` and `trace_flags` are
recorded too, which is what a `tracing-opentelemetry` layer picks up to join
the trace.

This layer earns its keep through the tenant field: because the fields sit on
the *span*, every event emitted during the request — including the ORM's —
carries them in span context, with no subsystem knowing what a tenant is.

**`Cli` installs it for you**, on every serving path, together with the access
log. Before #1480 nothing in the framework mounted it, so a `tracing::info!` in
a handler had no enclosing span — no tenant, no correlation — which is what
made handler logs read as loose, context-free lines.

`[logging] access_log = false` turns off **the access log only**. The request
span and the `X-Request-Id` header stay mounted. That setting names the log,
and the service it exists for — one logging requests at the edge — is precisely
the one that still wants trace context and request correlation. (An earlier
version of this page said it turned both off, and an earlier version of the
code did; both were wrong for the same reason.)

`server::Builder` mounts them too when `Cli` hands it the configuration, which
it does on the tenancy serving paths — the tenant admin and the operator
console are behind a Host dispatch the api router never sees, so the layers go
on the outermost router rather than on the one you pass in. Building a
`server::Builder` entirely by hand mounts nothing until you call
`.observability(..)`.

## Logging in tests

Installers use `try_init`, so calling `logging::setup()` from a test is safe
even when another test already installed one. For asserting on output, prefer a
scoped subscriber over a global one:

```rust,ignore
let subscriber = tracing_subscriber::fmt()
    .with_writer(make_writer)
    .with_max_level(tracing::Level::INFO)
    .finish();
let _guard = tracing::subscriber::set_default(subscriber);
```

`set_default` is thread-local and returns a guard that restores the previous
subscriber, so parallel tests don't fight. `#[tokio::test]` runs a
current-thread runtime, which keeps the whole future on the thread the guard
covers.

## Nothing is coming out

- **`RUST_LOG` set, still silent.** The filter is read once, at install. Setting
  the variable after `setup()` has run changes nothing.
- **Your own crate is quiet at `info`.** `RUST_LOG=info` applies to every
  target; if you set `RUST_LOG=rustango=info` you filtered your own code out.
  Name both: `RUST_LOG=info,rustango=warn`.
- **A filter on `crate::something` matches nothing.** Targets are strings; see
  the note under [Targets](#targets--naming-the-subsystem).
- **File is empty after a crash.** The guard from `install()` was dropped, or
  the process died before the appender flushed. See
  [Writing to a file](#writing-to-a-file).
- **Two subscribers, second one ignored.** `try_init` means first install wins.
  If you call `logging::setup()` *and* `Cli::with_logging()`, the
  settings-driven one loses. `install()` warns when this happens — look for
  `[logging] settings ignored` on stderr. Under `#[rustango::main]`, add
  `logging = false`.
