//! Cookbook Chapter 20 — Health checks.
//!
//! `rustango::health::HealthRouter` mounts `GET /health` (liveness — always
//! `200 {"status":"ok"}`, touches nothing) and `GET /ready` (readiness —
//! runs each registered check, `200` when all pass, `503` when any fails).
//! Point a load balancer at `/health` and a deploy/rollout gate at
//! `/ready`.
//!
//! No DB here: a lazy pool never connects and `.skip_db_probe()` drops the
//! built-in `SELECT 1`, so the recipe runs anywhere. In production you'd
//! keep the DB probe and add `.tcp_probe` / `.cache_probe` / `.http_probe`
//! / custom `.check(...)` for each downstream.
//!
//! Run: `cargo test --test cookbook_chapter20_health`

use rustango::health::HealthRouter;
use rustango::sql::sqlx;
use rustango::test_client::TestClient;

/// A lazy pool validates the URL but never opens a connection; paired with
/// `.skip_db_probe()` the readiness handler never touches a DB.
fn lazy_pool() -> sqlx::PgPool {
    sqlx::PgPool::connect_lazy("postgres://u:p@localhost/db").unwrap()
}

// §20.148 — `/health` is liveness: 200 with `{"status":"ok"}`, no probes.
#[tokio::test]
async fn health_liveness_is_ok() {
    let app = HealthRouter::new(lazy_pool()).skip_db_probe().into_router();
    let r = TestClient::new(app).get("/health").send().await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json_value()["status"], "ok");
}

// §20.149 — `/ready` runs each registered check; all pass → 200, and each
// check reports its own status + latency.
#[tokio::test]
async fn ready_ok_when_all_checks_pass() {
    let app = HealthRouter::new(lazy_pool())
        .skip_db_probe()
        .check("cache_warm", || async { Ok(()) })
        .into_router();
    let r = TestClient::new(app).get("/ready").send().await;
    assert_eq!(r.status, 200);
    let v = r.json_value();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["checks"]["cache_warm"]["status"], "ok");
    assert!(v["checks"]["cache_warm"]["latency_ms"].is_number());
}

// §20.150 — a failing check flips `/ready` to 503 (deploy gate holds), and
// the body names which downstream is unhealthy.
#[tokio::test]
async fn ready_503_when_a_check_fails() {
    let app = HealthRouter::new(lazy_pool())
        .skip_db_probe()
        .check("ok", || async { Ok(()) })
        .check("payments", || async { Err("gateway timeout".into()) })
        .into_router();
    let r = TestClient::new(app).get("/ready").send().await;
    assert_eq!(r.status, 503);
    let v = r.json_value();
    assert_eq!(v["status"], "error");
    assert_eq!(v["checks"]["payments"]["status"], "error");
    assert_eq!(v["checks"]["payments"]["error"], "gateway timeout");
    assert_eq!(v["checks"]["ok"]["status"], "ok");
}
