//! `AppBuilder` + SQLite — minimal runnable bootstrap.
//!
//! Demonstrates the v0.27 bi-dialect single-pool app shape: connect
//! via `DATABASE_URL` (any backend), bootstrap a model schema,
//! mount an axum handler that pulls `&Pool` out of the request
//! extension, serve.
//!
//! Run with:
//!
//! ```sh
//! mkdir -p var
//! DATABASE_URL='sqlite:./var/app.db?mode=rwc' \
//!   cargo run -p rustango --example sqlite_app_demo --features sqlite
//! ```
//!
//! Then in another terminal:
//!
//! ```sh
//! curl -X POST http://localhost:8080/users -d '{"name":"alice"}' \
//!      -H 'content-type: application/json'
//! curl http://localhost:8080/users
//! ```
//!
//! Switch to Postgres without changing a line of code:
//!
//! ```sh
//! DATABASE_URL='postgres://rustango:rustango@localhost:5432/demo' \
//!   cargo run -p rustango --example sqlite_app_demo --features sqlite,postgres
//! ```

#![cfg(feature = "sqlite")]

use std::sync::Arc;

use axum::{extract::Extension, routing::get, Json, Router};
use rustango::core::Model as _;
use rustango::server::AppBuilder;
use rustango::sql::{Auto, FetcherPool, Pool};
use rustango::Model;
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Clone, Serialize)]
#[rustango(table = "demo_user", display = "name")]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Deserialize)]
struct CreateUser {
    name: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `DATABASE_URL` decides the backend. SQLite default keeps the
    // demo runnable without any setup.
    if std::env::var("DATABASE_URL").is_err() {
        std::env::set_var("DATABASE_URL", "sqlite:./var/app.db?mode=rwc");
        std::fs::create_dir_all("./var").ok();
    }

    AppBuilder::from_env()
        .await?
        .bootstrap(&[User::SCHEMA])
        .await?
        .api(routes())
        .serve("0.0.0.0:8080")
        .await
}

fn routes() -> Router {
    Router::new()
        .route("/users", get(list_users).post(create_user))
        .route("/healthz", get(|| async { "ok" }))
}

async fn list_users(Extension(pool): Extension<Arc<Pool>>) -> Json<Vec<User>> {
    let users = User::objects()
        .order_by(&[("id", false)])
        .fetch_pool(&pool)
        .await
        .expect("fetch users");
    Json(users)
}

async fn create_user(
    Extension(pool): Extension<Arc<Pool>>,
    Json(payload): Json<CreateUser>,
) -> Json<User> {
    let mut u = User {
        id: Auto::Unset,
        name: payload.name,
    };
    u.insert_pool(&pool).await.expect("insert");
    Json(u)
}
