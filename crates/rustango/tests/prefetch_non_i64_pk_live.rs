#![cfg(feature = "postgres")]
//! Closes the P10 gap from `orm-improvements.md`: until v0.26
//! `fetch_with_prefetch` collected parent PKs as `Vec<i64>` and
//! silently dropped non-integer-keyed parents. This test parents over
//! a `String` PK (slug-keyed `Tenant`), prefetches its `Doc` children,
//! and asserts every parent gets its own children — the path that
//! pre-fix would have returned empty `Vec<C>` for every parent.
//!
//! Same shape as `prefetch_related_live.rs` but with String PKs end
//! to end.

#![cfg(feature = "tenancy")]

use rustango::sql::__macro_internals::fetch_with_prefetch;
use rustango::sql::{sqlx, Auto, ForeignKey};

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "_pf_str_tenant", display = "slug")]
pub struct StrTenant {
    #[rustango(primary_key, max_length = 64)]
    pub slug: String,
    #[rustango(max_length = 200)]
    pub display_name: String,
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "_pf_str_doc")]
pub struct StrDoc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub tenant: ForeignKey<StrTenant, String>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "_pf_str_doc" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "_pf_str_tenant" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "_pf_str_tenant" (
            "slug"         VARCHAR(64) PRIMARY KEY,
            "display_name" VARCHAR(200) NOT NULL DEFAULT ''
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "_pf_str_doc" (
            "id"     BIGSERIAL    PRIMARY KEY,
            "tenant" VARCHAR(64)  NOT NULL
                                  REFERENCES "_pf_str_tenant"("slug") ON DELETE CASCADE,
            "title"  VARCHAR(200) NOT NULL DEFAULT ''
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn fetch_with_prefetch_groups_children_under_string_pk_parents() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    // Seed: two parents, three children (2 + 1).
    for slug in ["acme", "globex"] {
        let t = StrTenant {
            slug: slug.into(),
            display_name: slug.to_uppercase(),
        };
        t.insert(&pool).await.unwrap();
    }
    for (slug, title) in [
        ("acme", "Acme Quickstart"),
        ("acme", "Acme Onboarding"),
        ("globex", "Globex Internals"),
    ] {
        let mut d = StrDoc {
            id: Auto::default(),
            tenant: ForeignKey::Unloaded(slug.to_owned()),
            title: title.into(),
        };
        d.insert(&pool).await.unwrap();
    }

    let bundles = fetch_with_prefetch::<StrTenant, StrDoc>(StrTenant::objects(), "tenant", &pool)
        .await
        .unwrap();

    assert_eq!(bundles.len(), 2, "two parents seeded");
    let acme = bundles
        .iter()
        .find(|(t, _)| t.slug == "acme")
        .expect("acme parent present");
    let globex = bundles
        .iter()
        .find(|(t, _)| t.slug == "globex")
        .expect("globex parent present");
    assert_eq!(
        acme.1.len(),
        2,
        "acme should have 2 docs after prefetch — pre-fix returned 0 (parent PK wasn't `i64`)"
    );
    assert_eq!(globex.1.len(), 1, "globex should have 1 doc");

    sqlx::query(r#"DROP TABLE IF EXISTS "_pf_str_doc" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "_pf_str_tenant" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}
