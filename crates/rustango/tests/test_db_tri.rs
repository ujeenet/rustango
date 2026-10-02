//! `test_db` truncate helpers, `Fixture` loading and `create_tables`
//! re-runs on every backend (#1959).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::fixtures::{Fixture, FixtureError};
use rustango::sql::{Auto, CounterPool as _, ExecError, FetcherPool as _, ForeignKey, Pool};
use rustango::test_db::{truncate_tables, with_rollback, with_truncate_after};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "tdb1959_parent", app = "tdb1959")]
pub struct Parent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tdb1959_child", app = "tdb1959")]
pub struct Child {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub parent: ForeignKey<Parent, i64>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "tdb1959_stamp", app = "tdb1959")]
pub struct Stamp {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub at: chrono::DateTime<chrono::Utc>,
    pub token: uuid::Uuid,
    #[rustango(max_length = 32)]
    pub label: String,
}

const TABLES: &[&str] = &["tdb1959_parent", "tdb1959_child"];

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Child::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Parent>(pool).await;
    rustango::testkit::matrix::fresh_table::<Child>(pool).await;
    rustango::testkit::matrix::fresh_table::<Stamp>(pool).await;
}

async fn seed_parent_and_child(pool: &Pool) {
    let mut p = Parent {
        id: Auto::default(),
        name: "p".into(),
    };
    p.insert_pool(pool).await.expect("parent");
    let mut c = Child {
        id: Auto::default(),
        parent: ForeignKey::unloaded(p.id.get().copied().expect("pk")),
    };
    c.insert_pool(pool).await.expect("child");
}

async fn counts(pool: &Pool) -> (i64, i64) {
    (
        Parent::objects().count(pool).await.expect("count parents"),
        Child::objects().count(pool).await.expect("count children"),
    )
}

/// The parent is listed before the child that points at it: MySQL used
/// to fail on `"quoted"` names, SQLite on the FK order.
async fn truncate_clears_parent_listed_before_child(pool: &Pool) {
    seed_parent_and_child(pool).await;
    truncate_tables(pool, TABLES).await.expect("truncate");
    assert_eq!(counts(pool).await, (0, 0));
}

async fn truncate_after_clears_on_ok_and_err(pool: &Pool) {
    let ok: Result<i32, ExecError> = with_truncate_after(pool, TABLES, || async {
        seed_parent_and_child(pool).await;
        assert_eq!(counts(pool).await, (1, 1), "the body's writes commit");
        Ok(7)
    })
    .await;
    assert_eq!(ok.expect("ok body"), 7);
    assert_eq!(counts(pool).await, (0, 0), "cleared after Ok");

    let err: Result<(), ExecError> = with_truncate_after(pool, TABLES, || async {
        seed_parent_and_child(pool).await;
        Err(ExecError::EmptyReturning)
    })
    .await;
    assert!(err.is_err(), "the body's error comes back");
    assert_eq!(counts(pool).await, (0, 0), "cleared after Err");
}

/// A panicking body still gets its rows cleared, and the panic still
/// reaches the test.
async fn truncate_after_clears_when_body_panics(pool: &Pool) {
    let owned = pool.clone();
    let joined = tokio::spawn(async move {
        let pool = &owned;
        with_truncate_after(pool, TABLES, || async move {
            seed_parent_and_child(pool).await;
            panic!("body failed");
            #[allow(unreachable_code)]
            Ok::<(), ExecError>(())
        })
        .await
    })
    .await;
    assert!(
        joined.as_ref().is_err_and(tokio::task::JoinError::is_panic),
        "the panic must propagate: {joined:?}"
    );
    assert_eq!(counts(pool).await, (0, 0), "cleared after a panic");
}

async fn truncate_empty_slice_is_a_no_op(pool: &Pool) {
    seed_parent_and_child(pool).await;
    truncate_tables(pool, &[]).await.expect("no-op");
    assert_eq!(counts(pool).await, (1, 1));
}

/// Only the parent is listed while a child points at it: PG cascades and
/// restarts ids, MySQL leaves the orphan, SQLite fails at COMMIT and rolls back.
async fn truncate_unlisted_child_follows_the_dialect(pool: &Pool) {
    seed_parent_and_child(pool).await;
    let res = truncate_tables(pool, &["tdb1959_parent"]).await;
    match pool.dialect().name() {
        "postgres" => {
            res.expect("truncate");
            assert_eq!(counts(pool).await, (0, 0), "CASCADE clears the child");
        }
        "mysql" => {
            res.expect("truncate");
            assert_eq!(counts(pool).await, (0, 1), "the orphan child stays");
        }
        _ => {
            assert!(res.is_err(), "the deferred FK check fails at COMMIT");
            assert_eq!(counts(pool).await, (1, 1), "rolled back");
            return;
        }
    }
    let mut p = Parent {
        id: Auto::default(),
        name: "after".into(),
    };
    p.insert_pool(pool).await.expect("parent");
    let id = p.id.get().copied().expect("pk");
    if pool.dialect().name() == "postgres" {
        assert_eq!(id, 1, "RESTART IDENTITY");
    } else {
        assert!(id > 1, "MySQL ids keep counting: {id}");
    }
}

/// A failing statement rolls back the tables already cleared.
async fn truncate_rolls_back_on_error(pool: &Pool) {
    seed_parent_and_child(pool).await;
    let res = truncate_tables(
        pool,
        &["tdb1959_child", "tdb1959_parent", "tdb1959_missing"],
    )
    .await;
    assert!(res.is_err(), "missing table");
    assert_eq!(counts(pool).await, (1, 1), "nothing was cleared");
}

fn row(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().expect("object").clone()
}

/// Values are typed from the model (timestamptz / uuid on PG) and the
/// id sequence moves past the loaded ids.
async fn fixture_loads_typed_values_and_resets_the_sequence(pool: &Pool) {
    let token = uuid::Uuid::from_u128(0x1959);
    let n = Fixture::new("stamps")
        .with_row(row(serde_json::json!({
            "id": 1,
            "at": "2026-01-02T03:04:05Z",
            "token": token.to_string(),
            "label": "loaded",
        })))
        .load_into_pool(Stamp::SCHEMA.table, pool)
        .await
        .expect("typed load");
    assert_eq!(n, 1);

    let mut next = Stamp {
        id: Auto::default(),
        at: chrono::Utc::now(),
        token: uuid::Uuid::from_u128(2),
        label: "next".into(),
    };
    next.insert_pool(pool)
        .await
        .expect("an insert after the load reused a loaded id");

    let loaded: Vec<Stamp> = Stamp::objects()
        .filter("label", "loaded")
        .fetch(pool)
        .await
        .expect("fetch");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].token, token);
    assert_eq!(loaded[0].at.to_rfc3339(), "2026-01-02T03:04:05+00:00");
}

/// A failing row rolls the whole load back.
async fn fixture_load_is_all_or_nothing(pool: &Pool) {
    let res = Fixture::new("parents")
        .with_row(row(serde_json::json!({"id": 1, "name": "a"})))
        .with_row(row(serde_json::json!({"id": 1, "name": "duplicate"})))
        .load_into_pool(Parent::SCHEMA.table, pool)
        .await;
    assert!(
        matches!(res, Err(FixtureError::Database(_))),
        "dup pk: {res:?}"
    );
    assert_eq!(counts(pool).await, (0, 0), "the first row was rolled back");
}

async fn fixture_rejects_a_key_the_model_lacks(pool: &Pool) {
    let res = Fixture::new("parents")
        .with_row(row(serde_json::json!({"id": 1, "nmae": "typo"})))
        .load_into_pool(Parent::SCHEMA.table, pool)
        .await;
    assert!(matches!(res, Err(FixtureError::Format { .. })), "{res:?}");
}

/// `create_tables` on tables that exist must not fail re-adding FKs.
async fn create_tables_is_re_runnable(pool: &Pool) {
    rustango::testkit::create_tables(pool, &[Parent::SCHEMA, Child::SCHEMA])
        .await
        .expect("second create_tables");
}

/// An `atomic()` inside `with_rollback` is a savepoint of it, so its
/// insert is undone too (#1761).
async fn with_rollback_undoes_a_nested_atomic(pool: &Pool) {
    let insert =
        rustango::core::InsertQuery::new(Parent::SCHEMA, vec!["name"], vec!["nested".into()]);
    let inner = pool.clone();
    let seen = with_rollback(pool, move |_tx| {
        Box::pin(async move {
            rustango::sql::atomic(&inner, move |tx| {
                Box::pin(
                    async move { rustango::sql::insert_tx(&mut *tx.lock().await?, &insert).await },
                )
            })
            .await?;
            Ok(7)
        })
    })
    .await
    .expect("with_rollback");
    assert_eq!(seen, 7);
    assert_eq!(
        Parent::objects().count(pool).await.unwrap(),
        0,
        "nested atomic committed"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        with_rollback_undoes_a_nested_atomic,
        truncate_clears_parent_listed_before_child,
        truncate_after_clears_on_ok_and_err,
        truncate_after_clears_when_body_panics,
        truncate_empty_slice_is_a_no_op,
        truncate_unlisted_child_follows_the_dialect,
        truncate_rolls_back_on_error,
        fixture_loads_typed_values_and_resets_the_sequence,
        fixture_load_is_all_or_nothing,
        fixture_rejects_a_key_the_model_lacks,
        create_tables_is_re_runnable,
    ],
}

/// A custom user model next to the built-in `User` on `rustango_users`.
#[cfg(feature = "tenancy")]
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_users", app = "tdb1959")]
pub struct AppUser {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub username: String,
    #[rustango(max_length = 64)]
    pub display_name: String,
}

/// The extra field picks the custom model, whatever the link order.
/// SQLite only: the live servers share `rustango_users` with other suites.
#[cfg(all(feature = "sqlite", feature = "tenancy"))]
#[tokio::test]
async fn user_fixture_with_an_extra_field_uses_the_custom_model() {
    let on_users = rustango::inventory::iter::<rustango::core::ModelEntry>()
        .filter(|e| e.schema.table == "rustango_users")
        .count();
    assert!(on_users >= 2, "the built-in User must be registered too");
    let pool = rustango::testkit::matrix::Backend::Sqlite
        .pool()
        .await
        .expect("sqlite");
    rustango::testkit::matrix::fresh_table::<AppUser>(&pool).await;
    Fixture::new("users")
        .with_row(row(
            serde_json::json!({"username": "a", "display_name": "A"}),
        ))
        .load_into_pool("rustango_users", &pool)
        .await
        .expect("load");
    let users = AppUser::objects().fetch(&pool).await.expect("fetch");
    assert_eq!(users[0].display_name, "A");
}
