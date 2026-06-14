//! Django 6.0 ORM parity — execution-based verification.
//! Scenario group I: complex write paths — UPSERT, F-expression
//! atomic updates, Case/When in UPDATE SET, zero-row update
//! semantics (Django 6.0 `Model.NotUpdated`), get_or_create /
//! update_or_create.
//!
//! Django scenarios covered (docs.djangoproject.com/en/6.0):
//! - `bulk_create(objs, update_conflicts=True, unique_fields=...,
//!   update_fields=...)` — single- and two-column conflict targets
//! - `update(qty=F("qty") + 5)` atomic arithmetic, no
//!   read-modify-write race
//! - `update(status=Case(When(qty=0, then=Value("out")), ...))`
//! - Django 6.0 NEW: `save(force_update=True)` affecting 0 rows now
//!   raises `Model.NotUpdated` — rustango's divergence is pinned
//!   (silent `Ok(())`, rows-affected discarded by the macro save
//!   path; the QuerySet update path does surface the count)
//! - `get_or_create()` / `update_or_create()`

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use rustango::core::case::{case, value};
    use rustango::core::{Column as _, F};
    use rustango::sql::{raw_execute_pool, update_pool, Auto, FetcherPool as _, Pool};
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6wr_sku")]
    #[allow(dead_code)]
    pub struct Sku {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 40, unique)]
        pub code: String,
        pub qty: i64,
        pub price: i64,
        #[rustango(max_length = 10)]
        pub status: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6wr_stock", unique_together(columns = "warehouse, sku"))]
    #[allow(dead_code)]
    pub struct Stock {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 20)]
        pub warehouse: String,
        #[rustango(max_length = 40)]
        pub sku: String,
        pub qty: i64,
    }

    fn sku(code: &str, qty: i64, price: i64) -> Sku {
        Sku {
            id: Auto::default(),
            code: code.into(),
            qty,
            price,
            status: "new".into(),
        }
    }

    async fn fetch_sku(pool: &Pool, code: &str) -> Sku {
        let rows: Vec<Sku> = Sku::objects()
            .filter("code", code)
            .fetch(pool)
            .await
            .expect("fetch sku");
        assert_eq!(rows.len(), 1, "exactly one row for code {code}");
        rows.into_iter().next().unwrap()
    }

    /// Django `bulk_create(update_conflicts=True, unique_fields=
    /// ["code"], update_fields=["qty"])` — idempotent re-run updates
    /// only the listed columns.
    pub async fn check_bulk_upsert_idempotent(pool: &Pool) {
        let first = vec![sku("a", 10, 100), sku("b", 20, 200)];
        Sku::bulk_upsert_pool(&first, &["code"], &["qty", "price"], pool)
            .await
            .expect("first upsert");
        assert_eq!(Sku::count(pool).await.unwrap(), 2);

        // Re-run with conflicting codes: qty in the update set, price
        // NOT — price must keep its original value.
        let second = vec![sku("a", 11, 999), sku("c", 30, 300)];
        Sku::bulk_upsert_pool(&second, &["code"], &["qty"], pool)
            .await
            .expect("second upsert");
        assert_eq!(Sku::count(pool).await.unwrap(), 3, "a updated, c inserted");
        let a = fetch_sku(pool, "a").await;
        assert_eq!(a.qty, 11, "qty is in update_fields");
        assert_eq!(a.price, 100, "price is NOT in update_fields");
    }

    /// Two-column conflict target — Django's `unique_fields=
    /// ["warehouse", "sku"]` against a composite UNIQUE constraint.
    pub async fn check_bulk_upsert_two_column_unique_target(pool: &Pool) {
        let mk = |wh: &str, s: &str, qty: i64| Stock {
            id: Auto::default(),
            warehouse: wh.into(),
            sku: s.into(),
            qty,
        };
        Stock::bulk_upsert_pool(
            &[mk("east", "w-1", 5), mk("west", "w-1", 7)],
            &["warehouse", "sku"],
            &["qty"],
            pool,
        )
        .await
        .expect("first upsert");
        // Same (warehouse, sku) pair → update; same sku in a new
        // warehouse → insert.
        Stock::bulk_upsert_pool(
            &[mk("east", "w-1", 50), mk("north", "w-1", 9)],
            &["warehouse", "sku"],
            &["qty"],
            pool,
        )
        .await
        .expect("second upsert");
        assert_eq!(Stock::count(pool).await.unwrap(), 3);
        let east: Vec<Stock> = Stock::objects()
            .filter("warehouse", "east")
            .fetch(pool)
            .await
            .expect("fetch east");
        assert_eq!(east[0].qty, 50);
    }

    /// Django `update(qty=F("qty") + 5)` — DB-side arithmetic.
    pub async fn check_f_expression_atomic_update(pool: &Pool) {
        Sku::bulk_upsert_pool(&[sku("f-test", 10, 1)], &["code"], &["qty"], pool)
            .await
            .expect("seed");
        let q = Sku::objects()
            .filter("code", "f-test")
            .update()
            .set_expr("qty", F("qty") + 5_i64)
            .compile()
            .expect("update compile");
        let affected = update_pool(pool, &q).await.expect("update execute");
        assert_eq!(affected, 1);
        assert_eq!(fetch_sku(pool, "f-test").await.qty, 15);
    }

    /// Django `update(status=Case(When(qty=0, then=Value("out")),
    /// default=Value("in")))` — conditional bulk update.
    pub async fn check_case_when_in_update(pool: &Pool) {
        Sku::bulk_upsert_pool(
            &[sku("c-zero", 0, 1), sku("c-some", 4, 1)],
            &["code"],
            &["qty"],
            pool,
        )
        .await
        .expect("seed");
        let q = Sku::objects()
            .update()
            .set_expr(
                "status",
                case()
                    .when(Sku::qty.eq(0_i64), value("out"))
                    .default(value("in")),
            )
            .compile()
            .expect("update compile");
        let affected = update_pool(pool, &q).await.expect("update execute");
        assert!(affected >= 2);
        assert_eq!(fetch_sku(pool, "c-zero").await.status, "out");
        assert_eq!(fetch_sku(pool, "c-some").await.status, "in");
    }

    /// Django 6.0 NEW: a forced update affecting 0 rows raises
    /// `Model.NotUpdated`. rustango: the QuerySet update path DOES
    /// surface rows-affected (0 here), but the instance-level
    /// `save_pool` on a stale row silently returns `Ok(())` — the
    /// macro discards rows-affected. DIVERGENCE PIN: this goes red — issue #1029 —
    /// (and the audit row flips) if save ever starts surfacing it.
    pub async fn check_zero_row_update_semantics(pool: &Pool) {
        // QuerySet path: no match → 0 affected, no error. Same as
        // Django's `.update()` (which never raises NotUpdated).
        let q = Sku::objects()
            .filter("code", "ghost")
            .update()
            .set("qty", 1_i64)
            .compile()
            .expect("update compile");
        let affected = update_pool(pool, &q).await.expect("update execute");
        assert_eq!(affected, 0);

        // Instance path: fetch a row, delete it out from under the
        // instance, then save — the UPDATE matches nothing.
        Sku::bulk_upsert_pool(&[sku("stale", 1, 1)], &["code"], &["qty"], pool)
            .await
            .expect("seed");
        let mut stale = fetch_sku(pool, "stale").await;
        raw_execute_pool(pool, "DELETE FROM d6wr_sku WHERE code = 'stale'", vec![])
            .await
            .expect("delete behind the instance's back");
        stale.qty = 99;
        stale
            .save_pool(pool)
            .await
            .expect("PINNED DIVERGENCE: rustango save() is silent on 0-row update; Django 6.0 raises Model.NotUpdated");
        assert_eq!(
            Sku::objects()
                .filter("code", "stale")
                .fetch(pool)
                .await
                .map(|rows: Vec<Sku>| rows.len())
                .unwrap(),
            0,
            "the silent save really did update nothing"
        );
    }

    /// Django `get_or_create(code="goc")` — create-then-find.
    pub async fn check_get_or_create(pool: &Pool) {
        let (created_sku, created) = Sku::objects()
            .filter("code", "goc")
            .get_or_create(
                |pool| async move {
                    let mut s = sku("goc", 1, 10);
                    s.save_pool(&pool).await?;
                    Ok(s)
                },
                pool,
            )
            .await
            .expect("get_or_create #1");
        assert!(created);
        assert_eq!(created_sku.code, "goc");

        let (found, created) = Sku::objects()
            .filter("code", "goc")
            .get_or_create(
                |_pool| async move { panic!("row exists — create must not run") },
                pool,
            )
            .await
            .expect("get_or_create #2");
        assert!(!created);
        assert_eq!(found.qty, 1);
    }

    /// Django `update_or_create(code="uoc", defaults={"qty": 5})`.
    pub async fn check_update_or_create(pool: &Pool) {
        let (row, created) = Sku::objects()
            .filter("code", "uoc")
            .update_or_create(
                |_pool, _row| async move { panic!("missing row — update must not run") },
                |pool| async move {
                    let mut s = sku("uoc", 1, 10);
                    s.save_pool(&pool).await?;
                    Ok(s)
                },
                pool,
            )
            .await
            .expect("update_or_create create branch");
        assert!(created);
        assert_eq!(row.qty, 1);

        let (row, created) = Sku::objects()
            .filter("code", "uoc")
            .update_or_create(
                |pool, mut row| async move {
                    row.qty = 5;
                    row.save_pool(&pool).await?;
                    Ok(row)
                },
                |_pool| async move { panic!("row exists — create must not run") },
                pool,
            )
            .await
            .expect("update_or_create update branch");
        assert!(!created);
        assert_eq!(row.qty, 5);
        assert_eq!(fetch_sku(pool, "uoc").await.qty, 5);
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
            r#"DROP TABLE IF EXISTS "d6wr_sku" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6wr_stock" CASCADE"#,
            r#"CREATE TABLE "d6wr_sku" (
                "id" BIGSERIAL PRIMARY KEY,
                "code" VARCHAR(40) NOT NULL UNIQUE,
                "qty" BIGINT NOT NULL,
                "price" BIGINT NOT NULL,
                "status" VARCHAR(10) NOT NULL
            )"#,
            r#"CREATE TABLE "d6wr_stock" (
                "id" BIGSERIAL PRIMARY KEY,
                "warehouse" VARCHAR(20) NOT NULL,
                "sku" VARCHAR(40) NOT NULL,
                "qty" BIGINT NOT NULL,
                UNIQUE ("warehouse", "sku")
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
                scenarios::$name(&pool).await;
            }
        };
    }

    pg_case!(check_bulk_upsert_idempotent);
    pg_case!(check_bulk_upsert_two_column_unique_target);
    pg_case!(check_f_expression_atomic_update);
    pg_case!(check_case_when_in_update);
    pg_case!(check_zero_row_update_semantics);
    pg_case!(check_get_or_create);
    pg_case!(check_update_or_create);
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
            "CREATE TABLE d6wr_sku (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                code TEXT NOT NULL UNIQUE,
                qty INTEGER NOT NULL,
                price INTEGER NOT NULL,
                status TEXT NOT NULL
            )",
            "CREATE TABLE d6wr_stock (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                warehouse TEXT NOT NULL,
                sku TEXT NOT NULL,
                qty INTEGER NOT NULL,
                UNIQUE (warehouse, sku)
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
                scenarios::$name(&pool).await;
            }
        };
    }

    sqlite_case!(check_bulk_upsert_idempotent);
    sqlite_case!(check_bulk_upsert_two_column_unique_target);
    sqlite_case!(check_f_expression_atomic_update);
    sqlite_case!(check_case_when_in_update);
    sqlite_case!(check_zero_row_update_semantics);
    sqlite_case!(check_get_or_create);
    sqlite_case!(check_update_or_create);
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
            "DROP TABLE IF EXISTS d6wr_sku",
            "DROP TABLE IF EXISTS d6wr_stock",
            "CREATE TABLE d6wr_sku (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                code VARCHAR(40) NOT NULL UNIQUE,
                qty BIGINT NOT NULL,
                price BIGINT NOT NULL,
                status VARCHAR(10) NOT NULL
            )",
            "CREATE TABLE d6wr_stock (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                warehouse VARCHAR(20) NOT NULL,
                sku VARCHAR(40) NOT NULL,
                qty BIGINT NOT NULL,
                UNIQUE KEY uq_wh_sku (warehouse, sku)
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
                scenarios::$name(&pool).await;
            }
        };
    }

    mysql_case!(check_bulk_upsert_idempotent);
    mysql_case!(check_bulk_upsert_two_column_unique_target);
    mysql_case!(check_f_expression_atomic_update);
    mysql_case!(check_case_when_in_update);
    mysql_case!(check_zero_row_update_semantics);
    mysql_case!(check_get_or_create);
    mysql_case!(check_update_or_create);
}
