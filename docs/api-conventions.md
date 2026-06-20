# API conventions

This page explains the patterns **Rustango**'s API follows, so you can predict how any method behaves before you read its docs. If you're contributing or auditing a feature, these are the rules.

[![Rustango's naming convention: the method suffix tells you what it takes — `*_on` for a typed pool, bare for the multi-backend pool, and pool-free signals](img/api-conventions.png)](img/api-conventions.png)

## Table of contents

- [Naming](#naming)
- [Constructors](#constructors)
- [Return types](#return-types)
- [Async vs sync](#async-vs-sync)
- [The pool argument](#the-pool-argument)
- [Filtering](#filtering)
- [Errors](#errors)
- [Module naming](#module-naming)
- [Builders vs config structs](#builders-vs-config-structs)
- [Feature flags](#feature-flags)
- [Macros vs runtime](#macros-vs-runtime)
- [Contributing](#contributing)

---

## Naming

The name of a method tells you what it does. Once you learn these suffixes, you can guess most of the API.

### Functions

- **`save_on(executor)`, `delete_on(executor)`** — write methods take an *executor* (a pool, connection, or transaction — the thing that talks to the database). The `_on` suffix means "run this against the executor I'm handing you."
- **`fetch_on(executor)`, `count_on(executor)`** — same `_on` suffix, for reads.
- **`save()`, `fetch()`, `count()`** without `_on` — shorthand that calls the `_on` version with a default `&pool`. Only works where the queryset or model already holds a pool reference (rare in app code).
- **`from_X(value)`** — converts FROM another value (e.g. `from_model(post)`, `from_base32(s)`).
- **`with_X(value)`** — a builder method that sets one option and returns the object, so you can chain calls (e.g. `with_default_ttl(d)`, `with_access_ttl(secs)`).
- **`new()`** — the minimal constructor. Any arguments it takes are required dependencies (e.g. `RedisCache::new(url)` — you can't build the cache without a URL).

### Types

These follow standard Rust casing, the same as Python's PEP 8 split between classes and functions:

- **`PascalCase`** — types, traits, and enum variants (like Python classes).
- **`snake_case`** — modules, functions, fields, and local variables.
- **`SCREAMING_SNAKE_CASE`** — constants, plus the `Model::SCHEMA` constant that the derive macro generates for each model.
- **`Boxed*`** — an alias for `Arc<dyn Trait>`, a thread-safe shared pointer to a trait object (the Rust way to hold "any implementation of this interface"). For example `BoxedCache = Arc<dyn Cache>`. This is the standard type for a pluggable backend you can swap out.

### Modules

- **Singular** when the module holds ONE main type or concept: `cache`, `email`, `storage`, `signed_url`, `request_id`.
- **Plural** when the module holds a COLLECTION of items: `bulk_actions`, `api_keys`, `passwords`, `forms`, `signals`.

---

## Constructors

How you build an object depends on what it needs. There are a few standard shapes:

| Pattern | When | Example |
|---|---|---|
| `T::new()` | Minimal — no required dependencies | `InMemoryCache::new()`, `Validator::new()` |
| `T::new(arg)` | One required dependency | `EnvSecrets::with_prefix(s)`, `RedisCache::new(url)` |
| `T::with_X(arg)` | Builder-style override after `new()` | `InMemoryCache::with_default_ttl(d)`, `JwtLifecycle::new(s).with_access_ttl(60)` |
| `T::from_X(arg)` | Convert FROM Y | `TotpSecret::from_base32(s)`, `Locale::new(s)` (sometimes `from_str`) |
| `T::for_Y(arg)` | Build T scoped to a specific Y | `ViewSet::for_model(schema)` |

**Avoid this:** `T::with_X_and_Y_and_Z(a, b, c)` — one constructor that takes everything. Split it into `new(...)` plus chained `.with_*()` calls instead.

---

## Return types

A method's return type tells you how it can fail. Rust has no exceptions, so failure is part of the return value. There are three shapes.

**`Result<T, E>`** — like a function that either returns a value or throws. You get either the value `T` or an error `E` with details. Use it for operations that can fail and where the *why* matters:
- I/O: `pool.fetch(...).await -> Result<_, sqlx::Error>`
- Validation: `Form::parse(data) -> Result<Self, FormErrors>`
- Issuance: `JwtLifecycle::issue_pair_with(uid, claims) -> Result<_, JwtIssueError>`

**`Option<T>`** — either a value (`Some`) or nothing (`None`), like a nullable field. Use it when "nothing found" is a normal outcome and you don't need an error message explaining why:
- Lookups: `cache.get(k) -> Result<Option<String>, _>` (the `Result` covers I/O failure; the `Option` covers "key not present")
- Verification: `JwtLifecycle::verify_access(token) -> Option<Claims>` ("expired or invalid" is an expected outcome, so `None` is enough)
- Optional config reads: `env::optional("FOO") -> Result<Option<T>, _>`

**`bool`** — a plain yes/no when no further detail is needed:
- `cache.exists(k) -> Result<bool, _>` (the `Result` covers I/O; the `bool` is the answer)
- `JwtLifecycle::revoke(token) -> bool` (true = added to the blacklist)
- `disconnect_pre_save(id) -> bool` (true = an entry was removed)

**`Result<Option<T>>` or `Result<T>` with a `NotFound` error?** Both can express "lookup failed," so pick by how exceptional "not found" is:
- Use `Result<Option<T>>` when "not found" is routine — your code almost always branches on `Some`/`None` anyway.
- Use `Result<T>` with a `NotFound` error variant when "not found" is exceptional — something you'd log as a warning or turn into a 404.

---

## Async vs sync

The rule of thumb: if a method waits on something (the database, network, or disk), it's `async` and you must `.await` it. If it just computes, it's a normal sync call. This table spells it out.

| Operation | Sync or async? |
|---|---|
| Trait method that touches I/O (DB, network, file) | **async** |
| Trait method that's pure compute (`hash`, `verify`, `encode`) | **sync** |
| Builder methods (`with_X`, chainable setters) | **sync** |
| Macros (`derive(Model)`, `derive(Serializer)`) | **N/A** (compile-time) |
| Signal `connect_*` (registers a receiver) | **sync** |
| Signal `send_*` (dispatches to async receivers) | **async** |

**Exception:** `Cache::set` is `async` even though the in-memory version (`InMemoryCache::set`) never actually waits. The trait is shaped for the Redis case, which does. This is intentional: a trait method should be `async` if *any* reasonable implementation needs to wait, so all backends share one signature.

---

## The pool argument

Every ORM call takes a pool or executor (the database handle) as its **last** argument. You pass the connection in every time, rather than relying on a hidden global:

```rust
post.save_on(&pool).await?
Post::objects().filter(...).fetch_on(&pool).await?
send_post_save(&post, ctx).await                  // ⚠️ no pool — signals are pool-free
```

**One exception:** signals don't take a pool, because they never touch the database. The rule holds: anything that hits the DB takes the pool; anything that doesn't, doesn't.

**Why pass it every time?** Rust prefers dependencies you can see over hidden global state. Django keeps the connection in thread-local storage, but that breaks down in Rust's async world, where a task can hop between threads mid-request. The downside is more typing; the upside is that you can grep for every place that touches the database.

If you find yourself passing `&pool` through ten layers of function calls, accept `impl Executor` once at the public entry point and let the internal helpers share that single connection.

---

## Filtering

There are three ways to filter a queryset, and they all combine in one query. Pick by where the filter comes from.

```rust
// 1. HTTP query string (set via ViewSet filter_fields, parsed at request time)
//    GET /api/posts?author_id=42&status__ne=archived

// 2. String-keyed (lookup at compile of the queryset; runtime field name resolution)
Post::objects().filter("author_id", Op::Eq, SqlValue::I64(42));

// 3. Typed columns (compile-time field check)
Post::objects().where_(Post::author_id.eq(42));
```

| Syntax | Use when |
|---|---|
| HTTP query | Public API endpoints — the ViewSet parses these for you, like DRF's filter backends |
| String-keyed `.filter` | Generic CRUD or admin code, where field names come from config and aren't known at compile time |
| Typed `.where_` | Your app code — the preferred default. The compiler checks the field exists and the types match |

You can **mix all three** in a single queryset.

---

## Errors

**Rustango** has **20+ error types** — one per module — instead of a single catch-all exception class. They form a loose hierarchy, and a top-level type ties them together so you rarely deal with them individually.

| Layer | Module | Error type |
|---|---|---|
| ORM I/O | `sql::*` | `ExecError` |
| ORM SQL writer | `sql::*` | `SqlError` (variant of `ExecError::Sql`) |
| Migrations | `migrate::*` | `MigrateError` |
| Forms | `forms::*` | `FormError` (single) + `FormErrors` (multi) + `ModelFormError` |
| Cache | `cache::*` | `CacheError` |
| Email | `email::*` | `MailError` |
| Storage | `storage::*` | `StorageError` |
| Auth backends | `tenancy::auth_backends` | `AuthError` |
| JWT | `tenancy::jwt_lifecycle` | `JwtIssueError` |
| API keys | `api_keys::*` | `ApiKeyError` |
| Passwords | `passwords::*` | `PasswordError` |
| Webhooks | `webhook::*` | (returns bool, no dedicated error) |
| Signed URLs | `signed_url::*` | `SignedUrlError` |
| Bulk actions | `bulk_actions::*` | `BulkActionError` |
| Fixtures | `fixtures::*` | `FixtureError` |
| IP filter | `ip_filter::*` | `IpFilterError` |
| i18n | `i18n::*` | `I18nError` |
| Env | `env::*` | `EnvError` |
| Secrets | `secrets::*` | `SecretsError` |
| API responses | `api_errors::*` | `ApiError` (HTTP-shaped, not internal) |

**The one to use in handlers:** there's a top-level `RustangoError` enum (exported from `lib.rs`, along with the alias `RustangoResult<T> = Result<T, RustangoError>`). It wraps every error above with `From` conversions, so the `?` operator promotes any module error into it automatically. It also implements `IntoResponse`, meaning each variant maps to a sensible HTTP status when returned from a handler. The split is simple: use the specific per-module errors deep in your code, and `RustangoError` / `RustangoResult` at the handler boundary. For errors from third-party crates, `RustangoError::other(msg)` / `RustangoError::other_from(e)` wrap any `std::error::Error + Send + Sync + 'static`.

**A handler example:**

```rust
use rustango::api_errors::ApiError;

async fn handler() -> Result<Json<X>, ApiError> {
    let post = Post::objects().get(&pool, 1).await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(post))
}
```

`ApiError` implements `IntoResponse`, so returning it produces the standard JSON error shape automatically.

---

## Module naming

A module's name should let you **guess the type names inside it** without opening the file.

| Module | Hosts | Lookup confidence |
|---|---|---|
| `cache` | `Cache` trait, `*Cache` impls | high |
| `email` | `Mailer` trait, `Email`, `*Mailer` impls | high |
| `storage` | `Storage` trait, `*Storage` impls | high |
| `signed_url` | `sign`, `verify` free fns | medium |
| `text` | `slugify`, `html_escape`, `truncate` free fns | medium |
| `bulk_actions` | `BulkActionRegistry`, `BulkAction`, `Bulk*Action` impls | high |
| `api_keys` | `generate_key`, `verify_key`, `split_token` free fns | medium |

**Avoid this:** a module that holds an unrelated grab-bag (`utils`, `helpers`, `common`). If you can't name the single concept it covers, it shouldn't be a module.

---

## Builders vs config structs

There are two ways to hand over a configured object. Pick based on how users will set it up.

### Builder: chained setters, no `Default`

```rust
let l = SecurityHeadersLayer::strict()
    .csp(...)
    .header("x-extra", "v");
```

Use when:
- Most users start from a preset and tweak
- Setters express intent (e.g. `.errors_only()` reads better than `.log_success(false)`)
- The struct has many optional fields (10+)

### Config struct: set fields directly, fall back to `Default`

```rust
let l = AccessLogLayer {
    log_success: false,
    include_ip: true,
    slow_threshold_ms: 500,
    ..Default::default()
};
```

Use when:
- Users want to be explicit about every field
- Reflection / serialization matters
- Updating in place is common (`config.field = ...`)

**As a rule, **Rustango** uses builders** for HTTP middleware (`security_headers`, `cors`, `rate_limit`, and so on) and config structs for plain data carriers (`Email`, `AccessLogLayer`, `RateLimitLayer`'s internal state).

---

## Feature flags

A *feature* is a Cargo build flag (`Cargo.toml`'s `[features]`) that switches a chunk of the crate on or off — similar to Laravel package discovery or Django's `INSTALLED_APPS`, but resolved at compile time. Every module that pulls in an extra dependency sits behind one. The default set is "you almost certainly want these":

```toml
default = [
    "postgres", "admin", "config", "forms", "serializer",
    "cache", "signals", "email", "storage", "scheduler",
    "secrets", "totp", "webhook", "api_keys", "passwords", "signed_url",
]
```

**Off by default:** features that pull in heavy dependencies or external services:
- `tenancy` — adds `argon2`, `hmac`, `sha2`, `cookie`, `tower` (most apps don't need it)
- `cache-redis` — adds the `redis` crate (most apps are fine with the in-memory cache)
- `csrf` — turned on automatically by `admin`, but available on its own too

To trim a binary that doesn't need everything, opt out of the defaults and list only what you use:

```toml
rustango = { version = "0.43", default-features = false, features = ["postgres", "admin"] }
```

---

## Macros vs runtime

A *macro* is code that generates code at compile time (`#[derive(Model)]` and friends) — roughly what a Rails generator does, except it runs every build and the compiler checks the result. The split below decides what's done by a macro versus plain runtime code.

| Concern | Macro or runtime? |
|---|---|
| Schema metadata for `inventory` | macro (`#[derive(Model)]`) |
| Schema-driven query building | runtime (uses the `&'static ModelSchema` from the macro) |
| Form parsing | macro for the struct (`#[derive(Form)]`); runtime for the parsing logic |
| Serializer field selection | macro (`#[derive(Serializer)]`) — emits a `from_model` + custom `Serialize` |
| Migration ops | runtime (`SchemaSnapshot` diff) |
| Signal dispatch | runtime (`TypeId`-keyed registry, no per-model macro) |
| Auth backend pattern matching | runtime (`#[async_trait]` on `AuthBackend`) |

**Rule:** use a macro for anything the compiler can verify up front (field names must exist, types must match). Use runtime code for anything that varies per request or per deployment.

---

## Contributing

When you add a new feature, follow these steps:

1. **One module per concept**, in `crates/rustango/src/<name>.rs` or `<name>/mod.rs`.
2. **Add module-level rustdoc** with a "Quick start" example in a `// ignore` block.
3. **Add a feature flag if you pull in a new dependency** — name it after the module (`feature = "<name>"`).
4. **Re-export the module from `lib.rs`** with a one-line rustdoc.
5. **Put unit tests in the same file**, behind `#[cfg(test)] mod tests` — no database unless you truly need one.
6. **Put integration tests in `crates/rustango/tests/<name>.rs`** for the end-to-end story.
7. **Don't add a new error type unless the existing ones don't fit** — extend an existing enum first.
8. **Follow the [return-type guide](#return-types)** when choosing `Result`, `Option`, or `bool`.
9. **Adding a `manage` subcommand?** Wire it into the `match cmd` dispatcher and `print_help`, add a test in `crates/rustango/tests/migrate_manage.rs`, and document a row in `docs/manage.md`.
10. **Update `CHANGELOG.md`** with an `Added` entry under the next version.

When you break the API:
- Mark the old item `#[deprecated(since = "...", note = "use X instead")]` and keep it for one full minor version before removing it.
- Record it in `CHANGELOG.md` under `Breaking changes`.
- Link the migration path from the release notes.
