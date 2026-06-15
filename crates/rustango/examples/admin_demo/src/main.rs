//! Admin showcase server. Mounts the auto-admin at `/admin` with a title, two
//! registered bulk actions, and a self-seeding hook so the admin has data to
//! browse on first boot. Companion to docs/admin.md.

use admin_demo::{seed, Author, Comment, Post, Tag};
use rustango::admin;
use rustango::core::SqlValue;
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

    let admin_router = admin::Builder::new(pool.clone())
        .title("Admin Demo")
        .subtitle("rustango auto-admin showcase")
        .admin_prefix("/admin")
        .register_action("posts", "publish", |pool, pks| {
            Box::pin(async move { set_status(pool, pks, "published").await })
        })
        .register_action("posts", "archive", |pool, pks| {
            Box::pin(async move { set_status(pool, pks, "archived").await })
        })
        .build();

    let api = axum::Router::new().nest("/admin", admin_router);

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

/// Bulk action handler — sets `status` on every selected post.
async fn set_status(
    pool: &rustango::sql::Pool,
    pks: &[SqlValue],
    status: &str,
) -> Result<(), admin::AdminError> {
    let ids: Vec<String> = pks
        .iter()
        .filter_map(|v| match v {
            SqlValue::I64(n) => Some(n.to_string()),
            SqlValue::I32(n) => Some(n.to_string()),
            _ => None,
        })
        .collect();
    if !ids.is_empty() {
        // ids are integers parsed from the admin — safe to inline.
        let sql = format!("UPDATE posts SET status = '{status}' WHERE id IN ({})", ids.join(", "));
        rustango::sql::raw_execute_pool(pool, &sql, Vec::new()).await?;
    }
    Ok(())
}
