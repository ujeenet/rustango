//! `manage.rs`-shaped runnable example for rustango.
//!
//! Demonstrates the Django-style migration UX end-to-end against the
//! docker Postgres. Lives here as an example so you can try it without
//! creating your own crate.
//!
//! ```text
//! docker compose up -d
//! cargo run --example manage_demo -- showmigrations
//! cargo run --example manage_demo -- makemigrations
//! cargo run --example manage_demo -- migrate
//! cargo run --example manage_demo -- downgrade
//! cargo run --example manage_demo -- migrate
//! ```
//!
//! Migrations are written to `./manage_demo_migrations/` in the
//! current working directory (gitignored). `DATABASE_URL` defaults to
//! the docker compose creds.
//!
//! In your real project you'd put this file at `src/bin/manage.rs`,
//! point `dir` at `./migrations`, and import your own models instead
//! of the `Author`/`Article` shown below.

use rustango::sql::sqlx::PgPool;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "manage_demo_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 50)]
    name: String,
    bio: Option<String>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "manage_demo_article")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(fk = "manage_demo_author", on = "id")]
    author_id: i64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://rustango:rustango@127.0.0.1:5432/rustango_test".into());
    let pool = PgPool::connect(&url).await?;

    let dir = std::path::Path::new("./manage_demo_migrations");
    let args = std::env::args().skip(1);

    rustango::migrate::manage::run(&pool, dir, args).await?;
    Ok(())
}
