//! UPDATE runs the same field checks as INSERT; ModelForm checks them too (#1893).

#![cfg(all(
    feature = "forms",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::collections::HashMap;

use rustango::core::{
    Assignment, Filter, Model as _, Op, QueryError, SqlValue, UpdateQuery, WhereExpr,
};
use rustango::forms::{ModelForm, ModelFormError};
use rustango::sql::{Auto, ExecError, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "val1893_post", app = "val1893")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub title: String,
    #[rustango(max_length = 16, choices = "draft:Draft, live:Live")]
    pub status: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "val1893_note", app = "val1893", audit(track = "status"))]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16, choices = "draft:Draft, live:Live")]
    pub status: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Post>(pool).await;
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
    rustango::audit::ensure_table_pool(pool)
        .await
        .expect("audit table");
}

async fn seed(pool: &Pool) -> Post {
    let mut p = Post {
        id: Auto::Unset,
        title: "ok".into(),
        status: "draft".into(),
    };
    p.save_pool(pool).await.expect("seed");
    p
}

async fn stored_status(pool: &Pool) -> String {
    Post::objects().fetch(pool).await.expect("fetch")[0]
        .status
        .clone()
}

fn is_choice_error(e: &ExecError) -> bool {
    matches!(e, ExecError::Query(QueryError::InvalidChoice { .. }))
}

fn set_status(pk: i64, status: &str) -> UpdateQuery {
    UpdateQuery::new(
        Post::SCHEMA,
        vec![Assignment::new("status", SqlValue::String(status.into()))],
        WhereExpr::Predicate(Filter::new("id", Op::Eq, pk)),
    )
}

/// Every backend stores a bad choice as-is, so only the check refuses it.
async fn update_pool_and_tx_check_choices(pool: &Pool) {
    let p = seed(pool).await;
    let pk = *p.id.get().expect("pk");
    let err = rustango::sql::update_pool(pool, &set_status(pk, "bogus"))
        .await
        .expect_err("update_pool");
    assert!(is_choice_error(&err), "{err:?}");

    let mut tx = rustango::sql::transaction_pool(pool).await.expect("tx");
    let err = rustango::sql::update_tx(&mut tx, &set_status(pk, "bogus"))
        .await
        .expect_err("update_tx");
    assert!(is_choice_error(&err), "{err:?}");
    drop(tx);
    assert_eq!(stored_status(pool).await, "draft");
}

async fn save_pool_update_checks_fields(pool: &Pool) {
    let mut p = seed(pool).await;
    p.status = "bogus".into();
    let err = p.save_pool(pool).await.expect_err("save_pool");
    assert!(is_choice_error(&err), "{err:?}");
    assert_eq!(stored_status(pool).await, "draft");

    let mut n = Note {
        id: Auto::Unset,
        status: "draft".into(),
    };
    n.save_pool(pool).await.expect("seed note");
    n.status = "bogus".into();
    let err = n.save_pool(pool).await.expect_err("audited save_pool");
    assert!(is_choice_error(&err), "{err:?}");
}

async fn model_form_reports_field_errors(pool: &Pool) {
    let p = seed(pool).await;
    let pk = *p.id.get().expect("pk");
    let data = HashMap::from([
        ("title".to_owned(), "t".repeat(60)),
        ("status".to_owned(), "bogus".to_owned()),
    ]);
    let form = ModelForm::for_update(Post::SCHEMA, data, SqlValue::I64(pk));
    let errors = form.validate();
    assert!(errors.fields().get("title").is_some(), "{errors:?}");
    assert!(errors.fields().get("status").is_some(), "{errors:?}");
    let err = form.save(pool).await.expect_err("save");
    assert!(matches!(err, ModelFormError::Validation(_)), "{err:?}");
    assert_eq!(stored_status(pool).await, "draft");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        update_pool_and_tx_check_choices,
        save_pool_update_checks_fields,
        model_form_reports_field_errors,
    ],
}
