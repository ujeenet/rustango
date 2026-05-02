//! Health check endpoints — `/health` (liveness) and `/ready` (readiness).
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::health::health_router;
//! use axum::Router;
//!
//! let app = Router::new()
//!     .merge(health_router(pool.clone()))
//!     .route("/api/posts", ...);
//! ```
//!
//! ## Endpoints
//!
//! - **`GET /health`** — liveness. Returns `200 OK` with `{"status":"ok"}`.
//!   Always succeeds — used by orchestrators to detect process crashes.
//! - **`GET /ready`** — readiness. Pings the database with `SELECT 1`.
//!   Returns `200 OK` when the DB is reachable, `503` otherwise.
//!
//! ## Custom checks
//!
//! Add additional checks (Redis, S3, external APIs) via [`HealthRouter::check`]:
//!
//! ```ignore
//! use rustango::health::HealthRouter;
//!
//! let app = HealthRouter::new(pool.clone())
//!     .check("redis", || async {
//!         redis_ping().await.map_err(|e| e.to_string())
//!     })
//!     .into_router();
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::sql::sqlx::{self, PgPool};

/// Async health-check function: returns `Ok(())` when healthy, `Err(message)`
/// otherwise. Each check has a name shown in the JSON response.
pub type CheckFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Builder for the health router.
pub struct HealthRouter {
    pool: PgPool,
    extra_checks: Vec<(String, CheckFn)>,
}

#[derive(Clone)]
struct HealthState {
    pool: PgPool,
    extra_checks: Arc<Vec<(String, CheckFn)>>,
}

impl HealthRouter {
    /// Create a health router with the default `/health` + `/ready` endpoints.
    /// `/ready` pings `pool` with `SELECT 1`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool, extra_checks: Vec::new() }
    }

    /// Register an additional check that runs as part of `/ready`.
    ///
    /// `name` appears in the JSON response. `check` returns `Ok(())` on
    /// success or `Err(message)` on failure.
    #[must_use]
    pub fn check<F, Fut>(mut self, name: &str, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let boxed: CheckFn = Arc::new(move || Box::pin(check()));
        self.extra_checks.push((name.to_owned(), boxed));
        self
    }

    /// Build the axum router.
    pub fn into_router(self) -> Router {
        let state = HealthState {
            pool: self.pool,
            extra_checks: Arc::new(self.extra_checks),
        };
        Router::new()
            .route("/health", get(handle_live))
            .route("/ready", get(handle_ready))
            .with_state(state)
    }
}

/// Convenience — `HealthRouter::new(pool).into_router()` in one call.
#[must_use]
pub fn health_router(pool: PgPool) -> Router {
    HealthRouter::new(pool).into_router()
}

async fn handle_live() -> Response {
    (StatusCode::OK, Json(json!({"status": "ok"}))).into_response()
}

async fn handle_ready(State(state): State<HealthState>) -> Response {
    let mut checks = serde_json::Map::new();
    let mut all_ok = true;

    // Database check
    let db_result = sqlx::query("SELECT 1").execute(&state.pool).await;
    match db_result {
        Ok(_) => {
            checks.insert("database".into(), json!({"status": "ok"}));
        }
        Err(e) => {
            all_ok = false;
            checks.insert(
                "database".into(),
                json!({"status": "error", "error": e.to_string()}),
            );
        }
    }

    // Extra checks
    for (name, check) in state.extra_checks.iter() {
        match check().await {
            Ok(()) => {
                checks.insert(name.clone(), json!({"status": "ok"}));
            }
            Err(e) => {
                all_ok = false;
                checks.insert(name.clone(), json!({"status": "error", "error": e}));
            }
        }
    }

    let body = Value::Object({
        let mut m = serde_json::Map::new();
        m.insert(
            "status".into(),
            Value::String(if all_ok { "ok" } else { "error" }.to_owned()),
        );
        m.insert("checks".into(), Value::Object(checks));
        m
    });
    let status = if all_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(body)).into_response()
}
