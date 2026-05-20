#![cfg(feature = "postgres")]
//! Live PG test for `WhereExpr::Xor` runtime semantics (issue #27).
//! Verifies the canonical binary rewrite + Django's N-ary odd-parity
//! tally produce the right row counts end-to-end. Skips silently when
//! `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qxor_live_person")]
#[allow(dead_code)]
pub struct Person {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    pub age: i32,
    pub active: bool,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "qxor_live_person" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "qxor_live_person" (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(32) NOT NULL,
            age INTEGER NOT NULL,
            active BOOLEAN NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    // Truth table of (name=alice, active=true) → expected XOR result:
    //   alice / true  → false (both true)
    //   alice / false → true  (one true)
    //   bob   / true  → true  (one true)
    //   bob   / false → false (both false)
    for (name, age, active) in [
        ("alice", 30, true),
        ("alice", 35, false),
        ("bob", 40, true),
        ("bob", 45, false),
    ] {
        let mut p = Person {
            id: Auto::default(),
            name: name.into(),
            age,
            active,
        };
        p.insert(&pool).await.unwrap();
    }
    Some(pool)
}

async fn sorted_names(rows: Vec<Person>) -> Vec<String> {
    let mut names: Vec<String> = rows.into_iter().map(|p| p.name).collect();
    names.sort();
    names
}

/// Binary XOR — `name = alice ^ active = true`. Matches exactly the
/// two rows where one of the predicates is true: (alice, false) and
/// (bob, true).
#[tokio::test]
async fn binary_xor_matches_exactly_one_true() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<Person> = Person::objects()
        .where_(Person::name.eq("alice").xor(Person::active.eq(true)))
        .fetch_on(&pool)
        .await
        .unwrap();

    assert_eq!(sorted_names(rows).await, vec!["alice", "bob"]);
}

/// N-ary XOR — `name=alice ^ active=true ^ age>=40`. Matches rows
/// where an odd number of predicates are true:
///   alice/30/true  : (T, T, F) → 2 trues → false
///   alice/35/false : (T, F, F) → 1 true  → true ✓
///   bob/40/true    : (F, T, T) → 2 trues → false
///   bob/45/false   : (F, F, T) → 1 true  → true ✓
#[tokio::test]
async fn ternary_xor_matches_odd_parity() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<Person> = Person::objects()
        .where_(
            Person::name
                .eq("alice")
                .xor(Person::active.eq(true))
                .xor(Person::age.gte(40_i32)),
        )
        .fetch_on(&pool)
        .await
        .unwrap();

    // alice/35/false (id 2) + bob/45/false (id 4)
    let mut ages: Vec<i32> = rows.iter().map(|p| p.age).collect();
    ages.sort_unstable();
    assert_eq!(ages, vec![35, 45]);
}
