//! Live test for `WhereExpr` OR / nested filter semantics
//! (v0.7 slice 4).
//!
//! Models: `Person { id, name, age, active }`. Verifies:
//!
//! * `User::name.eq("alice").or(User::name.eq("bob"))` materializes
//!   as `WHERE ("name" = $1 OR "name" = $2)` and selects the right rows.
//! * `(A.or(B)).and(C)` nests correctly: `WHERE ("name" = $1 OR
//!   "name" = $2) AND "active" = $3`.
//! * Multiple `.where_(…)` calls AND-join their arguments at the
//!   top level (existing v0.6 semantics preserved).
//! * Empty `WhereExpr::Or(vec![])` is rejected by the writer with
//!   `SqlError::EmptyOrBranch` so a footgun build doesn't silently
//!   match nothing.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently.

use std::sync::OnceLock;

use rustango::core::{Column as _, Model as _, WhereExpr};
use rustango::sql::{sqlx, Fetcher, SqlError};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_where_person")]
pub struct Person {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64)]
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
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL");
    sqlx::query("DROP TABLE IF EXISTS rustango_where_person")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE rustango_where_person (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(64) NOT NULL,
            age INTEGER NOT NULL,
            active BOOLEAN NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    for (name, age, active) in [
        ("alice", 30, true),
        ("bob", 40, true),
        ("carol", 25, false),
        ("dave", 50, false),
    ] {
        let mut p = Person {
            id: rustango::sql::Auto::default(),
            name: name.into(),
            age,
            active,
        };
        p.insert(&pool).await.unwrap();
    }
    Some(pool)
}

async fn names_of(rows: Vec<Person>) -> Vec<String> {
    let mut names: Vec<String> = rows.into_iter().map(|p| p.name).collect();
    names.sort();
    names
}

#[tokio::test]
async fn or_two_eq_predicates_matches_either() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<Person> = Person::objects()
        .where_(Person::name.eq("alice").or(Person::name.eq("bob")))
        .fetch(&pool)
        .await
        .unwrap();

    assert_eq!(names_of(rows).await, vec!["alice", "bob"]);
}

#[tokio::test]
async fn and_after_or_groups_correctly() {
    // Want: (name = alice OR name = bob) AND active = true
    // alice + bob are both active so both match.
    // Now add carol (inactive); she shouldn't appear.
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<Person> = Person::objects()
        .where_(
            Person::name
                .eq("alice")
                .or(Person::name.eq("bob"))
                .or(Person::name.eq("carol")),
        )
        .where_(Person::active.eq(true))
        .fetch(&pool)
        .await
        .unwrap();

    assert_eq!(names_of(rows).await, vec!["alice", "bob"]);
}

#[tokio::test]
async fn nested_or_and_complex_expression() {
    // (age >= 40 AND active = false) OR name = alice
    //   matches: alice (always), dave (50, inactive)
    //   does not match: bob (40, active), carol (25, inactive)
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<Person> = Person::objects()
        .where_(
            Person::age
                .gte(40_i32)
                .and(Person::active.eq(false))
                .or(Person::name.eq("alice")),
        )
        .fetch(&pool)
        .await
        .unwrap();

    assert_eq!(names_of(rows).await, vec!["alice", "dave"]);
}

#[tokio::test]
async fn multiple_where_calls_still_and_at_top_level() {
    // Successive `.where_()` calls AND at the top level.
    // (name = alice OR name = bob) AND age >= 35 → bob only.
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<Person> = Person::objects()
        .where_(Person::name.eq("alice").or(Person::name.eq("bob")))
        .where_(Person::age.gte(35_i32))
        .fetch(&pool)
        .await
        .unwrap();

    assert_eq!(names_of(rows).await, vec!["bob"]);
}

#[tokio::test]
async fn empty_or_branch_returns_named_writer_error() {
    // Built-by-hand WhereExpr with an empty Or — the writer rejects
    // it. We don't even need a pool for this; the failure surfaces
    // before any SQL is executed. Keep the pool guard to match the
    // other tests.
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    use rustango::core::{Filter, Op, SelectQuery, SqlValue};
    use rustango::sql::Dialect as _;
    use rustango::sql::Postgres;

    let q = SelectQuery {
        model: Person::SCHEMA,
        where_clause: WhereExpr::Or(vec![]),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
    };
    let err = Postgres.compile_select(&q).unwrap_err();
    assert!(matches!(err, SqlError::EmptyOrBranch));

    // Single-element Or is fine — proves we only reject *empty* Or.
    let q2 = SelectQuery {
        model: Person::SCHEMA,
        where_clause: WhereExpr::Or(vec![WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        })]),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
    };
    rustango::sql::select_rows(&pool, &q2).await.unwrap();
}
