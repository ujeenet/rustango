//! Migration demo — step 1 of 2.
//!
//! Bootstraps a tiny blog-shaped schema (`Author`, `Article`, `TempTag`)
//! against the docker Postgres, then writes the initial schema snapshot
//! to `/tmp/rustango_migrate_demo/0001_initial.json`. Run
//! `migrate_demo_v2` next to see the diff/apply side.
//!
//! ```text
//! cargo run --example migrate_demo_v1
//! cargo run --example migrate_demo_v2
//! ```

use std::path::PathBuf;

use rustango::sql::sqlx::{self, PgPool};
use rustango::{migrate, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "mig_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 50)]
    name: String,
    joined: chrono::DateTime<chrono::Utc>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mig_article")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(fk = "mig_author", on = "id")]
    author_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mig_temp_tag")]
#[allow(dead_code)]
pub struct TempTag {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 30)]
    label: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://rustango:rustango@127.0.0.1:5432/rustango_test".into());
    let pool = PgPool::connect(&url).await?;

    let dir: PathBuf = "/tmp/rustango_migrate_demo".into();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;

    sqlx::query("DROP TABLE IF EXISTS mig_comment, mig_temp_tag, mig_article, mig_author CASCADE")
        .execute(&pool)
        .await?;

    println!("=== migrate_demo v1: bootstrap ===\n");
    println!("Models registered in this binary:");
    let mut models = migrate::registered_models();
    models.sort_by_key(|m| m.table);
    for m in &models {
        println!("  - {} ({})", m.name, m.table);
    }

    migrate::apply_all(&pool).await?;
    println!("\napply_all OK — three tables created with FKs.");

    let snap = migrate::SchemaSnapshot::from_registry();
    let path = dir.join("0001_initial.json");
    std::fs::write(&path, serde_json::to_string_pretty(&snap)?)?;
    println!("Wrote initial snapshot → {}", path.display());

    sqlx::query("INSERT INTO mig_author (id, name, joined) VALUES (1, 'Ada', NOW())")
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO mig_article (id, title, author_id) VALUES (1, 'Hello', 1)")
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO mig_temp_tag (id, label) VALUES (1, 'draft')")
        .execute(&pool)
        .await?;
    println!("Seeded one row per table.\n");

    println!("Next: cargo run --example migrate_demo_v2");
    Ok(())
}
