//! One tenant key layout on a `DatabaseCache`, on every dialect: every key a
//! tenant request writes starts with `tenant:#org:<id>:<slug>:`, and a purge
//! deletes all of them for org 1 `acme` and none for org 12 `acme2`.

#![cfg(all(
    feature = "tenancy",
    feature = "cache-page",
    feature = "sessions",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use rustango::cache::{BoxedCache, Cache, CacheError, CacheScope, DatabaseCache};
use rustango::cache_page::{CachePageLayer, PageKey};
use rustango::idempotency::{IdempotencyLayer, IdempotencyRouterExt};
use rustango::rate_limit::KeyBy;
use rustango::rate_limit_cache::{CacheRateLimitLayer, CacheRateLimitRouterExt};
use rustango::sql::Pool;
use rustango::tenancy::{RequestTenant, TenantSlug};
use rustango::tri_dialect_test;
use tower::ServiceExt as _;

async fn noop(_: &Pool) {}

/// Global keys a tenant request may write: single-use markers already bound
/// to their tenant.
const GLOBAL_ALLOW: [&str; 3] = ["authflow_used:", "passkey_challenge_used:", "hmac_nonce:"];

fn acme() -> TenantSlug {
    TenantSlug::new(1, "acme")
}

fn acme2() -> TenantSlug {
    TenantSlug::new(12, "acme2")
}

fn prefix(t: &TenantSlug) -> String {
    format!("tenant:{}:", t.namespace())
}

/// Forwards to `inner` and records the key of every write.
struct Recording {
    inner: BoxedCache,
    written: Mutex<Vec<String>>,
}

impl Recording {
    fn note(&self, key: &str) {
        self.written.lock().unwrap().push(key.to_owned());
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.written.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl Cache for Recording {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        self.inner.get(key).await
    }
    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        self.note(key);
        self.inner.set(key, value, ttl).await
    }
    async fn set_forever(&self, key: &str, value: &str) -> Result<(), CacheError> {
        self.note(key);
        self.inner.set_forever(key, value).await
    }
    async fn get_bytes(&self, key: &str) -> Result<Option<bytes::Bytes>, CacheError> {
        self.inner.get_bytes(key).await
    }
    async fn set_bytes(
        &self,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        self.note(key);
        self.inner.set_bytes(key, value, ttl).await
    }
    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        self.inner.delete(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool, CacheError> {
        self.inner.exists(key).await
    }
    async fn clear(&self) -> Result<(), CacheError> {
        self.inner.clear().await
    }
    async fn incr(&self, key: &str, by: i64, ttl: Option<Duration>) -> Result<i64, CacheError> {
        self.note(key);
        self.inner.incr(key, by, ttl).await
    }
    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        self.note(key);
        self.inner.add(key, value, ttl).await
    }
    async fn touch(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        self.inner.touch(key, ttl).await
    }
    async fn delete_many(&self, keys: &[&str]) -> Result<(), CacheError> {
        self.inner.delete_many(keys).await
    }
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        self.inner.delete_prefix(prefix).await
    }
    async fn delete_prefixes(&self, prefixes: &[String]) -> Result<(), CacheError> {
        self.inner.delete_prefixes(prefixes).await
    }
}

/// `/pay` writes app, session, flag and lock keys through `CacheScope`; the
/// layers add idempotency, page and rate-limit keys. `x-org` picks the tenant.
fn app(root: BoxedCache, pages: CachePageLayer) -> Router {
    let pay_root = root.clone();
    let pay = move |mut parts: Parts| {
        let root = pay_root.clone();
        async move {
            let scope = CacheScope::from_request(&root, &mut parts)
                .await
                .expect("a tenant");
            scope.app().set("stats", "1", None).await.unwrap();
            let session = rustango::sessions::Session::new();
            scope.sessions().save(&session).await.unwrap();
            scope.flags().enable_for_user("beta", "5").await.unwrap();
            let locks = scope.locks();
            let held = locks.try_acquire("job", Duration::from_secs(60)).await;
            std::mem::forget(held.expect("lock"));
            "paid"
        }
    };
    Router::new()
        .route("/pay", post(pay))
        .route("/page", get(|| async { "page" }))
        .idempotency(IdempotencyLayer::new(root.clone()))
        .layer(pages)
        .cache_rate_limit(
            CacheRateLimitLayer::new(root, 100, Duration::from_secs(60)).key_by(KeyBy::Tenant),
        )
        .layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                let t = if req.headers()["x-org"] == "1" {
                    acme()
                } else {
                    acme2()
                };
                req.extensions_mut().insert(RequestTenant::Tenant(t));
                next.run(req).await
            },
        ))
}

/// One request of each kind for `t`, plus a page invalidation.
async fn fill(app: &Router, pages: &CachePageLayer, t: &TenantSlug) {
    let org = t.org_id.to_string();
    let send = |req: axum::http::request::Builder| {
        let app = app.clone();
        let org = org.clone();
        async move {
            let req = req
                .header("host", "shop.example")
                .header("x-org", org)
                .body(Body::empty())
                .unwrap();
            let r = app.oneshot(req).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
    };
    let pay = || {
        Request::builder()
            .method("POST")
            .uri("/pay")
            .header("idempotency-key", "k1")
    };
    send(pay()).await;
    send(Request::builder().uri("/page")).await;
    pages
        .invalidate([&PageKey::new("/page", "shop.example").tenant(t)])
        .await
        .unwrap();
    send(Request::builder().uri("/page")).await;
}

/// The area of a tenant key: `tenant:<ns>:<area>:…`.
fn area<'a>(key: &'a str, t: &TenantSlug) -> &'a str {
    let rest = key.strip_prefix(&prefix(t)).expect("a tenant key");
    rest.split(':').next().unwrap()
}

async fn fresh(pool: &Pool) -> (Arc<Recording>, BoxedCache) {
    let db = DatabaseCache::new(pool.clone(), "rustango_cache_tenant_keys");
    let _ = db.drop_table().await;
    db.ensure_table().await.expect("ensure_table");
    let rec = Arc::new(Recording {
        inner: Arc::new(db),
        written: Mutex::new(Vec::new()),
    });
    (rec.clone(), rec)
}

/// Every key a tenant request writes is under its tenant prefix.
async fn a_tenant_request_writes_only_tenant_keys(pool: &Pool) {
    let (rec, root) = fresh(pool).await;
    let pages = CachePageLayer::new(root.clone());
    let app = app(root.clone(), pages.clone());
    for t in [acme(), acme2()] {
        fill(&app, &pages, &t).await;
        // The 0.63 lock and idempotency keys, written beside the new ones.
        // Remove in 0.65.0.
        let legacy_lock = format!("lock:{}", prefix(&t));
        let written = rec.take();
        assert!(!written.is_empty());
        for key in &written {
            assert!(
                key.starts_with(&prefix(&t))
                    || key.starts_with(&legacy_lock)
                    || key.starts_with("idem:")
                    || GLOBAL_ALLOW.iter().any(|g| key.starts_with(g)),
                "{} wrote `{key}`",
                t.slug
            );
        }
        let areas: BTreeSet<&str> = written
            .iter()
            .filter(|k| k.starts_with(&prefix(&t)))
            .map(|k| area(k, &t))
            .collect();
        let want = ["app", "flag", "idem", "lock", "page", "rl", "session"];
        assert_eq!(areas, want.into_iter().collect(), "{}", t.slug);
    }
    let _ = root.clear().await;
}

/// The purge removes every area of org 1 `acme`, the 0.63 lock key too, and
/// nothing of org 12 `acme2`.
async fn purge_removes_one_tenant_only(pool: &Pool) {
    let (rec, root) = fresh(pool).await;
    let pages = CachePageLayer::new(root.clone());
    let app = app(root.clone(), pages.clone());
    let mut live = Vec::new();
    for t in [acme(), acme2()] {
        fill(&app, &pages, &t).await;
        for key in rec.take() {
            // The 0.63 idempotency copy has no tenant prefix and expires
            // instead. Remove in 0.65.0.
            if key.starts_with("idem:") {
                continue;
            }
            if root.exists(&key).await.unwrap() {
                live.push((t.org_id, key));
            }
        }
    }
    let acme_areas: BTreeSet<&str> = live
        .iter()
        .filter(|(id, k)| *id == 1 && k.starts_with(&prefix(&acme())))
        .map(|(_, k)| area(k, &acme()))
        .collect();
    let want = ["app", "flag", "idem", "lock", "page", "rl", "session"];
    assert_eq!(acme_areas, want.into_iter().collect());
    // As a 0.63 pod left an app key and a lock.
    for t in [acme(), acme2()] {
        let old_app = format!("{}stats", prefix(&t));
        root.set(&old_app, "old", None).await.unwrap();
        live.push((t.org_id, old_app));
        let old_lock = format!("lock:{}nightly", prefix(&t));
        root.set(&old_lock, "pod-063", None).await.unwrap();
        live.push((t.org_id, old_lock));
    }

    CacheScope::for_tenant(root.clone(), &acme())
        .purge()
        .await
        .unwrap();

    for (id, key) in &live {
        let exists = root.exists(key).await.unwrap();
        if *id == 1 {
            assert!(!exists, "purge left `{key}`");
        } else {
            assert!(exists, "purge of acme deleted `{key}`");
        }
    }
    let _ = root.clear().await;
}

tri_dialect_test!(
    setup: noop,
    scenarios: [a_tenant_request_writes_only_tenant_keys, purge_removes_one_tenant_only]
);
