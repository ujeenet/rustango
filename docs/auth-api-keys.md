# API keys

An API key is a **long-lived credential for machines** — CI jobs, scripts,
server-to-server calls — that can't present a login form or carry a session
cookie. The client sends the key on every request; the server looks it up and
identifies the caller. **Rustango** gives you two layers: a standalone
generate/verify helper you can wire to your own table, and a batteries-included
backend that stores keys and authenticates `Authorization: Bearer` requests.

[![API keys in Rustango: generate_key returns a one-time token prefix.secret, you store the 8-char prefix plus an argon2id hash, and verify_key checks an incoming secret](img/auth-api-keys.png)](img/auth-api-keys.png)

> **New to a term here?** *Token*, *hash*, *Bearer*, *argon2id* — the
> [glossary](glossary.md) defines the building blocks.

> **Source:** `rustango::api_keys` (`generate_key`, `hash_secret`, `verify_key`,
> `split_token`, `ApiKeyError`) — the standalone helper, behind the `api_keys`
> feature (on by default). The stored backend is
> `rustango::tenancy::auth_backends` (`create_api_key`, `ApiKeyBackend`,
> `ensure_api_keys_table_pool`, `set_api_key_secret`) — behind the `tenancy`
> feature.
>
> **Runnable version:** the helper snippets are copied from
> [`auth_api_keys_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_api_keys_doc.rs)
> (`cargo test -p rustango --test auth_api_keys_doc`); the `ApiKeyBackend`
> middleware flow from
> [`auth_backends_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/auth_backends_doc.rs)
> (`cargo test -p rustango --features sqlite,tenancy --test auth_backends_doc`).

## Table of contents

- [How an API key works](#how-an-api-key-works)
- [The standalone helper](#the-standalone-helper)
- [The stored backend](#the-stored-backend)
- [Issuing a key (CLI + code)](#issuing-a-key)
- [Authenticating requests](#authenticating-requests)
- [Security notes](#security-notes)
- [See also](#see-also)

---

## How an API key works

A key has two parts joined by a dot: **`{prefix}.{secret}`**.

- The **prefix** is 8 hex characters — stored in clear text and used as a
  fast lookup index ("which key is this?").
- The **secret** is 32 hex characters and is the actual credential. You store
  only a **hash** of it, never the secret itself.

The full token is shown to the user **exactly once**, at creation. Lose it and
you reissue — there's no way to recover it, because only the hash is kept. This
is the same "hash, don't store" discipline as [passwords](auth-passwords.md),
applied to machine credentials.

---

## The standalone helper

`rustango::api_keys` is a dependency-free toolkit (no database, no tables) — use
it when you want to store keys in your own schema.

```rust
use rustango::api_keys::{generate_key, split_token, verify_key};

// At creation: returns (full_token, prefix, hash).
let (token, prefix, hash) = generate_key()?;
// → token  = "a1b2c3d4.<secret>"   show to the user ONCE
// → prefix = "a1b2c3d4"            store as the lookup key
// → hash   = "$argon2id$v=19$..."  store instead of the secret

// On an incoming request: pull the token, find the row by prefix, verify.
let (prefix, secret) = split_token(&token).expect("well-formed token");
let stored_hash = lookup_hash_by_prefix(prefix);     // your query
if verify_key(secret, &stored_hash)? {
    // authenticated
}
```

`split_token` is strict — it returns `None` unless the prefix is exactly 8
characters and the secret is non-empty, so malformed input is rejected before
you touch the database:

```rust
assert!(split_token("no-dot-here").is_none());
assert!(split_token("short.secret").is_none()); // prefix must be 8 chars
assert!(split_token("a1b2c3d4.").is_none());     // empty secret
```

`hash_secret` and `verify_key` use argon2id with a random per-hash salt, so
hashing the same secret twice yields different strings — and both verify.
`verify_key` returns `Ok(false)` on a mismatch and `Err(ApiKeyError)` only when
the stored string isn't a valid hash.

From async code call `generate_key_async`, `hash_secret_async` and
`verify_key_async`; they run argon2 off the runtime. They can also return
`ApiKeyError::Busy` when no hashing slot frees up in time.

---

## The stored backend

If you're already on the `tenancy` layer, you don't need your own table.
`rustango::tenancy::auth_backends` ships an `ApiKey` model (table
`rustango_api_keys`), a creator, and an auth backend that plugs into the
[backend chain](auth-backends.md).

Bootstrap the table once (tri-dialect, idempotent):

```rust
use rustango::tenancy::auth_backends::ensure_api_keys_table_pool;

ensure_api_keys_table_pool(&pool).await?;   // CREATE TABLE IF NOT EXISTS
```

The `ApiKey` row stores `user_id` (FK to `rustango_users`), the 8-char
`key_prefix` (indexed, not unique), the `key_hash`, a `label`, an optional
`expires_at` and `created_at`.

The stored backend does not use argon2id. `key_hash` is an HMAC-SHA256 of the
secret, under a key derived from the server secret. That keeps the check cheap
on every request. The server secret is `RUSTANGO_SESSION_SECRET` (in dev, the
tenant session key file). Call `set_api_key_secret(secret)` once at startup to
use another key; the first call wins. **Changing that key, or rotating
`RUSTANGO_SESSION_SECRET`, makes every issued key stop working.** Rows stored
as argon2id by older versions still verify and are rewritten to the HMAC on
first use.

---

## Issuing a key

`create_api_key` generates the token, hashes the secret, inserts the row, and
returns the **plaintext token once**. On the prod tier it fails when
`RUSTANGO_SESSION_SECRET` is unset:

```rust
use rustango::tenancy::auth_backends::create_api_key;

// Issue a non-expiring key for user 42, labelled "ci-key".
let token = create_api_key(42, "ci-key", None, &pool).await?;
println!("Store this — it won't be shown again: {token}");

// Or with an expiry:
use chrono::{Duration, Utc};
let token = create_api_key(42, "tmp", Some(Utc::now() + Duration::days(30)), &pool).await?;
```

From the command line, the `manage` CLI wraps the same call. It creates the
table if needed, and `--label` defaults to empty:

```bash
cargo run -- create-api-key <tenant> <username> --label "ci-key" --expires-days 30
```

The CLI must use the same server secret as the server, or the key matches
nothing. When it falls back to the dev key file it prints a note saying so.

---

## Authenticating requests

Register `ApiKeyBackend` in your [auth-backend chain](auth-backends.md) and the
middleware authenticates any `Authorization: Bearer {prefix}.{secret}` request:

```rust
use std::sync::Arc;
use rustango::tenancy::auth_backends::{ApiKeyBackend, AuthBackend, ModelBackend};
use rustango::tenancy::RouterAuthExt;

let backends: Vec<Arc<dyn AuthBackend>> = vec![
    Arc::new(ModelBackend),   // HTTP Basic (humans)
    Arc::new(ApiKeyBackend),  // Bearer key  (machines)
];

let app = Router::new()
    .route("/api/data", get(handler))
    .require_auth(backends);
```

A client then calls:

```bash
curl https://api.example.com/api/data \
  -H "Authorization: Bearer a1b2c3d4.the-secret-half"
```

The backend finds the `ApiKey` rows by the 8-char prefix, checks the secret
against each stored hash, then checks `expires_at`, loads the owning user, and
injects it for your handlers to read via [`CurrentUser`](auth-backends.md).

- A wrong secret or unknown prefix is not an error: the next backend in the
  chain runs. If none signs the caller in, `require_auth` answers `401`.
- An expired key is a `401` at once.
- A disabled owner is a `403`.
- Failed attempts are rate-limited per tenant and per client IP. Over the
  limit the answer is `429`.

The key lives in one tenant's database, so it only works on that tenant.

---

## Security notes

- **The secret is shown once.** Only the prefix + the keyed hash are
  persisted — there is no recovery, only reissue.
- **The prefix is stored in clear text on purpose** — it's the O(1) lookup
  index. A database leak reveals which prefixes exist, never the secrets.
- **Timing is equalized.** The hash is compared in constant time, and the
  secret is checked before expiry, so an expired key costs the same as an
  unknown one.
- **Scope keys to a user, set an expiry, and rotate.** Issue per integration so
  you can revoke one without disturbing the others; prefer short `expires_at`
  windows for temporary access.
- **Disambiguation from JWTs:** the backend treats a Bearer value as an API key
  only when its first dot-segment is exactly 8 chars — so API keys and
  [JWTs](auth-jwt.md) can share the `Authorization: Bearer` header.

---

## See also

- [Auth backends](auth-backends.md) — the chain `ApiKeyBackend` plugs into, and
  the `CurrentUser` extractor + `require_auth`/`require_perm` middleware.
- [HMAC request signing](auth-hmac.md) — for machine callers that need
  per-request integrity, not just a bearer credential.
- [Passwords](auth-passwords.md) — the same hash-don't-store discipline for human
  logins.
- [JWT](auth-jwt.md) — short-lived stateless tokens, the other machine option.
