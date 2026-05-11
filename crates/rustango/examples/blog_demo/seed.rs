//! Idempotent seed — provisions the demo tenant, operator, user, and
//! three authors with two posts each using the ORM. Re-running is safe:
//! `create_*_if_missing` calls are no-ops if the row already exists, and
//! the author insert block is guarded by an `is_empty()` check.
//!
//! To regenerate `migrations/0001_blog_initial.json` from the model
//! definitions, run from the workspace root:
//!
//!   cargo run --bin manage --features tenancy -- \
//!       makemigrations blog_initial \
//!       --migrations-dir crates/rustango/examples/blog_demo/migrations

use std::path::Path;
use std::sync::Arc;

use rustango::sql::{sqlx::PgPool, Auto, ForeignKey};
use rustango::tenancy::{
    manage::api::{
        create_operator_if_missing, create_tenant_if_missing, create_user_if_missing,
        CreateTenantOpts,
    },
    StorageMode, TenantPools,
};

use crate::models::{Author, Post};

pub const TENANT_SLUG: &str = "acme";
const MIGRATIONS_DIR: &str = "crates/rustango/examples/blog_demo/migrations";

pub async fn run(
    pools: Arc<TenantPools>,
    _registry: PgPool,
    registry_url: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let migrations_dir = Path::new(MIGRATIONS_DIR);

    // ── provisioning (all idempotent) ─────────────────────────────────
    let org = create_tenant_if_missing(
        &pools,
        &registry_url,
        migrations_dir,
        TENANT_SLUG,
        CreateTenantOpts {
            mode: StorageMode::Schema,
            display_name: Some("ACME Blog".into()),
            ..Default::default()
        },
    )
    .await?;

    create_operator_if_missing(&pools, "admin", "letmein").await?;
    create_user_if_missing(&pools, TENANT_SLUG, "alice", "hunter2", true).await?;

    // ── seed data (skip if already present) ───────────────────────────
    let mut conn = pools.acquire(&org).await?;
    let conn_ref: &mut rustango::sql::sqlx::PgConnection = &mut conn;

    if Author::objects().fetch_on(&mut *conn_ref).await?.is_empty() {
        // Authors
        let mut ada = Author {
            id: Auto::default(),
            name: "Ada Lovelace".into(),
            bio: "Pioneer of analytical engines and the very idea of software.".into(),
        };
        ada.save_on(&mut *conn_ref).await?;

        let mut grace = Author {
            id: Auto::default(),
            name: "Grace Hopper".into(),
            bio: "Invented the compiler. Made computers speak human.".into(),
        };
        grace.save_on(&mut *conn_ref).await?;

        let mut margaret = Author {
            id: Auto::default(),
            name: "Margaret Hamilton".into(),
            bio: "Her software took Apollo 11 to the Moon.".into(),
        };
        margaret.save_on(&mut *conn_ref).await?;

        let ada_id = ada.id.get().copied().unwrap();
        let grace_id = grace.id.get().copied().unwrap();
        let margaret_id = margaret.id.get().copied().unwrap();

        // Posts
        let posts: &mut [Post] = &mut [
            Post {
                id: Auto::default(),
                title: "On the Analytical Engine".into(),
                body: "Notes on Babbage's design and how patterns of operation foreshadow software.".into(),
                author_id: ForeignKey::unloaded(ada_id),
                published_at: chrono::Utc::now(),
                featured: true,
            },
            Post {
                id: Auto::default(),
                title: "Looping and the Punch Card".into(),
                body: "How iteration was encoded before electricity powered the computation.".into(),
                author_id: ForeignKey::unloaded(ada_id),
                published_at: chrono::Utc::now(),
                featured: false,
            },
            Post {
                id: Auto::default(),
                title: "Why Compilers Matter".into(),
                body: "The jump from machine code to human-readable language changed everything.".into(),
                author_id: ForeignKey::unloaded(grace_id),
                published_at: chrono::Utc::now(),
                featured: false,
            },
            Post {
                id: Auto::default(),
                title: "Debugging at 3am".into(),
                body: "The literal moth, the metaphor, and what rigorous testing really means.".into(),
                author_id: ForeignKey::unloaded(grace_id),
                published_at: chrono::Utc::now(),
                featured: false,
            },
            Post {
                id: Auto::default(),
                title: "Software as Mission-Critical Infrastructure".into(),
                body: "When a program has to work the first time, every time, there are no second chances.".into(),
                author_id: ForeignKey::unloaded(margaret_id),
                published_at: chrono::Utc::now(),
                featured: false,
            },
            Post {
                id: Auto::default(),
                title: "Priority Displays and Error Recovery".into(),
                body: "How Apollo's onboard computer saved the mission by doing less, not more.".into(),
                author_id: ForeignKey::unloaded(margaret_id),
                published_at: chrono::Utc::now(),
                featured: false,
            },
        ];
        for post in posts.iter_mut() {
            post.save_on(&mut *conn_ref).await?;
        }
    }

    Ok(())
}
