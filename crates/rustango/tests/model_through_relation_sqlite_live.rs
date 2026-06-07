#![cfg(feature = "sqlite")]
//! Live SQLite tests for the Eloquent `hasManyThrough` /
//! `hasOneThrough` accessor — closes issue
//! [#817](https://github.com/ujeenet/rustango/issues/817).
//!
//! Canonical example from the Eloquent docs (`Country hasMany Posts
//! through Users`):
//!
//! ```ignore
//! Country ──< User ──< Post
//! ```
//!
//! The `Country` model declares the through relation:
//!
//! ```ignore
//! #[rustango(through(
//!     name = "posts",
//!     far = "Post",
//!     far_fk_column = "author_id",
//!     intermediate = "User",
//!     intermediate_fk_column = "country_id",
//! ))]
//! ```
//!
//! and gets a `country.posts_through()` accessor returning a
//! `QuerySet<Post>` filtered via the correlated subquery
//! `WHERE author_id IN (SELECT id FROM tr_user WHERE country_id = ?)`.
//! The accessor is chainable — additional filters, ordering, and
//! limits compose normally on top of the through-clause.

use rustango::sql::{sqlx, Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "tr_country",
    through(
        name = "posts",
        far = "Post",
        far_fk_column = "author_id",
        intermediate = "User",
        intermediate_fk_column = "country_id",
    )
)]
#[allow(dead_code)]
pub struct Country {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tr_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    pub country_id: ForeignKey<Country, i64>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tr_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub title: String,
    pub author_id: ForeignKey<User, i64>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE tr_country (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE tr_user (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL,
            country_id INTEGER NOT NULL REFERENCES tr_country(id)
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE tr_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            author_id INTEGER NOT NULL REFERENCES tr_user(id)
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

/// Seed two countries with their own user-tree. Returns the two
/// country IDs.
async fn seed(pool: &Pool) -> (i64, i64) {
    let mut a = Country {
        id: Auto::default(),
        name: "Atlantis".into(),
    };
    a.save_pool(pool).await.unwrap();
    let a_id = a.id.get().copied().unwrap();
    let mut b = Country {
        id: Auto::default(),
        name: "Brookland".into(),
    };
    b.save_pool(pool).await.unwrap();
    let b_id = b.id.get().copied().unwrap();

    for (country_id, names) in [
        (a_id, ["alice", "andrew"].as_slice()),
        (b_id, ["bob", "betty", "barry"].as_slice()),
    ] {
        for n in names.iter() {
            let mut u = User {
                id: Auto::default(),
                name: (*n).into(),
                country_id: ForeignKey::unloaded(country_id),
            };
            u.save_pool(pool).await.unwrap();
            let user_id = u.id.get().copied().unwrap();
            for i in 0..3 {
                let mut p = Post {
                    id: Auto::default(),
                    title: format!("{n}-post-{i}"),
                    author_id: ForeignKey::unloaded(user_id),
                };
                p.save_pool(pool).await.unwrap();
            }
        }
    }
    (a_id, b_id)
}

#[tokio::test]
async fn posts_through_returns_only_far_model_rows_for_this_country() {
    let pool = make_pool().await;
    let (a_id, b_id) = seed(&pool).await;

    let a = Country::find(a_id, &pool).await.unwrap().unwrap();
    let posts = a.posts_through().fetch_pool(&pool).await.unwrap();
    // Country A has 2 users × 3 posts each = 6 far-model rows.
    assert_eq!(posts.len(), 6);
    for p in &posts {
        assert!(
            p.title.starts_with("alice-") || p.title.starts_with("andrew-"),
            "unexpected far-side title: {}",
            p.title
        );
    }

    let b = Country::find(b_id, &pool).await.unwrap().unwrap();
    let posts_b = b.posts_through().fetch_pool(&pool).await.unwrap();
    // Country B has 3 users × 3 posts each = 9.
    assert_eq!(posts_b.len(), 9);
    for p in &posts_b {
        assert!(
            p.title.starts_with("bob-")
                || p.title.starts_with("betty-")
                || p.title.starts_with("barry-"),
            "unexpected far-side title: {}",
            p.title
        );
    }
}

#[tokio::test]
async fn through_accessor_is_chainable_with_filter_and_limit() {
    let pool = make_pool().await;
    let (a_id, _) = seed(&pool).await;
    let a = Country::find(a_id, &pool).await.unwrap().unwrap();

    // Compose .filter() on top of the through accessor — narrows
    // the 6 country-A posts down to the ones whose title begins
    // with "alice-".
    let alice_posts = a
        .posts_through()
        .filter("title__startswith", "alice-")
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(alice_posts.len(), 3);
    for p in &alice_posts {
        assert!(p.title.starts_with("alice-"));
    }

    // Compose .order_by() + .limit() — proves the InSubquery WHERE
    // sits alongside an outer ORDER BY + LIMIT correctly.
    let first_two = a
        .posts_through()
        .order_by(&[("id", false)])
        .limit(2)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(first_two.len(), 2);
}

#[tokio::test]
async fn through_accessor_returns_empty_for_country_with_no_users() {
    let pool = make_pool().await;
    // Insert a country with no users → 0 far-side rows.
    let mut empty = Country {
        id: Auto::default(),
        name: "Empty".into(),
    };
    empty.save_pool(&pool).await.unwrap();
    let posts = empty.posts_through().fetch_pool(&pool).await.unwrap();
    assert!(posts.is_empty());
}

#[tokio::test]
async fn posts_through_count_returns_far_side_count() {
    // Eloquent `$country->posts->count()` analog for the
    // through-relation — `posts_through_count(&pool)` returns the
    // number of far rows reachable from this country via users.
    let pool = make_pool().await;
    let (a_id, b_id) = seed(&pool).await;

    let a = Country::find(a_id, &pool).await.unwrap().unwrap();
    let b = Country::find(b_id, &pool).await.unwrap().unwrap();

    // Country A: 2 users × 3 posts each = 6. Country B: 3 × 3 = 9.
    assert_eq!(a.posts_through_count(&pool).await.unwrap(), 6);
    assert_eq!(b.posts_through_count(&pool).await.unwrap(), 9);

    let mut empty = Country {
        id: Auto::default(),
        name: "Empty".into(),
    };
    empty.save_pool(&pool).await.unwrap();
    assert_eq!(empty.posts_through_count(&pool).await.unwrap(), 0);
}
