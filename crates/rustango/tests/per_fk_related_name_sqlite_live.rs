#![cfg(feature = "sqlite")]
//! Live SQLite test for the per-FK `#[rustango(related_name = "...")]`
//! attribute — follow-up to #816.
//!
//! Confirms that the field-level override beats both the container-
//! level `default_related_name` and the default `<child_snake>_set`
//! fallback when picking the reverse-accessor method name on the
//! parent type. Also confirms a model with TWO FKs to the same parent
//! gets two distinct reverse accessors (default would collide).

use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "prn_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

// Two FKs to User — without `related_name` they would BOTH default
// to `comment_set_pool`, an outright compile-time collision. The
// per-FK override fixes it.
#[derive(Model, Debug, Clone)]
#[rustango(table = "prn_comment")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub body: String,
    #[rustango(related_name = "authored_comments")]
    pub author: ForeignKey<User>,
    #[rustango(related_name = "reviewed_comments")]
    pub reviewer: ForeignKey<User>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE prn_user (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE prn_comment (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            body      TEXT NOT NULL,
            author    INTEGER NOT NULL REFERENCES prn_user(id),
            reviewer  INTEGER NOT NULL REFERENCES prn_user(id)
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

#[tokio::test]
async fn per_fk_related_name_disambiguates_two_fks_to_same_parent() {
    let pool = make_pool().await;

    let mut ada = User {
        id: Auto::default(),
        name: "Ada".into(),
    };
    ada.save_pool(&pool).await.unwrap();
    let mut grace = User {
        id: Auto::default(),
        name: "Grace".into(),
    };
    grace.save_pool(&pool).await.unwrap();

    let ada_pk = ada.id.get().copied().unwrap();
    let grace_pk = grace.id.get().copied().unwrap();

    // Ada authored 2; Grace authored 1. All 3 reviewed by Ada.
    for (body, author_pk, reviewer_pk) in [
        ("A1", ada_pk, ada_pk),
        ("A2", ada_pk, ada_pk),
        ("G1", grace_pk, ada_pk),
    ] {
        let mut c = Comment {
            id: Auto::default(),
            body: body.into(),
            author: ForeignKey::unloaded(author_pk),
            reviewer: ForeignKey::unloaded(reviewer_pk),
        };
        c.save_pool(&pool).await.unwrap();
    }

    let ada_db = QuerySet::<User>::default()
        .filter("id", ada_pk)
        .fetch(&pool)
        .await
        .unwrap()
        .pop()
        .unwrap();

    // Field-level `related_name = "authored_comments"` →
    // `authored_comments_pool` on the parent.
    let authored = ada_db.authored_comments_pool(&pool).await.unwrap();
    assert_eq!(authored.len(), 2);
    let bodies: Vec<&str> = authored.iter().map(|c| c.body.as_str()).collect();
    assert!(bodies.contains(&"A1"));
    assert!(bodies.contains(&"A2"));

    // Field-level `related_name = "reviewed_comments"` →
    // `reviewed_comments_pool` on the parent. Ada reviewed all 3.
    let reviewed = ada_db.reviewed_comments_pool(&pool).await.unwrap();
    assert_eq!(reviewed.len(), 3);

    let grace_db = QuerySet::<User>::default()
        .filter("id", grace_pk)
        .fetch(&pool)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let authored = grace_db.authored_comments_pool(&pool).await.unwrap();
    assert_eq!(authored.len(), 1);
    assert_eq!(authored[0].body, "G1");
    // Grace reviewed zero comments.
    let reviewed = grace_db.reviewed_comments_pool(&pool).await.unwrap();
    assert_eq!(reviewed.len(), 0);
}
