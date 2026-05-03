//! Live integration test for the v0.15 sub-slice F.1 ContentType
//! framework — bootstrap the table, seed it from inventory, look up
//! by `for_model::<T>` / `by_natural_key` / `by_id` / `all`.
//!
//! Activated when `DATABASE_URL` is set (the same env var the rest of
//! the live tests use); skips silently otherwise.
//!
//! Schema is owned: every test starts by `DROP TABLE IF EXISTS
//! rustango_content_types CASCADE` then re-applies the ContentType
//! DDL via `apply_all` so re-runs are idempotent. Tests share the
//! one table so they're serialized via a tokio mutex.

use std::sync::OnceLock;

use rustango::contenttypes::{self, ContentType};
use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_live_post")]
#[rustango(app = "blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_live_user")]
#[rustango(app = "auth")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub username: String,
}

/// Realistic composite-PK target — a junction-style table whose
/// primary key is the natural `(left_id, right_id)` pair. Single-FK
/// paths can't reference a row in this table; you need both columns
/// matched, which is what `fk_composite` is for.
///
/// PG-side, this maps to:
/// `CREATE TABLE ct_live_pair (left_id BIGINT NOT NULL, right_id
/// BIGINT NOT NULL, PRIMARY KEY (left_id, right_id))` — the
/// composite PK constraint isn't yet auto-emitted by the rustango
/// DDL writer (single-column PK is the v0.x default), so the live
/// test seeds the supporting unique index by hand. The model exists
/// purely so [`AuditTarget`] has a target with two distinct columns
/// for its `fk_composite`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_live_pair")]
#[allow(dead_code)]
pub struct Pair {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub left_id: i64,
    pub right_id: i64,
}

/// Composite-FK demo model (sub-slice F.2). Two columns
/// `(left_ref, right_ref)` form a logical FK to `(left_id,
/// right_id)` on [`Pair`]. Exercises the full macro / schema / DDL
/// composite-FK path end-to-end.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ct_live_audit",
    fk_composite(
        name = "pair_target",
        to = "ct_live_pair",
        from = ("left_ref", "right_ref"),
        on = ("left_id", "right_id"),
    ),
)]
#[allow(dead_code)]
pub struct AuditTarget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub left_ref: i64,
    pub right_ref: i64,
}

fn ct_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL failed");
    // Reset the tables involved in these tests so re-runs are clean.
    for tbl in [
        "rustango_content_types",
        "ct_live_post",
        "ct_live_user",
        "ct_live_audit",
        "ct_live_pair",
    ] {
        let drop_sql = format!(r#"DROP TABLE IF EXISTS "{tbl}" CASCADE"#);
        let _ = sqlx::query(&drop_sql).execute(&pool).await;
    }
    // Phase 1 — CREATE TABLE for every registered model. We do this
    // manually rather than via `apply_all` because apply_all also
    // emits FK constraints in the same call, and our composite FK
    // needs a UNIQUE INDEX on `ct_live_pair (left_id, right_id)` to
    // exist *before* the FK can be added (PG rule).
    use rustango::core::Model as _;
    use rustango::migrate::ddl::{create_constraints_sql, create_table_sql};
    for entry in rustango::core::inventory::iter::<rustango::core::ModelEntry> {
        let _ = sqlx::query(&create_table_sql(entry.schema))
            .execute(&pool)
            .await;
    }
    // Phase 2 — supporting unique index for the composite FK target.
    let _ = sqlx::query(
        r#"CREATE UNIQUE INDEX IF NOT EXISTS "ct_live_pair_left_right_uq"
           ON "ct_live_pair" ("left_id", "right_id")"#,
    )
    .execute(&pool)
    .await;
    // Phase 3 — emit ALTER TABLE FK constraints (single-col + composite).
    // Best-effort; some FKs may already exist from earlier runs.
    for entry in rustango::core::inventory::iter::<rustango::core::ModelEntry> {
        for stmt in create_constraints_sql(entry.schema) {
            let _ = sqlx::query(&stmt).execute(&pool).await;
        }
    }
    let _ = AuditTarget::SCHEMA; // silence "unused import" if Model's only use is here
    Some(pool)
}

#[tokio::test]
async fn ensure_seeded_inserts_a_row_per_model() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    let inserted = contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded");
    assert!(
        inserted >= 2,
        "expected at least the two test models seeded, got {inserted}"
    );
    // Re-running should be a no-op.
    let inserted_again = contenttypes::ensure_seeded(&pool)
        .await
        .expect("ensure_seeded idempotent");
    assert_eq!(
        inserted_again, 0,
        "re-running ensure_seeded should insert nothing"
    );
}

#[tokio::test]
async fn for_model_resolves_to_correct_row() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool).await.expect("seed");
    let ct = ContentType::for_model::<Post>(&pool)
        .await
        .expect("for_model")
        .expect("Post ContentType row exists");
    assert_eq!(ct.app_label, "blog");
    assert_eq!(ct.model_name, "post");
    assert_eq!(ct.table, "ct_live_post");
    assert!(ct.id.get().is_some(), "id should be populated by RETURNING");
}

#[tokio::test]
async fn by_natural_key_round_trips() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool).await.expect("seed");
    let ct = ContentType::by_natural_key(&pool, "auth", "user")
        .await
        .expect("by_natural_key")
        .expect("auth.user exists");
    assert_eq!(ct.table, "ct_live_user");
    let pk = ct.id.get().copied().expect("auto pk populated");

    // by_id should return the same row.
    let by_id = ContentType::by_id(&pool, pk)
        .await
        .expect("by_id")
        .expect("pk exists");
    assert_eq!(by_id.app_label, "auth");
    assert_eq!(by_id.model_name, "user");
}

#[tokio::test]
async fn all_returns_seeded_rows_ordered() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool).await.expect("seed");
    let rows = ContentType::all(&pool).await.expect("all");
    assert!(rows.len() >= 2, "at least two seeded models");
    // Confirm sort order: app_label asc, model_name asc.
    let mut last: Option<(String, String)> = None;
    for ct in &rows {
        if let Some(prev) = &last {
            assert!(
                (&prev.0, &prev.1) <= (&ct.app_label, &ct.model_name),
                "rows out of order at {} / {}",
                ct.app_label,
                ct.model_name,
            );
        }
        last = Some((ct.app_label.clone(), ct.model_name.clone()));
    }
}

#[test]
fn composite_fk_relation_is_emitted_on_model_schema() {
    // Pure macro/schema test — doesn't need a live DB. Wired here
    // alongside the live tests so the schema and the F.2 DDL stay
    // exercised together.
    use rustango::core::Model as _;
    let s = AuditTarget::SCHEMA;
    assert_eq!(
        s.composite_relations.len(),
        1,
        "AuditTarget should declare one composite FK"
    );
    let rel = &s.composite_relations[0];
    assert_eq!(rel.name, "pair_target");
    assert_eq!(rel.to, "ct_live_pair");
    assert_eq!(rel.from, &["left_ref", "right_ref"]);
    assert_eq!(rel.on, &["left_id", "right_id"]);
}

#[test]
fn composite_fk_emits_alter_table_constraint_in_ddl() {
    // Confirm the DDL writer renders the composite FK as
    // `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY (a, b) REFERENCES t (x, y)`.
    use rustango::core::Model as _;
    use rustango::migrate::ddl::create_constraints_sql;
    let stmts = create_constraints_sql(AuditTarget::SCHEMA);
    let composite = stmts
        .iter()
        .find(|s| s.contains("pair_target_fkey"))
        .expect("composite FK ALTER TABLE statement should be emitted");
    assert!(composite.contains(r#"FOREIGN KEY ("left_ref", "right_ref")"#));
    assert!(composite.contains(r#"REFERENCES "ct_live_pair" ("left_id", "right_id")"#));
}

#[tokio::test]
async fn ensure_seeded_skips_content_type_table_itself() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool).await.expect("seed");
    let row = ContentType::by_natural_key(&pool, "project", "contenttype")
        .await
        .expect("query");
    assert!(
        row.is_none(),
        "ContentType should not have a self-referential row"
    );
    let alt = ContentType::by_natural_key(&pool, "contenttypes", "contenttype")
        .await
        .expect("query");
    assert!(alt.is_none(), "ContentType should not seed itself");
}
