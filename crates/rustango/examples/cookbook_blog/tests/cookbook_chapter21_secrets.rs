//! Cookbook Chapter 21 — Secrets manager.
//!
//! `rustango::secrets::Secrets` is a pluggable backend for pulling secrets
//! out of your code and config: `EnvSecrets` (environment variables, with
//! an optional prefix), `InMemorySecrets` (tests / static config), or your
//! own impl (AWS Secrets Manager, Vault, …). `get` returns `Option`;
//! `require` errors when a secret is missing. Store it as `BoxedSecrets`
//! (`Arc<dyn Secrets>`) so the rest of the app is backend-agnostic.
//!
//! All in-process, no DB. Run: `cargo test --test cookbook_chapter21_secrets`

use rustango::secrets::{BoxedSecrets, EnvSecrets, InMemorySecrets, Secrets};
use std::sync::Arc;

// §21.151 — the pluggable backend: `get` is `Option`, `require` errors on
// a missing key. Swap `InMemorySecrets` for any `Secrets` impl.
#[tokio::test]
async fn in_memory_backend_get_and_require() {
    let secrets: BoxedSecrets =
        Arc::new(InMemorySecrets::new().with(&[("STRIPE_KEY", "sk_test_123")]));

    assert_eq!(secrets.get("STRIPE_KEY").await.unwrap().as_deref(), Some("sk_test_123"));
    // Missing key → get is None…
    assert_eq!(secrets.get("MISSING").await.unwrap(), None);
    // …and require is an error (fail fast at startup, not at first use).
    assert!(secrets.require("MISSING").await.is_err());
    assert_eq!(secrets.require("STRIPE_KEY").await.unwrap(), "sk_test_123");
}

// §21.152 — the env backend applies a prefix: `get("DB_PASSWORD")` on a
// `with_prefix("COOKBOOK_")` store reads `COOKBOOK_DB_PASSWORD`.
#[tokio::test]
async fn env_backend_with_prefix() {
    // (edition-2021 crate → set_var is safe.)
    std::env::set_var("COOKBOOK21_DB_PASSWORD", "hunter2");

    let secrets = EnvSecrets::with_prefix("COOKBOOK21_");
    assert_eq!(secrets.get("DB_PASSWORD").await.unwrap().as_deref(), Some("hunter2"));
    // Unset var → None.
    assert_eq!(secrets.get("NOT_SET").await.unwrap(), None);

    std::env::remove_var("COOKBOOK21_DB_PASSWORD");
}

// A custom backend (AWS Secrets Manager, Vault, …) is just an
// `impl Secrets` — the app depends only on `BoxedSecrets`, so swapping
// backends never touches call sites. See `docs`/the module docs for the
// `#[async_trait] impl Secrets for … {}` shape.
