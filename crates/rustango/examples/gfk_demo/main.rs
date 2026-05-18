//! gfk_demo — runnable showcase for the full GenericForeignKey surface
//! (epic #246). SQLite-backed, single-binary, no tenancy.
//!
//! Demonstrates:
//!
//! - `#[rustango(generic_fk(...))]` declarations on two child models
//!   (`Tag`, `Comment`) pointing at two unrelated target models
//!   (`Post`, `Article`)
//! - Typed accessor + setter codegen (`comment.target_pool(&pool)`,
//!   `comment.set_target_for::<Post>(&pool, pk)`)
//! - `register_admin_inline_generic!` panels — both read-only display
//!   (#242) on the parent detail page AND editable formset edit
//!   (#243) on the parent edit page
//! - List-view rendering of GFK pairs as clickable links (#241)
//! - ContentType `<select>` picker on the standalone Tag / Comment
//!   create/edit forms (#244)
//!
//! ## Run
//!
//! ```sh
//! mkdir -p var
//! DATABASE_URL='sqlite:./var/gfk_demo.db?mode=rwc' \
//!   cargo run -p rustango --example gfk_demo \
//!     --features sqlite,admin,runserver
//! ```
//!
//! Then visit:
//!
//! - <http://localhost:8080/>                   (admin index)
//! - <http://localhost:8080/gfkdemo_post/1>     (post detail w/ Tags + Comments panels)
//! - <http://localhost:8080/gfkdemo_article/1>  (article detail w/ Tags + Comments panels)
//! - <http://localhost:8080/gfkdemo_post/1/edit> (post edit — inline FormSets editable)
//! - <http://localhost:8080/gfkdemo_tag>        (list — `target` column is one clickable link)
//! - <http://localhost:8080/gfkdemo_tag/new>    (create form — `content_type_id` is a CT `<select>`)

#![cfg(feature = "sqlite")]

mod models;
mod seed;

use rustango::core::Model as _;
use rustango::server::AppBuilder;
use rustango::session::SessionSecret;

use crate::models::{Article, Comment, Post, Tag};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("DATABASE_URL").is_err() {
        std::env::set_var("DATABASE_URL", "sqlite:./var/gfk_demo.db?mode=rwc");
        std::fs::create_dir_all("./var").ok();
    }

    println!("→ bootstrapping schema + seeding demo data…");
    let app = AppBuilder::from_env()
        .await?
        .bootstrap(&[
            Post::SCHEMA,
            Article::SCHEMA,
            Tag::SCHEMA,
            Comment::SCHEMA,
            rustango::admin::AdminUser::SCHEMA,
        ])
        .await?;

    // Build the Pool once, then reuse for both seed + admin mount.
    let pool = app.pool().clone();
    seed::run(&pool).await?;

    // #253 — opt into signed-cookie session auth. The signing key
    // is loaded from `RUSTANGO_SESSION_SECRET` (production path) or
    // generated randomly for dev. Pair with the admin user seeded
    // by `seed::run` (default credentials: admin / admin).
    let secret = SessionSecret::from_env_or_random();
    let admin = rustango::admin::Builder::new(pool)
        .admin_prefix("")
        .with_session_auth(secret)
        .build();

    let user = std::env::var("RUSTANGO_DEMO_USER").unwrap_or_else(|_| "admin".to_owned());
    let pass = std::env::var("RUSTANGO_DEMO_PASS").unwrap_or_else(|_| "admin".to_owned());

    println!("✓ ready. open http://localhost:8080/");
    println!();
    println!("  Login form:  /login");
    println!("  Credentials:  {user} / {pass}");
    println!("  Override via RUSTANGO_DEMO_USER / RUSTANGO_DEMO_PASS");
    println!();
    println!("  Try:");
    println!("    • /gfkdemo_post/1       — detail page with Tags + Comments inline panels");
    println!("    • /gfkdemo_post/1/edit  — edit page (inlines editable)");
    println!("    • /gfkdemo_tag          — list view (target column = one clickable link)");
    println!("    • /gfkdemo_tag/new      — create form (content_type_id = CT <select> picker)");
    println!();

    app.api(admin).serve("0.0.0.0:8080").await
}
