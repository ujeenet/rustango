# API conventions

How to read rustango's surface — patterns to expect, and where exceptions live. If you're contributing or auditing a feature, these are the rules.

## Table of contents

- [Naming](#naming)
- [Constructor patterns](#constructor-patterns)
- [Result vs Option vs bool returns](#result-vs-option-vs-bool-returns)
- [Async vs sync](#async-vs-sync)
- [The pool argument](#the-pool-argument)
- [Three filter syntaxes — when to use which](#three-filter-syntaxes--when-to-use-which)
- [Error type hierarchy](#error-type-hierarchy)
- [Module naming](#module-naming)
- [Builder vs config struct](#builder-vs-config-struct)
- [Feature gates](#feature-gates)
- [Macros vs runtime code](#macros-vs-runtime-code)
- [Conventions for contributors](#conventions-for-contributors)

---

## Naming

### Functions

- **`save_on(executor)`, `delete_on(executor)`** — write methods take an executor (pool / connection / transaction). The `_on` suffix means "execute against this executor."
- **`fetch_on(executor)`, `count_on(executor)`** — same suffix for reads.
- **`save()`, `fetch()`, `count()`** without `_on` — sugar that calls the `_on` version with `&pool`. Available where the queryset/model has a default pool reference (rare in user code).
- **`from_X(value)`** — converts FROM another value (e.g. `from_model(post)`, `from_base32(s)`).
- **`with_X(value)`** — builder method that adds/replaces one config option (e.g. `with_default_ttl(d)`, `with_access_ttl(secs)`).
- **`new()`** — minimal constructor. Other args are required dependencies (e.g. `RedisCache::new(url)` — URL is required to even construct).

### Types

- **`PascalCase`** — types, traits, enum variants.
- **`snake_case`** — modules, functions, fields, local vars.
- **`SCREAMING_SNAKE_CASE`** — constants and the macro-emitted associated const `Model::SCHEMA`.
- **`Boxed*`** alias for `Arc<dyn Trait>` — e.g. `BoxedCache = Arc<dyn Cache>`. Standard sharing type for pluggable backends.

### Modules

- **Singular** when the module hosts ONE main type or concept: `cache`, `email`, `storage`, `signed_url`, `request_id`.
- **Plural** when the module hosts a COLLECTION of items: `bulk_actions`, `api_keys`, `passwords`, `forms`, `signals`.

---

## Constructor patterns

The 3-constructor convention:

| Pattern | When | Example |
|---|---|---|
| `T::new()` | Minimal — no required dependencies | `InMemoryCache::new()`, `Validator::new()` |
| `T::new(arg)` | One required dependency | `EnvSecrets::with_prefix(s)`, `RedisCache::new(url)` |
| `T::with_X(arg)` | Builder-style override after `new()` | `InMemoryCache::with_default_ttl(d)`, `JwtLifecycle::new(s).with_access_ttl(60)` |
| `T::from_X(arg)` | Convert FROM Y | `TotpSecret::from_base32(s)`, `Locale::new(s)` (sometimes `from_str`) |
| `T::for_Y(arg)` | Build T scoped to a specific Y | `ViewSet::for_model(schema)` |

**Anti-pattern to avoid:** `T::with_X_and_Y_and_Z(a, b, c)` — split into `new(...)` + chained `.with_*()` calls instead.

---

## Result vs Option vs bool returns

**`Result<T, E>`** for operations that can fail with detail:
- I/O: `pool.fetch(...).await -> Result<_, sqlx::Error>`
- Validation: `Form::parse(data) -> Result<Self, FormErrors>`
- Issuance: `JwtLifecycle::issue_pair_with(uid, claims) -> Result<_, JwtIssueError>`

**`Option<T>`** for operations where "not found" is normal and detail isn't useful:
- Lookups: `cache.get(k) -> Result<Option<String>, _>` (Result for I/O, Option for "key absent")
- Verification: `JwtLifecycle::verify_access(token) -> Option<Claims>` ("expired or invalid" is one of the expected outcomes)
- Optional config reads: `env::optional("FOO") -> Result<Option<T>, _>`

**`bool`** for cheap yes/no with no further detail needed:
- `cache.exists(k) -> Result<bool, _>` (Result for I/O, bool for the answer)
- `JwtLifecycle::revoke(token) -> bool` (true = added to blacklist)
- `disconnect_pre_save(id) -> bool` (true = an entry was removed)

**Picking between Result<Option<T>> and Result<T, NotFound + Other>:**
- Use `Result<Option<T>>` when "not found" is non-exceptional (the caller code path branches on Some/None almost always).
- Use `Result<T>` with a `NotFound` error variant when "not found" is exceptional (logged as warning, surfaced as 404, etc.).

---

## Async vs sync

| Operation | Sync or async? |
|---|---|
| Trait method that touches I/O (DB, network, file) | **async** |
| Trait method that's pure compute (`hash`, `verify`, `encode`) | **sync** |
| Builder methods (`with_X`, chainable setters) | **sync** |
| Macros (`derive(Model)`, `derive(Serializer)`) | **N/A** (compile-time) |
| Signal `connect_*` (registers a receiver) | **sync** |
| Signal `send_*` (dispatches to async receivers) | **async** |

**Exception:** `Cache::set` is async even though `InMemoryCache::set` doesn't await anything — the trait is shaped for the Redis case. This is the Right Choice; trait methods should be async if any reasonable impl needs to be.

---

## The pool argument

Every ORM call takes a pool/executor as its **last** argument:

```rust
post.save_on(&pool).await?
Post::objects().filter(...).fetch_on(&pool).await?
send_post_save(&post, ctx).await                  // ⚠️ no pool — signals are pool-free
```

**Inconsistency:** signals don't take a pool because they don't need one. Operations that touch the DB take it; operations that don't, don't.

**Why explicit?** Rust idiom favors visible dependencies over hidden context. Django uses thread-local connection state; Rust async makes that fragile (tasks move across threads). The cost is verbosity; the benefit is grep-ability.

If you find yourself threading `&pool` through 10 function call sites, refactor to take `impl Executor` once at the public boundary and call internal helpers from within a single critical section.

---

## Three filter syntaxes — when to use which

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
| HTTP query | Public API surface (auto-handled by ViewSet) |
| String-keyed `.filter` | Generic CRUD / admin code where field names come from a config |
| Typed `.where_` | App code — preferred default; gets you compile-time field check |

The three styles **mix freely** in one queryset.

---

## Error type hierarchy

rustango has **20+ error types** because each module defines its own. They form a loose hierarchy:

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

**Coming:** a top-level `RustangoError` enum (in the [framework comparison roadmap](../memory/framework-comparison-2026-05-02.md), Tier 3 SEM2) that wraps all of the above with `From` impls. Until it ships, app code uses `Box<dyn std::error::Error>` for handler returns and granular per-module errors at the lower levels.

**For app handlers right now:**

```rust
use rustango::api_errors::ApiError;

async fn handler() -> Result<Json<X>, ApiError> {
    let post = Post::objects().get(&pool, 1).await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(post))
}
```

`ApiError` implements `IntoResponse` and gives you the standardized JSON shape.

---

## Module naming

A module name should let you **predict the type names inside** without opening the file.

| Module | Hosts | Lookup confidence |
|---|---|---|
| `cache` | `Cache` trait, `*Cache` impls | high |
| `email` | `Mailer` trait, `Email`, `*Mailer` impls | high |
| `storage` | `Storage` trait, `*Storage` impls | high |
| `signed_url` | `sign`, `verify` free fns | medium |
| `text` | `slugify`, `html_escape`, `truncate` free fns | medium |
| `bulk_actions` | `BulkActionRegistry`, `BulkAction`, `Bulk*Action` impls | high |
| `api_keys` | `generate_key`, `verify_key`, `split_token` free fns | medium |

**Anti-pattern:** module hosting an unrelated mix (`utils`, `helpers`, `common`). If you can't name the concept, it shouldn't be a module.

---

## Builder vs config struct

Two patterns for "give me a configurable instance":

### Builder (chained setters, no `Default`)

```rust
let l = SecurityHeadersLayer::strict()
    .csp(...)
    .header("x-extra", "v");
```

Use when:
- Most users start from a preset and tweak
- Setters express intent (e.g. `.errors_only()` reads better than `.log_success(false)`)
- The struct has many optional fields (10+)

### Config struct + `Default`

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

**rustango leans builder** for HTTP middleware (security_headers, cors, rate_limit, etc.) and config struct for data carriers (Email, AccessLogLayer, RateLimitLayer's internal state).

---

## Feature gates

Every module that adds a workspace dep is behind a cargo feature. Default-enabled features = "you almost certainly want these":

```toml
default = [
    "postgres", "admin", "config", "forms", "serializer",
    "cache", "signals", "email", "storage", "scheduler",
    "secrets", "totp", "webhook", "api_keys", "passwords", "signed_url",
]
```

**Not default:** features that pull heavy or external-service deps:
- `tenancy` — adds argon2, hmac, sha2, cookie, tower (most apps don't need it)
- `cache-redis` — adds the `redis` crate (most apps OK with in-memory)
- `csrf` — implied by `admin`, but available standalone

To trim a binary that doesn't need everything:

```toml
rustango = { version = "0.20", default-features = false, features = ["postgres", "admin"] }
```

---

## Macros vs runtime code

| Concern | Macro or runtime? |
|---|---|
| Schema metadata for `inventory` | macro (`#[derive(Model)]`) |
| Schema-driven query building | runtime (uses the `&'static ModelSchema` from the macro) |
| Form parsing | macro for the struct (`#[derive(Form)]`); runtime for the parsing logic |
| Serializer field selection | macro (`#[derive(Serializer)]`) — emits a `from_model` + custom `Serialize` |
| Migration ops | runtime (`SchemaSnapshot` diff) |
| Signal dispatch | runtime (`TypeId`-keyed registry, no per-model macro) |
| Auth backend pattern matching | runtime (`#[async_trait]` on `AuthBackend`) |

**Rule:** macros for things rustc can verify at compile time (field names must exist; types must match). Runtime for everything that varies per-request or per-deployment.

---

## Conventions for contributors

When adding a new feature:

1. **One module per concept** in `crates/rustango/src/<name>.rs` or `<name>/mod.rs`.
2. **Module-level rustdoc** with a "Quick start" `// ignore` example block.
3. **Feature gate** if you add a dep — name matches the module (`feature = "<name>"`).
4. **Re-export the module from `lib.rs`** with a one-line rustdoc.
5. **Tests in the same file** behind `#[cfg(test)] mod tests` — unit-style, no DB unless necessary.
6. **Integration tests in `crates/rustango/tests/<name>.rs`** for the end-to-end story.
7. **No new error type unless the existing ones don't fit** — prefer extending an existing enum to creating a new one.
8. **Follow the [Result/Option/bool decision matrix](#result-vs-option-vs-bool-returns)**.
9. **New `manage` subcommand?** Add to `match cmd` dispatcher + `print_help` + a test in `crates/rustango/tests/migrate_manage.rs` + a row in `docs/manage.md`.
10. **Update `CHANGELOG.md`** with an `Added` entry under the next version.

When breaking the API:
- `#[deprecated(since = "...", note = "use X instead")]` for one full minor version before removal.
- Document in `CHANGELOG.md` under `Breaking changes`.
- Release notes link to the migration path.
