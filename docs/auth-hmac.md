# HMAC request signing

HMAC signing proves **both who sent a request and that it wasn't altered in
flight**. The client signs each request with a shared secret; the server
recomputes the signature and compares. Unlike a bearer [API key](auth-api-keys.md)
— which is replayable if captured — an HMAC signature covers the method, path,
query, timestamp, and body, so a tampered or stale request is rejected. It's the
scheme AWS SigV4 and webhook signatures use, and **Rustango** ships it as one
tower layer.

[![HMAC signing in Rustango: the client signs method+path+query+date+body-hash with a shared secret; HmacAuthLayer recomputes and constant-time compares, rejecting tampered or stale requests](img/auth-hmac.png)](img/auth-hmac.png)

> **New to a term here?** *HMAC*, *shared secret*, *replay*, *constant-time compare* —
> see the [glossary](glossary.md).

> **Source:** `rustango::hmac_auth` (`HmacAuthLayer`, `KeyResolver`, `sign_now`,
> `sign_request`) — behind the `hmac-auth` feature (on by default; replay
> protection additionally needs `cache`).
>
> **Runnable version:** every snippet is copied from
> [`auth_hmac_doc.rs`](../crates/rustango/tests/auth_hmac_doc.rs)
> (`cargo test -p rustango --test auth_hmac_doc`).

## Table of contents

- [When to use it](#when-to-use-it)
- [What gets signed](#what-gets-signed)
- [Server: verify with the layer](#server-verify-with-the-layer)
- [Client: sign a request](#client-sign-a-request)
- [Clock skew and replay](#clock-skew-and-replay)
- [Limits](#limits)
- [See also](#see-also)

---

## When to use it

| Use… | When |
|---|---|
| [API key](auth-api-keys.md) (Bearer) | Simple machine auth; capture risk is acceptable (TLS, short rotation). |
| **HMAC signing** | You need **per-request integrity + replay resistance** — webhooks, partner APIs, anything where a captured request must not be reusable or modifiable. |
| [JWT](auth-jwt.md) | Stateless, self-describing user tokens with claims. |

HMAC needs both sides to hold the same secret out-of-band (you provision it),
and reasonably synced clocks.

---

## What gets signed

The client builds a canonical string and HMAC-SHA256s it with the shared secret:

```text
<UPPERCASE-METHOD>\n
<PATH>\n
<SORTED-QUERY>\n
<X-DATE>\n
<HEX-SHA256(BODY)>
```

Two request headers carry the result:

- `X-Date` — an RFC 3339 timestamp (also part of the signed string).
- `Authorization: HMAC-SHA256 keyId=<id>,signature=<base64>`

Because the query is **sorted** on both ends, `?b=2&a=1` and `?a=1&b=2` produce
the same signature. Because the body is hashed into the string, changing a single
byte invalidates it.

---

## Server: verify with the layer

`HmacAuthLayer::new` takes a **`KeyResolver`** — a closure mapping a `keyId` to
its secret (`None` ⇒ unknown key ⇒ 401). Attach it as a normal tower layer in
front of the routes you want to protect:

```rust
use std::sync::Arc;
use rustango::hmac_auth::{HmacAuthLayer, KeyResolver};
use tower::Layer;

// Resolve key ids to secrets — back this with your DB / secret store.
let resolver: KeyResolver = Arc::new(|key_id: &str| {
    (key_id == "k_demo").then(|| b"shared-secret-at-least-32-bytes-long!!".to_vec())
});

let layer = HmacAuthLayer::new(resolver)
    .tolerance_secs(300);                 // ±5 min clock-skew window (default)

let app = protected_router.layer(layer);
```

A correctly-signed request passes; tamper with the body, drop `X-Date`, or sign
with an unknown key and it's a `401`:

```rust
// correctly signed            → 200
// body changed after signing  → 401  (signature mismatch)
// missing X-Date header       → 401
// keyId the resolver rejects  → 401
```

> **No identity extractor.** The layer verifies the signature but does **not**
> inject which `keyId` signed into the request — there's no `HmacUser` extractor.
> If a handler needs the caller identity, wrap the layer or carry it yourself.
> Rejections are plain `401`/`413` responses, not a typed error you match on.

---

## Client: sign a request

`sign_now` signs with the current time and returns the two header values to
attach (`sign_request` is the variant that takes an explicit RFC 3339 date):

```rust
use rustango::hmac_auth::sign_now;

let body = br#"{"amount": 100}"#;
let (x_date, authorization) =
    sign_now("k_demo", b"shared-secret-at-least-32-bytes-long!!",
             "POST", "/api/charge", "", body);

// Attach both headers and send the EXACT body you signed:
let req = http::Request::post("/api/charge")
    .header("x-date", x_date)
    .header("authorization", authorization)
    .body(body.to_vec())?;
```

The signature is base64; the body-hash inside the canonical string is hex. Send
the body byte-for-byte as signed — any proxy that rewrites it (recompression,
JSON re-serialization) breaks verification.

---

## Clock skew and replay

The `X-Date` timestamp bounds replay: a request whose date is outside
`tolerance_secs` (default ±300 s) is rejected, so a captured request is only
reusable inside that short window. To close it entirely, attach a **nonce store**
(any `cache::Cache`) and each signature can be spent only once within the window:

```rust
use rustango::cache::InMemoryCache;

let layer = HmacAuthLayer::new(resolver)
    .tolerance_secs(120)
    .nonce_store(Arc::new(InMemoryCache::new()));  // reject replays
```

In production use a **shared** store (Redis) so the protection holds across
replicas — an in-process cache only guards one instance. The replay check fails
open on a cache error (availability over the narrow in-window risk).

---

## Limits

- **Symmetric ±skew, RFC 3339 dates.** Both clocks must be roughly synced; the
  client must send the same timestamp it signed (`sign_now` returns it for you).
- **Full body buffering.** The body is read into memory to hash it (default cap
  10 MiB → `413`; raise with `.body_limit(n)` but mind memory). Streaming bodies
  aren't supported.
- **Signature is base64 on the wire, body-hash is hex** — easy to mix up when
  writing a client in another language.
- **Keep the layer outermost** relative to anything that mutates the body.

---

## See also

- [API keys](auth-api-keys.md) — simpler bearer credential when integrity/replay
  aren't a concern.
- [Auth backends](auth-backends.md) — for identifying a *user* per request (HMAC
  proves message integrity, not a session identity).
- [Webhooks](security.md) — the inbound counterpart: verifying signatures on
  events you receive.
- [Middleware](middleware.md) — how tower layers attach and order.
