//! Runnable rustango admin demo.
//!
//! Spins up a tiny blog-shaped schema (`User`, `Post`, `AuditLog`),
//! seeds a few rows, and serves the admin on `127.0.0.1:8080`.
//!
//! # Run
//! Make sure Postgres is up (`docker compose up -d` from the repo root)
//! and then:
//!
//! ```text
//! cargo run --example admin_demo
//! ```
//!
//! Open <http://127.0.0.1:8080/> in a browser. Login with
//! `admin` / `secret`.
//!
//! - Click `User` → list view → `+ new` → fill the form → save
//! - Click into a row → `edit` → change something → save
//! - Click `delete` (confirms first)
//! - `AuditLog` is mounted as **read-only**: visible, no edit/delete
//!   buttons, and a direct POST returns 403

use rustango::admin;
use rustango::migrate;
use rustango::sql::sqlx::PgPool;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "demo_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32)]
    username: String,
    #[rustango(max_length = 100)]
    email: Option<String>,
    #[rustango(min = 0, max = 150)]
    age: i32,
    is_active: bool,
    joined: chrono::DateTime<chrono::Utc>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "demo_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    body: String,
    #[rustango(fk = "demo_user", on = "id")]
    author_id: i64,
    published: bool,
    created: chrono::DateTime<chrono::Utc>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "demo_audit_log")]
#[allow(dead_code)]
pub struct AuditLog {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    message: String,
    at: chrono::DateTime<chrono::Utc>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://rustango:rustango@localhost:5432/rustango_test".into());

    let pool = PgPool::connect(&url).await?;
    println!("→ connected to {url}");

    migrate::drop_all(&pool).await?;
    migrate::apply_all(&pool).await?;
    println!("→ schema applied");

    seed(&pool).await?;
    println!("→ seeded demo data");

    let app = admin::Builder::new(pool)
        .read_only(["demo_audit_log"])
        .build();
    let app = admin::protect_with_basic_auth(app, "admin", "secret");

    let bind = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!();
    println!("  rustango admin demo");
    println!("  →  http://{bind}/");
    println!("  →  login: admin / secret");
    println!("  →  Ctrl-C to stop");
    println!();

    axum::serve(listener, app).await?;
    Ok(())
}

async fn seed(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = chrono::Utc::now();

    for u in [
        User {
            id: 1,
            username: "alice".into(),
            email: Some("alice@example.com".into()),
            age: 30,
            is_active: true,
            joined: now,
        },
        User {
            id: 2,
            username: "bob".into(),
            email: None,
            age: 45,
            is_active: false,
            joined: now,
        },
        User {
            id: 3,
            username: "carol".into(),
            email: Some("carol@example.com".into()),
            age: 28,
            is_active: true,
            joined: now,
        },
    ] {
        u.insert(pool).await?;
    }

    for p in [
        Post {
            id: 1,
            title: "Hello, rustango".into(),
            body: "First post.".into(),
            author_id: 1,
            published: true,
            created: now,
        },
        Post {
            id: 2,
            title: "Draft thoughts".into(),
            body: "Still writing this one.".into(),
            author_id: 1,
            published: false,
            created: now,
        },
        Post {
            id: 3,
            title: "On bunnies".into(),
            body: "They eat carrots.".into(),
            author_id: 3,
            published: true,
            created: now,
        },
    ] {
        p.insert(pool).await?;
    }

    for log in [
        AuditLog {
            id: 1,
            message: "schema applied".into(),
            at: now,
        },
        AuditLog {
            id: 2,
            message: "demo seed loaded".into(),
            at: now,
        },
    ] {
        log.insert(pool).await?;
    }
    Ok(())
}
