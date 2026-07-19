#![cfg(feature = "postgres")]
//! Proof: the `Org` Model derive already works against a SQLite pool
//! via the `rustango::sql::Pool` enum dispatch — the foundation for
//! v0.34's pure-SQLite stack.
//!
//! What this test demonstrates and what's still missing:
//!
//! ✅ `Org::insert_pool(&Pool::Sqlite(...))` round-trips through
//!    SQLite's `INSERT ... RETURNING id` (SQLite ≥ 3.35).
//! ✅ `Org::objects().fetch(&Pool::Sqlite(...))` reads rows back.
//! ✅ The new `Org.backend_kind` column persists via SQLite TEXT.
//!
//! ❌ `TenantPools::registry()` still returns `&PgPool` — apps wanting
//!    a SQLite registry can't go through the framework's tenancy
//!    chain yet (resolver / middleware / migrate_registry all bind PG).
//! ❌ Schema migrations against SQLite still need to be hand-written
//!    (`make_migrations` against a SQLite pool isn't supported yet —
//!    v0.34 Phase B work).
//!
//! Together, this means: **tests can use SQLite for Org storage
//! today** by going through the rustango Pool enum directly,
//! bypassing the still-PG-bound `TenantPools` builder.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::tenancy::Org;

/// Bootstrap a sqlite pool with `rustango_orgs` built from `Org::SCHEMA`
/// via the same DDL emitter the migration runner uses — no hand-written
/// DDL to drift when the model gains a column.
async fn sqlite_registry() -> Pool {
    let sqlx_pool: sqlx::SqlitePool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    let pool: Pool = sqlx_pool.into();
    rustango::testkit::create_tables_for::<Org>(&pool)
        .await
        .expect("bootstrap rustango_orgs");
    pool
}

fn fake_sqlite_org(slug: &str) -> Org {
    Org {
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        backend_kind: "sqlite".into(),
        database_url: Some(format!(
            "sqlite:file:tenant_{slug}?mode=memory&cache=shared"
        )),
        ..rustango::testkit::org()
    }
}

#[tokio::test]
async fn org_insert_and_fetch_against_sqlite_registry() {
    let pool = sqlite_registry().await;

    // INSERT through Model derive — emits the SQLite-compatible path
    // (no PG-specific syntax leaks into the query).
    let mut acme = fake_sqlite_org("acme");
    acme.insert_pool(&pool).await.expect("insert acme");
    assert!(matches!(acme.id, Auto::Set(_)));

    let mut beta = fake_sqlite_org("beta");
    beta.insert_pool(&pool).await.expect("insert beta");

    // FETCH through the QuerySet API.
    let rows: Vec<Org> = Org::objects()
        .order_by(&[("slug", false)])
        .fetch(&pool)
        .await
        .expect("fetch");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].slug, "acme");
    assert_eq!(rows[1].slug, "beta");

    // Filtered fetch — confirms WHERE generation lands sqlite syntax.
    let mut just_acme: Vec<Org> = Org::objects()
        .where_(Org::slug.eq("acme".to_owned()))
        .fetch(&pool)
        .await
        .expect("filtered fetch");
    assert_eq!(just_acme.len(), 1);
    let acme_row = just_acme.pop().unwrap();
    assert_eq!(acme_row.backend_kind, "sqlite");
    assert_eq!(acme_row.storage_mode, "database");
}

#[tokio::test]
async fn org_save_against_sqlite_registry() {
    let pool = sqlite_registry().await;

    let mut org = fake_sqlite_org("acme");
    org.insert_pool(&pool).await.expect("insert");
    let original_id = *org.id.get().expect("id set");

    // Mutate + save — this exercises the UPDATE path on SQLite.
    org.display_name = "Acme Corp Ltd".into();
    org.save_pool(&pool).await.expect("save");

    let after: Vec<Org> = Org::objects()
        .where_(Org::slug.eq("acme".to_owned()))
        .fetch(&pool)
        .await
        .expect("refetch");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].display_name, "Acme Corp Ltd");
    assert_eq!(*after[0].id.get().expect("id"), original_id);
}
