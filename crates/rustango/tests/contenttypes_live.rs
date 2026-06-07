#![cfg(feature = "postgres")]
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

/// Generic-FK demo model (F.4). Two columns
/// `(content_type_id, object_pk)` form a generic pointer at any
/// registered model's row, surfaced via the runtime
/// `GenericForeignKey { content_type_id, object_pk }` value type.
/// The container `#[rustango(generic_fk(...))]` attr emits the
/// metadata the admin renderer reads to show the target as a
/// clickable link.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ct_live_activity",
    generic_fk(
        name = "target",
        ct_column = "target_content_type_id",
        pk_column = "target_object_pk",
    )
)]
#[allow(dead_code)]
pub struct ActivityEntry {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub target_content_type_id: i64,
    pub target_object_pk: i64,
    #[rustango(max_length = 64)]
    pub action: String,
}

/// Soft-FK target (F.3) — `Comment.post_id` is a plain `i64` column
/// pointing at `ct_live_post.id` *without* a declared
/// `Relation::Fk` on the field. Used to exercise [`prefetch_soft`].
#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_live_comment")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Soft FK to ct_live_post.id — no `relation` declared so the
    /// framework treats it as a plain integer column.
    pub post_id: i64,
    #[rustango(max_length = 500)]
    pub body: String,
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
        "ct_live_comment",
        "ct_live_activity",
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
    let inserted = contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("ensure_seeded");
    assert!(
        inserted >= 2,
        "expected at least the two test models seeded, got {inserted}"
    );
    // Re-running should be a no-op.
    let inserted_again = contenttypes::ensure_seeded(&pool.clone().into())
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
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");
    let ct = ContentType::for_model::<Post>(&pool.clone().into())
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
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");
    let ct = ContentType::by_natural_key(&pool.clone().into(), "auth", "user")
        .await
        .expect("by_natural_key")
        .expect("auth.user exists");
    assert_eq!(ct.table, "ct_live_user");
    let pk = ct.id.get().copied().expect("auto pk populated");

    // by_id should return the same row.
    let by_id = ContentType::by_id(&pool.clone().into(), pk)
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
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");
    let rows = ContentType::all_ordered(&pool.clone().into())
        .await
        .expect("all");
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
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");
    let row = ContentType::by_natural_key(&pool.clone().into(), "project", "contenttype")
        .await
        .expect("query");
    assert!(
        row.is_none(),
        "ContentType should not have a self-referential row"
    );
    let alt = ContentType::by_natural_key(&pool.clone().into(), "contenttypes", "contenttype")
        .await
        .expect("query");
    assert!(alt.is_none(), "ContentType should not seed itself");
}

// ============================================================ F.3 — GenericForeignKey + prefetch

#[test]
fn generic_foreign_key_constructs_and_compares() {
    use rustango::contenttypes::GenericForeignKey;
    let g = GenericForeignKey::new(7, 42);
    assert_eq!(g.content_type_id, 7);
    assert_eq!(g.object_pk, 42);
    assert_eq!(g, GenericForeignKey::new(7, 42));
    assert_ne!(g, GenericForeignKey::new(7, 43));
}

#[tokio::test]
async fn for_target_resolves_via_content_type() {
    use rustango::contenttypes::GenericForeignKey;
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");
    let g = GenericForeignKey::for_target::<Post>(&pool.clone().into(), 99)
        .await
        .expect("for_target");
    let post_ct = ContentType::for_model::<Post>(&pool.clone().into())
        .await
        .expect("for_model")
        .expect("Post CT exists");
    assert_eq!(g.content_type_id, post_ct.id.get().copied().unwrap());
    assert_eq!(g.object_pk, 99);
}

#[tokio::test]
async fn prefetch_soft_groups_children_by_fk_value() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };

    // Seed two posts + 3 comments — two on post 1, one on post 2.
    let mut p1 = Post {
        id: Auto::Unset,
        title: "first".into(),
    };
    p1.insert(&pool).await.expect("insert p1");
    let mut p2 = Post {
        id: Auto::Unset,
        title: "second".into(),
    };
    p2.insert(&pool).await.expect("insert p2");
    let p1_id = p1.id.get().copied().unwrap();
    let p2_id = p2.id.get().copied().unwrap();

    for (post_id, body) in [
        (p1_id, "comment-A"),
        (p1_id, "comment-B"),
        (p2_id, "comment-C"),
    ] {
        let mut c = Comment {
            id: Auto::Unset,
            post_id,
            body: body.into(),
        };
        c.insert(&pool).await.expect("insert comment");
    }

    let parent_pks = vec![p1_id, p2_id];
    let by_post = contenttypes::prefetch_soft::<Comment, _>(
        &pool.clone().into(),
        &parent_pks,
        "post_id",
        |c| c.post_id,
    )
    .await
    .expect("prefetch_soft");

    let p1_kids = by_post.get(&p1_id).expect("p1 has kids");
    assert_eq!(p1_kids.len(), 2);
    let p2_kids = by_post.get(&p2_id).expect("p2 has kids");
    assert_eq!(p2_kids.len(), 1);
    assert_eq!(p2_kids[0].body, "comment-C");
}

#[tokio::test]
async fn prefetch_soft_short_circuits_on_empty_parent_list() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    let by_post =
        contenttypes::prefetch_soft::<Comment, _>(&pool.clone().into(), &[], "post_id", |c| {
            c.post_id
        })
        .await
        .expect("prefetch_soft");
    assert!(by_post.is_empty());
}

#[tokio::test]
async fn prefetch_generic_hydrates_typed_targets() {
    use rustango::contenttypes::GenericForeignKey;
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");

    // Seed 2 posts + 1 user. Build a list of generic FKs pointing at
    // both kinds. prefetch_generic::<Post>(...) should hydrate only
    // the post-typed pairs and ignore the user-typed pair.
    let mut p1 = Post {
        id: Auto::Unset,
        title: "alpha".into(),
    };
    p1.insert(&pool).await.expect("insert p1");
    let mut p2 = Post {
        id: Auto::Unset,
        title: "beta".into(),
    };
    p2.insert(&pool).await.expect("insert p2");
    let mut u = User {
        id: Auto::Unset,
        username: "carol".into(),
    };
    u.insert(&pool).await.expect("insert u");

    let p1_pk = p1.id.get().copied().unwrap();
    let p2_pk = p2.id.get().copied().unwrap();
    let u_pk = u.id.get().copied().unwrap();

    let g_p1 = GenericForeignKey::for_target::<Post>(&pool.clone().into(), p1_pk)
        .await
        .expect("g_p1");
    let g_p2 = GenericForeignKey::for_target::<Post>(&pool.clone().into(), p2_pk)
        .await
        .expect("g_p2");
    let g_u = GenericForeignKey::for_target::<User>(&pool.clone().into(), u_pk)
        .await
        .expect("g_u");
    let pairs = vec![
        (g_p1.content_type_id, g_p1.object_pk),
        (g_p2.content_type_id, g_p2.object_pk),
        (g_u.content_type_id, g_u.object_pk),
    ];

    let posts = contenttypes::prefetch_generic::<Post>(&pool.clone().into(), &pairs)
        .await
        .expect("prefetch_generic Post");
    assert_eq!(
        posts.len(),
        2,
        "should hydrate both posts and ignore the user-typed pair"
    );
    assert_eq!(
        posts
            .get(&(g_p1.content_type_id, p1_pk))
            .map(|p| p.title.as_str()),
        Some("alpha")
    );
    assert_eq!(
        posts
            .get(&(g_p2.content_type_id, p2_pk))
            .map(|p| p.title.as_str()),
        Some("beta")
    );
    assert!(
        posts.get(&(g_u.content_type_id, u_pk)).is_none(),
        "user-typed pair must not appear in the Post-targeted result"
    );
}

#[tokio::test]
async fn prefetch_generic_short_circuits_on_empty_pairs() {
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");
    let out = contenttypes::prefetch_generic::<Post>(&pool.clone().into(), &[])
        .await
        .expect("prefetch_generic empty");
    assert!(out.is_empty());
}

// ============================================================ F.4 — generic_relations + admin renderer

#[test]
fn generic_fk_relation_is_emitted_on_model_schema() {
    use rustango::core::Model as _;
    let s = ActivityEntry::SCHEMA;
    assert_eq!(
        s.generic_relations.len(),
        1,
        "ActivityEntry should declare one generic_fk"
    );
    let rel = &s.generic_relations[0];
    assert_eq!(rel.name, "target");
    assert_eq!(rel.ct_column, "target_content_type_id");
    assert_eq!(rel.pk_column, "target_object_pk");
}

#[tokio::test]
async fn render_generic_fk_link_resolves_via_content_type() {
    use rustango::contenttypes::{render_generic_fk_link, GenericForeignKey};
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");
    let mut p = Post {
        id: Auto::Unset,
        title: "rendered".into(),
    };
    p.insert(&pool).await.expect("insert post");
    let pk = p.id.get().copied().unwrap();

    let gfk = GenericForeignKey::for_target::<Post>(&pool.clone().into(), pk)
        .await
        .expect("for_target");
    let html = render_generic_fk_link(&pool.clone().into(), gfk)
        .await
        .expect("render_generic_fk_link");

    // Link to the target table's admin route, with app_label.model_name
    // as the visible label.
    assert!(html.contains(r#"href="/ct_live_post/"#));
    assert!(html.contains("blog.post"));
    assert!(html.contains(&format!("#{pk}")));
}

#[tokio::test]
async fn render_generic_fk_link_falls_back_on_unknown_ct_id() {
    use rustango::contenttypes::{render_generic_fk_link, GenericForeignKey};
    let _g = ct_lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    contenttypes::ensure_seeded(&pool.clone().into())
        .await
        .expect("seed");
    let gfk = GenericForeignKey::new(/* unknown */ 99_999_999, 42);
    let html = render_generic_fk_link(&pool.clone().into(), gfk)
        .await
        .expect("render_generic_fk_link");
    // Fallback shape — raw pair, no anchor link, italics for visibility.
    assert!(html.contains("ct=99999999"));
    assert!(html.contains("pk=42"));
    assert!(!html.contains("href="));
}
