//! Migration demo — step 2 of 2.
//!
//! v2 of the schema: `Author` gains a nullable `bio` column and loses
//! `joined`; `TempTag` is gone; a new `Comment` table FKs into `Article`.
//! Reads the v1 snapshot from `/tmp/rustango_migrate_demo/0001_initial.json`,
//! diffs it against the current registry, prints the planned DDL, applies
//! it, and writes the new snapshot. Run `migrate_demo_v1` first.

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
    bio: Option<String>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mig_article")]
#[allow(dead_code)]
pub struct Article {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 200)]
    slug: Option<String>,
    #[rustango(fk = "mig_author", on = "id")]
    author_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mig_comment")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(fk = "mig_article", on = "id")]
    article_id: i64,
    body: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://rustango:rustango@127.0.0.1:5432/rustango_test".into());
    let pool = PgPool::connect(&url).await?;

    let dir: PathBuf = "/tmp/rustango_migrate_demo".into();
    let prev_path = dir.join("0001_initial.json");
    if !prev_path.exists() {
        eprintln!(
            "no v1 snapshot at {} — run `cargo run --example migrate_demo_v1` first",
            prev_path.display()
        );
        std::process::exit(1);
    }

    let prev: migrate::SchemaSnapshot =
        serde_json::from_str(&std::fs::read_to_string(&prev_path)?)?;
    let current = migrate::SchemaSnapshot::from_registry();

    println!("=== migrate_demo v2: detect changes ===\n");
    let changes = migrate::detect_changes(&prev, &current);
    if changes.is_empty() {
        println!("(no changes — registry matches snapshot)");
        return Ok(());
    }
    for c in &changes {
        println!("  - {c:?}");
    }

    println!("\n=== render DDL ===");
    let ddl = migrate::render_changes(&changes, &current).map_err(|e| e.to_string())?;
    for stmt in &ddl {
        println!("\n{stmt};");
    }

    println!("\n=== apply ===");
    for stmt in &ddl {
        sqlx::query(stmt).execute(&pool).await?;
    }
    println!("Applied {} statements.", ddl.len());

    let next = dir.join("0002_evolve.json");
    std::fs::write(&next, serde_json::to_string_pretty(&current)?)?;
    println!("Wrote new snapshot → {}", next.display());

    sqlx::query("UPDATE mig_author SET bio = 'inventor of the analytical engine' WHERE id = 1")
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO mig_comment (id, article_id, body) VALUES (1, 1, 'first!')")
        .execute(&pool)
        .await?;
    println!("\nWrote bio + a comment to prove the new schema works.");
    Ok(())
}
