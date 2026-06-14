//! Django 6.0 ORM parity — execution-based verification.
//! Scenario group J: the Django 6.0 release-note delta — features new
//! or changed in 6.0, each verified or pinned against rustango.
//!
//! Release-note items covered here:
//! - `StringAgg` is database-agnostic in Django 6.0 (was PG-only
//!   contrib.postgres) — rustango lowers it to GROUP_CONCAT (MySQL) /
//!   group_concat (SQLite) (#1024)
//! - `AnyValue` aggregate (new in 6.0) — PG `any_value()`, MySQL
//!   `ANY_VALUE()`, SQLite `min()` fallback (#1025)
//! - `GeneratedField` values are refreshed from the database after
//!   `save()` on RETURNING-capable backends (PG/SQLite) — rustango
//!   never refreshes: PINNED DIVERGENCE (DB value correct, struct
//!   stale)
//! - `DEFAULT_AUTO_FIELD` now defaults to `BigAutoField` — rustango's
//!   `Auto<i64>` ↔ BIGSERIAL/BIGINT AUTO_INCREMENT already matches
//!
//! - `Aggregate(order_by=...)` (new in 6.0, deprecates PG
//!   `OrderableAggMixin`) — `string_agg_ordered` / `_distinct_ordered`
//!   emit `ORDER BY` inside the aggregate (#1026).
//!
//! Release-note items that are compile-time API absences (no runtime
//! pin possible — audit rows + issues only):
//! - `Model.NotUpdated` on forced 0-row update — pinned in
//!   django6_writes.rs::check_zero_row_update_semantics.
//! - CompositePrimaryKey enhancements (raw(), subquery lookups) — N/A
//!   by architecture: rustango's composite-key idiom is surrogate
//!   `Auto<i64>` PK + `unique_together`, so the 6.0 enhancements have
//!   no surface to land on.

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use rustango::core::{AggregateExpr, SqlValue};
    use rustango::sql::{raw_execute_pool, Auto, FetcherPool as _, Pool};
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6delta_row")]
    #[allow(dead_code)]
    pub struct DeltaRow {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 40)]
        pub name: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6delta_invoice")]
    #[allow(dead_code)]
    pub struct Invoice {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        pub price: i64,
        pub qty: i64,
        /// Database-computed `price * qty` — the struct carries a
        /// placeholder; the macro skips the column on INSERT/UPDATE.
        #[rustango(generated_as = "price * qty")]
        pub total: i64,
    }

    pub async fn seed(pool: &Pool) {
        for sql in [
            "INSERT INTO d6delta_row (id, name) VALUES (1, 'alpha')",
            "INSERT INTO d6delta_row (id, name) VALUES (2, 'beta')",
            "INSERT INTO d6delta_row (id, name) VALUES (3, 'gamma')",
            "INSERT INTO d6delta_row (id, name) VALUES (4, 'beta')",
        ] {
            raw_execute_pool(pool, sql, vec![]).await.expect("seed");
        }
    }

    /// Django 6.0: `StringAgg("name", delimiter=Value(","))` is
    /// database-agnostic — PG `string_agg`, MySQL `GROUP_CONCAT`, SQLite
    /// `group_concat` (#1024). Uniform assertion across all backends.
    pub async fn check_string_agg_dialect_matrix(pool: &Pool) {
        let rows = DeltaRow::objects()
            .aggregate()
            .values(&[])
            .annotate("names", AggregateExpr::string_agg("name", ","))
            .fetch(pool)
            .await
            .expect("string_agg on every backend");
        let joined = match rows[0].get("names") {
            Some(SqlValue::String(s)) => s.clone(),
            other => panic!("expected string names, got {other:?}"),
        };
        // Unordered form — element order is backend-arbitrary, so sort
        // before asserting (the ordered form is `check_string_agg_ordered`).
        let mut parts: Vec<&str> = joined.split(',').collect();
        parts.sort_unstable();
        assert_eq!(parts, vec!["alpha", "beta", "beta", "gamma"]);
    }

    /// `string_agg(DISTINCT name, ",")` — the dedup variant. With the
    /// default `,` delimiter this works on every backend (#1024): PG
    /// `string_agg(DISTINCT …)`, MySQL `GROUP_CONCAT(DISTINCT …)`, SQLite
    /// `group_concat(DISTINCT …)` (DISTINCT + a *custom* delimiter stays
    /// rejected on SQLite — covered by the emission tests).
    pub async fn check_string_agg_distinct(pool: &Pool) {
        let rows = DeltaRow::objects()
            .aggregate()
            .values(&[])
            .annotate("names", AggregateExpr::string_agg_distinct("name", ","))
            .fetch(pool)
            .await
            .expect("string_agg_distinct on PG");
        let joined = match rows[0].get("names") {
            Some(SqlValue::String(s)) => s.clone(),
            other => panic!("expected string names, got {other:?}"),
        };
        let mut parts: Vec<&str> = joined.split(',').collect();
        parts.sort_unstable();
        assert_eq!(
            parts,
            vec!["alpha", "beta", "gamma"],
            "duplicate beta deduped"
        );
    }

    /// Django 6.0 `Aggregate(order_by=…)` (#1026) — ordered StringAgg.
    /// With ORDER BY the joined string is deterministic, so we assert the
    /// EXACT result (no sort-to-compensate). PG/MySQL native; SQLite needs
    /// 3.44+ for ORDER BY inside an aggregate.
    pub async fn check_string_agg_ordered(pool: &Pool) {
        let rows = DeltaRow::objects()
            .aggregate()
            .values(&[])
            .annotate(
                "names",
                AggregateExpr::string_agg_ordered("name", ",", &[("name", false)]),
            )
            .fetch(pool)
            .await
            .expect("ordered string_agg on every backend");
        let joined = match rows[0].get("names") {
            Some(SqlValue::String(s)) => s.clone(),
            other => panic!("expected string names, got {other:?}"),
        };
        assert_eq!(
            joined, "alpha,beta,beta,gamma",
            "exact ascending-by-name order"
        );
    }

    /// Django 6.0 `AnyValue` (#1025) — projects a value from each group
    /// without adding the column to GROUP BY. Group by name, then take
    /// `any_value(id)`: the returned id must be a member of that name's
    /// group (PG/MySQL pick arbitrarily; SQLite's `min()` fallback picks
    /// the lowest — both are valid group members).
    pub async fn check_any_value(pool: &Pool) {
        let rows = DeltaRow::objects()
            .aggregate()
            .group_by("name")
            .annotate("anyid", AggregateExpr::AnyValue("id"))
            .fetch(pool)
            .await
            .expect("any_value on every backend");
        assert_eq!(rows.len(), 3, "three distinct names");
        for row in &rows {
            let name = match row.get("name") {
                Some(SqlValue::String(s)) => s.as_str(),
                other => panic!("expected name, got {other:?}"),
            };
            let anyid = match row.get("anyid") {
                Some(SqlValue::I64(n)) => *n,
                Some(SqlValue::I32(n)) => i64::from(*n),
                other => panic!("expected integer anyid, got {other:?}"),
            };
            // Seed: alpha=id1, beta=id2+id4, gamma=id3.
            let ok = match name {
                "alpha" => anyid == 1,
                "beta" => anyid == 2 || anyid == 4,
                "gamma" => anyid == 3,
                other => panic!("unexpected name {other}"),
            };
            assert!(
                ok,
                "any_value(id) for {name} returned {anyid}, not a group member"
            );
        }
    }

    /// Django 6.0: after `save()`, `GeneratedField`s refresh from the
    /// database via RETURNING (PG/SQLite; deferred on MySQL).
    /// rustango DIVERGENCE PIN: the DB computes the column correctly
    /// but the in-memory struct is never refreshed — on ANY backend. Issue #1028.
    /// Goes red (flip the audit row) when save() starts returning
    /// generated columns.
    pub async fn check_generated_column_not_refreshed_on_save(pool: &Pool) {
        let mut inv = Invoice {
            id: Auto::default(),
            price: 3,
            qty: 4,
            total: 0, // placeholder — DB computes 12
        };
        inv.save_pool(pool).await.expect("insert");
        assert_eq!(
            inv.total, 0,
            "PINNED DIVERGENCE: struct not refreshed after save — Django 6.0 \
             refreshes GeneratedFields via RETURNING; if this is now 12, update \
             the audit + issue"
        );
        let fetched: Vec<Invoice> = Invoice::objects().fetch(pool).await.expect("re-fetch");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].total, 12, "the DB-side value IS computed");
    }

    /// Django 6.0: `DEFAULT_AUTO_FIELD` now defaults to
    /// `BigAutoField`. rustango's `Auto<i64>` maps to
    /// BIGSERIAL / BIGINT AUTO_INCREMENT / INTEGER PRIMARY KEY —
    /// 64-bit on every backend, already matching.
    pub async fn check_auto_pk_is_big_auto_field(pool: &Pool) {
        let mut inv = Invoice {
            id: Auto::default(),
            price: 1,
            qty: 1,
            total: 0,
        };
        inv.save_pool(pool).await.expect("insert");
        let pk: i64 = *inv.id.get().expect("Auto PK populated after save");
        assert!(pk > 0, "64-bit auto PK assigned: {pk}");
    }
}

// ------------------------------------------------------------- Postgres

#[cfg(feature = "postgres")]
mod pg_live {
    use std::sync::OnceLock;

    use rustango::sql::{sqlx, Pool};
    use tokio::sync::Mutex;

    use super::scenarios;

    fn live_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    async fn fresh_pool() -> Option<Pool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pg = sqlx::PgPool::connect(&url).await.ok()?;
        for sql in [
            r#"DROP TABLE IF EXISTS "d6delta_row" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6delta_invoice" CASCADE"#,
            r#"CREATE TABLE "d6delta_row" (
                "id" BIGINT PRIMARY KEY,
                "name" VARCHAR(40) NOT NULL
            )"#,
            r#"CREATE TABLE "d6delta_invoice" (
                "id" BIGSERIAL PRIMARY KEY,
                "price" BIGINT NOT NULL,
                "qty" BIGINT NOT NULL,
                "total" BIGINT GENERATED ALWAYS AS ("price" * "qty") STORED
            )"#,
        ] {
            sqlx::query(sql).execute(&pg).await.unwrap();
        }
        Some(Pool::Postgres(pg))
    }

    macro_rules! pg_case {
        ($name:ident) => {
            #[tokio::test]
            async fn $name() {
                let _g = live_lock().lock().await;
                let Some(pool) = fresh_pool().await else {
                    eprintln!("DATABASE_URL unset — skipping PG django6 test");
                    return;
                };
                scenarios::seed(&pool).await;
                scenarios::$name(&pool).await;
            }
        };
    }

    pg_case!(check_string_agg_dialect_matrix);
    pg_case!(check_string_agg_distinct);
    pg_case!(check_any_value);
    pg_case!(check_string_agg_ordered);
    pg_case!(check_generated_column_not_refreshed_on_save);
    pg_case!(check_auto_pk_is_big_auto_field);
}

// --------------------------------------------------------------- SQLite

#[cfg(feature = "sqlite")]
mod sqlite_live {
    use rustango::sql::{sqlx, Pool};

    use super::scenarios;

    async fn fresh_pool() -> Pool {
        let sq = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite mem pool");
        for sql in [
            "CREATE TABLE d6delta_row (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            )",
            "CREATE TABLE d6delta_invoice (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                price INTEGER NOT NULL,
                qty INTEGER NOT NULL,
                total INTEGER GENERATED ALWAYS AS (price * qty) STORED
            )",
        ] {
            sqlx::query(sql).execute(&sq).await.expect("ddl");
        }
        Pool::Sqlite(sq)
    }

    macro_rules! sqlite_case {
        ($name:ident) => {
            #[tokio::test]
            async fn $name() {
                let pool = fresh_pool().await;
                scenarios::seed(&pool).await;
                scenarios::$name(&pool).await;
            }
        };
    }

    sqlite_case!(check_string_agg_dialect_matrix);
    sqlite_case!(check_string_agg_distinct);
    sqlite_case!(check_any_value);
    sqlite_case!(check_string_agg_ordered);
    sqlite_case!(check_generated_column_not_refreshed_on_save);
    sqlite_case!(check_auto_pk_is_big_auto_field);
}

// ---------------------------------------------------------------- MySQL

#[cfg(feature = "mysql")]
mod mysql_live {
    use std::sync::OnceLock;

    use rustango::sql::{sqlx, Pool};
    use tokio::sync::Mutex;

    use super::scenarios;

    fn live_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    async fn fresh_pool() -> Option<Pool> {
        let url = std::env::var("MYSQL_TEST_URL").ok()?;
        let my = sqlx::MySqlPool::connect(&url).await.ok()?;
        for sql in [
            "DROP TABLE IF EXISTS d6delta_row",
            "DROP TABLE IF EXISTS d6delta_invoice",
            "CREATE TABLE d6delta_row (
                id BIGINT PRIMARY KEY,
                name VARCHAR(40) NOT NULL
            )",
            "CREATE TABLE d6delta_invoice (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                price BIGINT NOT NULL,
                qty BIGINT NOT NULL,
                total BIGINT GENERATED ALWAYS AS (price * qty) STORED
            )",
        ] {
            sqlx::query(sql).execute(&my).await.unwrap();
        }
        Some(Pool::Mysql(my))
    }

    macro_rules! mysql_case {
        ($name:ident) => {
            #[tokio::test]
            async fn $name() {
                let _g = live_lock().lock().await;
                let Some(pool) = fresh_pool().await else {
                    eprintln!("MYSQL_TEST_URL unset — skipping MySQL django6 test");
                    return;
                };
                scenarios::seed(&pool).await;
                scenarios::$name(&pool).await;
            }
        };
    }

    mysql_case!(check_string_agg_dialect_matrix);
    mysql_case!(check_string_agg_distinct);
    mysql_case!(check_any_value);
    mysql_case!(check_string_agg_ordered);
    mysql_case!(check_generated_column_not_refreshed_on_save);
    mysql_case!(check_auto_pk_is_big_auto_field);
}
