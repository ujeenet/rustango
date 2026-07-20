//! DDL-shape guards for the SSO provider models (`admin-sso`).
//!
//! The `slug` column is `unique`, so on MySQL it MUST render as a bounded
//! `VARCHAR(N)` — an unbounded `TEXT` can't be indexed without a key length
//! (MySQL error 1170). This pins that so a future edit dropping `max_length`
//! doesn't silently break MySQL (the class of bug fixed on the audit table).
#![cfg(all(feature = "admin-sso", feature = "mysql"))]

use rustango::admin::sso_provider::SsoProvider;
use rustango::core::Model as _;
use rustango::migrate::{detect_changes, render_changes_split_with_dialect, SchemaSnapshot};
use rustango::sql::{Dialect, MySql};

fn mysql_stmts() -> Vec<String> {
    let snap = SchemaSnapshot::from_models(&[SsoProvider::SCHEMA]);
    let changes = detect_changes(&SchemaSnapshot::default(), &snap);
    let batch = render_changes_split_with_dialect(&changes, &snap, &MySql as &dyn Dialect)
        .expect("render SsoProvider DDL for MySQL");
    batch
        .immediate
        .into_iter()
        .chain(batch.deferred_fks)
        .collect()
}

#[test]
fn slug_is_indexable_varchar_on_mysql() {
    let ddl = mysql_stmts().join("\n");
    assert!(ddl.contains("`rustango_sso_providers`"), "got: {ddl}");
    // Bounded VARCHAR so the UNIQUE(slug) is indexable — never TEXT (1170).
    assert!(ddl.contains("`slug` VARCHAR(64)"), "got: {ddl}");
    assert!(
        !ddl.to_uppercase().contains("`SLUG` TEXT"),
        "slug must not be TEXT on MySQL: {ddl}"
    );
    assert!(
        ddl.to_uppercase().contains("UNIQUE"),
        "expected a UNIQUE constraint on slug: {ddl}"
    );
}

/// Actually apply the rendered DDL against a real MySQL — this is what a
/// pure render check can't catch (error 1170 fires at CREATE time). Skips
/// when `MYSQL_TEST_URL` is unset.
#[tokio::test]
async fn applies_on_real_mysql() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    use rustango::sql::{raw_execute_pool, Pool};
    let pool = Pool::connect(&url).await.expect("connect MYSQL_TEST_URL");
    let drop = "DROP TABLE IF EXISTS `rustango_sso_providers`";
    let _ = raw_execute_pool(&pool, drop, Vec::new()).await;
    for stmt in mysql_stmts() {
        raw_execute_pool(&pool, &stmt, Vec::new())
            .await
            .expect("SsoProvider DDL must apply cleanly on MySQL (no 1170)");
    }
    let _ = raw_execute_pool(&pool, drop, Vec::new()).await;
}
