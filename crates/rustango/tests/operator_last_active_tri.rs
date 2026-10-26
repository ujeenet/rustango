//! Two operators deactivating each other at once leave one active.

#![cfg(all(feature = "tenancy", feature = "testkit"))]

use std::sync::Arc;

use rustango::core::Column as _;
use rustango::sql::{Auto, CounterPool as _, Pool};
use rustango::tenancy::operators::{set_active, OperatorError, Outcome};
use rustango::tenancy::Operator;
use rustango::tri_dialect_test;

async fn operator(pool: &Pool, username: String) -> Operator {
    let mut op = Operator {
        id: Auto::default(),
        username,
        password_hash: "x".into(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    op.insert_pool(pool).await.expect("insert operator");
    op
}

/// Each of two active operators deactivates the other, released together.
async fn mutual_deactivation_keeps_one_active(pool: &Pool) {
    for round in 0..40 {
        let objs = Operator::objects()
            .where_(Operator::id.gt(0))
            .compile_delete()
            .expect("delete all");
        rustango::sql::delete_pool(pool, &objs)
            .await
            .expect("clear");
        let a = operator(pool, format!("a{round}")).await;
        let b = operator(pool, format!("b{round}")).await;
        let (a_id, b_id) = (*a.id.get().unwrap(), *b.id.get().unwrap());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let run = |mut target: Operator, actor: i64| {
            let (pool, barrier) = (pool.clone(), barrier.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                set_active(&pool, &mut target, false, Some(actor)).await
            })
        };
        let (x, y) = (run(b, a_id), run(a, b_id));
        let results = [x.await.unwrap(), y.await.unwrap()];
        let active = Operator::objects()
            .where_(Operator::active.eq(true))
            .count(pool)
            .await
            .expect("count");
        assert_eq!(active, 1, "round {round}: {active} active, {results:?}");
        let changed = results
            .iter()
            .filter(|r| matches!(r, Ok(Outcome::Changed)))
            .count();
        assert_eq!(changed, 1, "round {round}: {results:?}");
        assert!(
            results
                .iter()
                .any(|r| matches!(r, Err(OperatorError::LastActive))),
            "round {round}: {results:?}"
        );
    }
}

/// A target someone else already deactivated reports "already so".
async fn a_stale_target_reports_already_so(pool: &Pool) {
    let objs = Operator::objects()
        .where_(Operator::id.gt(0))
        .compile_delete()
        .expect("delete all");
    rustango::sql::delete_pool(pool, &objs)
        .await
        .expect("clear");
    let _keeper = operator(pool, "keeper".into()).await;
    let _other = operator(pool, "other".into()).await;
    let target = operator(pool, "target".into()).await;
    let mut first = target.clone();
    let mut stale = target;
    assert!(matches!(
        set_active(pool, &mut first, false, None).await,
        Ok(Outcome::Changed)
    ));
    assert!(matches!(
        set_active(pool, &mut stale, false, None).await,
        Ok(Outcome::AlreadySo)
    ));
    assert!(!stale.active);
}

tri_dialect_test!(
    model: Operator,
    sqlite: file,
    scenarios: [
        mutual_deactivation_keeps_one_active,
        a_stale_target_reports_already_so,
    ]
);
