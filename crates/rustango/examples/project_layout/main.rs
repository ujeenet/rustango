//! Multi-file example: Django-shape `models.rs / views.rs / urls.rs`
//! layout for a downstream rustango project.
//!
//! ```text
//! examples/project_layout/
//!   main.rs       — boots the binary, ties everything together
//!   models.rs     — every #[derive(Model)] lives here
//!   views.rs      — request handlers (Django-style "views")
//!   urls.rs       — single Router builder mapping paths → handlers
//! ```
//!
//! In a real project, replace `examples/project_layout/` with
//! `src/`, drop `models.rs / views.rs / urls.rs` next to your own
//! `main.rs`, and you're set. Add new models in `models.rs` and they
//! show up in the auto-admin automatically.
//!
//! # Run
//!
//! Postgres up (`docker compose up -d`), then:
//!
//! ```text
//! cargo run --example project_layout
//! ```
//!
//! Visit:
//! * <http://127.0.0.1:8082/>                  — landing page with links
//! * <http://127.0.0.1:8082/admin>             — the auto-admin
//! * <http://127.0.0.1:8082/posts/published>   — custom JSON view
//! * <http://127.0.0.1:8082/users/1>           — custom JSON view
//! * <http://127.0.0.1:8082/healthz>           — liveness probe

mod models;
mod urls;
mod views;

use rustango::sql::sqlx::PgPool;
use rustango::sql::Auto;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://rustango:rustango@localhost:5432/rustango_test".into()
    });
    let pool = PgPool::connect(&url).await?;

    // Fresh schema on every run — keep the example self-contained.
    for sql in [
        "DROP TABLE IF EXISTS layout_post",
        "DROP TABLE IF EXISTS layout_user",
    ] {
        rustango::sql::sqlx::query(sql).execute(&pool).await?;
    }
    rustango::migrate::apply_all(&pool).await?;

    // Seed a couple rows so the views have something to render.
    let mut alice = models::User {
        id: Auto::default(),
        username: "alice".into(),
        active: true,
    };
    alice.save(&pool).await?;
    let alice_pk = alice.id.get().copied().unwrap_or_default();

    for (title, published) in [("Hello, World", true), ("Draft", false)] {
        let mut post = models::Post {
            id: Auto::default(),
            title: title.into(),
            author: rustango::sql::ForeignKey::unloaded(alice_pk),
            published,
        };
        post.save(&pool).await?;
    }

    let app = urls::router(pool);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8082").await?;
    eprintln!(
        "project_layout demo on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}
