//! v0.17.2 — `#[rustango(fk = "self")]` self-referential FK.
//!
//! Page tree shape:
//!
//! ```text
//! root
//! ├── child_a
//! └── child_b
//!     └── grandchild
//! ```
//!
//! Verifies:
//! 1. `Page::SCHEMA` exposes the parent_id column with a Relation::Fk
//!    pointing at the model's own table (no chicken-and-egg in const
//!    eval).
//! 2. CREATE TABLE + ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY emits
//!    cleanly against the model's own table.
//! 3. Inserts cascade through the recursion (root.parent NULL, children
//!    point at root, grandchild at child_b).
//! 4. Filtering by parent_id resolves the right children.
//!
//! Reads DATABASE_URL; skips silently if unset.

use rustango::core::{Model as _, Op};
use rustango::sql::{sqlx, Auto, Fetcher};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_self_fk_page")]
pub struct Page {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(fk = "self", on = "id")]
    pub parent_id: Option<i64>,
}

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

#[test]
fn schema_self_fk_resolves_to_own_table() {
    let parent = Page::SCHEMA
        .fields
        .iter()
        .find(|f| f.column == "parent_id")
        .expect("parent_id field present in schema");
    let rel = parent.relation.expect("parent_id has a relation");
    match rel {
        rustango::core::Relation::Fk { to, on } => {
            assert_eq!(to, Page::SCHEMA.table, "self-FK target == own table");
            assert_eq!(on, "id");
        }
        other => panic!("expected Relation::Fk, got {other:?}"),
    }
}

#[tokio::test]
async fn live_self_fk_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let Some(url) = database_url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;

    sqlx::query("DROP TABLE IF EXISTS rustango_self_fk_page CASCADE")
        .execute(&pool)
        .await?;
    sqlx::query(
        r#"CREATE TABLE rustango_self_fk_page (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(64) NOT NULL,
            parent_id BIGINT NULL
        )"#,
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"ALTER TABLE rustango_self_fk_page
           ADD CONSTRAINT rustango_self_fk_page_parent_id_fkey
           FOREIGN KEY (parent_id) REFERENCES rustango_self_fk_page (id)
           ON DELETE CASCADE"#,
    )
    .execute(&pool)
    .await?;

    let mut root = Page {
        id: Auto::Unset,
        title: "root".into(),
        parent_id: None,
    };
    root.save(&pool).await?;
    let root_id = match root.id {
        Auto::Set(v) => v,
        Auto::Unset => panic!("root id unset"),
    };

    let mut child_a = Page {
        id: Auto::Unset,
        title: "child_a".into(),
        parent_id: Some(root_id),
    };
    child_a.save(&pool).await?;
    let mut child_b = Page {
        id: Auto::Unset,
        title: "child_b".into(),
        parent_id: Some(root_id),
    };
    child_b.save(&pool).await?;
    let child_b_id = match child_b.id {
        Auto::Set(v) => v,
        Auto::Unset => panic!("child_b id unset"),
    };

    let mut grand = Page {
        id: Auto::Unset,
        title: "grand".into(),
        parent_id: Some(child_b_id),
    };
    grand.save(&pool).await?;

    let mut root_children: Vec<Page> = Page::objects()
        .filter("parent_id", Op::Eq, root_id)
        .fetch(&pool)
        .await?;
    root_children.sort_by(|a, b| a.title.cmp(&b.title));
    assert_eq!(root_children.len(), 2);
    assert_eq!(root_children[0].title, "child_a");
    assert_eq!(root_children[1].title, "child_b");

    let grandkids: Vec<Page> = Page::objects()
        .filter("parent_id", Op::Eq, child_b_id)
        .fetch(&pool)
        .await?;
    assert_eq!(grandkids.len(), 1);
    assert_eq!(grandkids[0].title, "grand");

    sqlx::query("DROP TABLE IF EXISTS rustango_self_fk_page CASCADE")
        .execute(&pool)
        .await?;
    Ok(())
}
