//! Admin showcase server. Mounts the auto-admin at `/admin` behind a login,
//! with a title, two registered bulk actions, and a self-seeding hook so the
//! admin has data to browse on first boot. Companion to docs/admin.md.

use admin_demo::{admin_router, seed, Author, Comment, Post, Tag};
use rustango::sql::sqlx::PgPool;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    // Force the lib's inventory registrations (models + the comments inline)
    // to link into this binary.
    let _ = (
        std::any::type_name::<Author>(),
        std::any::type_name::<Tag>(),
        std::any::type_name::<Post>(),
        std::any::type_name::<Comment>(),
    );

    let api = axum::Router::new().nest("/admin", admin_router(pool.clone()));

    let seed_pool = pool.clone();
    rustango::manage::Cli::new()
        .api(api)
        .seed(move |_registry| {
            let p = seed_pool.clone();
            async move { seed(&p).await }
        })
        .with_health()
        .run()
        .await
}
