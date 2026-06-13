//! Django 6.0 ORM parity — execution-based verification.
//! Scenario groups G (JSONField lookups) + H (date transforms,
//! `.dates()` / `.datetimes()`).
//!
//! Django scenarios covered (docs.djangoproject.com/en/6.0):
//! - `filter(data__meta__kind="post")` nested key traversal
//! - `filter(data__items__0__name="x")` key + array-index traversal
//! - JSON array length lookup (`tags__len__gte` shape)
//! - negative array indexing — PG (native) + SQLite (`$[#-1]` anchor,
//!   Django 6.0 + #1027); MySQL's `$[N]` path syntax genuinely can't
//!   express it upstream (documented rejection)
//! - `filter(created__year__gte=...)` / `__month` / `__quarter`
//!   date-transform chains
//! - `.dates("created", "month")` / `.datetimes("created", "hour")`
//!   truncation querysets

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use rustango::core::funcs::{json_array_length, json_path, json_path_indexed};
    use rustango::core::{Expr, JsonPathStep, Op, SqlValue, WhereExpr, F};
    use rustango::query::{DateKind, DateTimeKind};
    use rustango::sql::{
        fetch_dates_pool, fetch_datetimes_pool, raw_execute_pool, ExecError, FetcherPool as _,
        Pool, SqlError,
    };
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6jd_doc")]
    #[allow(dead_code)]
    pub struct Doc {
        #[rustango(primary_key)]
        pub id: i64,
        pub data: serde_json::Value,
        pub created: chrono::DateTime<chrono::Utc>,
    }

    pub async fn seed(pool: &Pool) {
        for sql in [
            r#"INSERT INTO d6jd_doc (id, data, created) VALUES (1, '{"meta":{"kind":"post"},"items":[{"name":"x"},{"name":"y"}],"tags":["a","b","c"]}', '2024-03-15 10:30:00')"#,
            r#"INSERT INTO d6jd_doc (id, data, created) VALUES (2, '{"meta":{"kind":"page"},"items":[{"name":"z"}],"tags":["a"]}', '2024-03-15 14:00:00')"#,
            r#"INSERT INTO d6jd_doc (id, data, created) VALUES (3, '{"meta":{"kind":"post"},"items":[{"name":"w"}],"tags":["a","b"]}', '2025-07-01 09:00:00')"#,
        ] {
            raw_execute_pool(pool, sql, vec![]).await.expect("seed");
        }
    }

    fn ids(rows: Vec<Doc>) -> Vec<i64> {
        let mut ids: Vec<i64> = rows.into_iter().map(|d| d.id).collect();
        ids.sort_unstable();
        ids
    }

    /// Django `filter(data__meta__kind="post")` — nested object keys,
    /// compared as text (`->>` / `JSON_UNQUOTE(JSON_EXTRACT(...))` /
    /// `json_extract`).
    pub async fn check_nested_key_traversal(pool: &Pool) {
        let rows: Vec<Doc> = Doc::objects()
            .where_raw(WhereExpr::ExprCompare {
                lhs: json_path(F("data"), &["meta", "kind"], true),
                op: Op::Eq,
                rhs: Expr::Literal(SqlValue::String("post".into())),
            })
            .fetch_pool(pool)
            .await
            .expect("nested key traversal");
        assert_eq!(ids(rows), vec![1, 3]);
    }

    /// Django `filter(data__items__0__name="x")` — key + array index.
    pub async fn check_key_index_traversal(pool: &Pool) {
        let rows: Vec<Doc> = Doc::objects()
            .where_raw(WhereExpr::ExprCompare {
                lhs: json_path_indexed(
                    F("data"),
                    [
                        JsonPathStep::Key("items".into()),
                        JsonPathStep::Index(0),
                        JsonPathStep::Key("name".into()),
                    ],
                    true,
                ),
                op: Op::Eq,
                rhs: Expr::Literal(SqlValue::String("x".into())),
            })
            .fetch_pool(pool)
            .await
            .expect("key+index traversal");
        assert_eq!(ids(rows), vec![1]);
    }

    /// JSON array length — docs with at least two tags.
    pub async fn check_json_array_length(pool: &Pool) {
        let rows: Vec<Doc> = Doc::objects()
            .where_raw(WhereExpr::ExprCompare {
                lhs: json_array_length(json_path(F("data"), &["tags"], false)),
                op: Op::Gte,
                rhs: Expr::Literal(SqlValue::I64(2)),
            })
            .fetch_pool(pool)
            .await
            .expect("json_array_length");
        assert_eq!(ids(rows), vec![1, 3]);
    }

    /// Negative array index (`data__tags__-1`). PG (native `-> -1`) and
    /// SQLite (the `$[#-1]` from-the-end anchor, Django 6.0 + #1027) both
    /// resolve it; MySQL's `$[N]` path grammar has no negative form, so
    /// it stays a documented rejection.
    pub async fn check_negative_index_dialect_matrix(pool: &Pool) {
        let qs = || {
            Doc::objects().where_raw(WhereExpr::ExprCompare {
                lhs: json_path_indexed(
                    F("data"),
                    [JsonPathStep::Key("tags".into()), JsonPathStep::Index(-1)],
                    true,
                ),
                op: Op::Eq,
                rhs: Expr::Literal(SqlValue::String("c".into())),
            })
        };
        let name = pool.dialect().name();
        if name == "postgres" || name == "sqlite" {
            let rows: Vec<Doc> = qs()
                .fetch_pool(pool)
                .await
                .expect("negative index (PG / SQLite)");
            assert_eq!(ids(rows), vec![1], "row 1's last tag is \"c\"");
        } else {
            // MySQL only — `$[N]` has no negative form upstream.
            let err = qs()
                .fetch_pool(pool)
                .await
                .map(|rows: Vec<Doc>| rows.len())
                .expect_err("negative index must be rejected on MySQL");
            match err {
                ExecError::Sql(SqlError::OpNotSupportedInDialect { op, dialect }) => {
                    assert!(
                        op.contains("negative"),
                        "error should mention negative indices: {op}"
                    );
                    assert_eq!(dialect, "mysql");
                }
                other => panic!("expected OpNotSupportedInDialect on MySQL, got {other:?}"),
            }
        }
    }

    /// Django `filter(created__year__gte=2025)` and
    /// `filter(created__month=3)` — date-transform lookups with and
    /// without trailing comparisons.
    pub async fn check_date_transform_chains(pool: &Pool) {
        let rows: Vec<Doc> = Doc::objects()
            .filter("created__year__gte", 2025_i64)
            .fetch_pool(pool)
            .await
            .expect("__year__gte");
        assert_eq!(ids(rows), vec![3]);

        let rows: Vec<Doc> = Doc::objects()
            .filter("created__month", 3_i64)
            .fetch_pool(pool)
            .await
            .expect("__month");
        assert_eq!(ids(rows), vec![1, 2]);
    }

    /// `filter(created__quarter=N)` across all three dialects. PG/MySQL
    /// have native QUARTER extraction; SQLite synthesizes it from the
    /// month (`((month + 2) / 3)`), matching Django. Issue #1037 — was a
    /// SQLite gap-pin, now uniform.
    pub async fn check_quarter_dialect_matrix(pool: &Pool) {
        // March (rows 1+2) is Q1 on every backend.
        let q1 = Doc::objects()
            .filter("created__quarter", 1_i64)
            .fetch_pool(pool)
            .await
            .expect("__quarter Q1");
        assert_eq!(ids(q1), vec![1, 2], "March is Q1");

        // July (row 3, seeded 2025-07-01) is Q3 — guards the arithmetic
        // off-by-one in the SQLite synthesis.
        let q3 = Doc::objects()
            .filter("created__quarter", 3_i64)
            .fetch_pool(pool)
            .await
            .expect("__quarter Q3");
        assert_eq!(ids(q3), vec![3], "July is Q3");
    }

    /// Django `.dates("created", "month")` — distinct truncated
    /// dates, ascending, plus `order_desc` reversal.
    pub async fn check_dates_truncation(pool: &Pool) {
        let months = fetch_dates_pool(pool, Doc::objects().dates("created", DateKind::Month))
            .await
            .expect("dates(month)");
        let rendered: Vec<String> = months.iter().map(|d| d.to_string()).collect();
        assert_eq!(rendered, vec!["2024-03-01", "2025-07-01"]);

        let desc = fetch_dates_pool(
            pool,
            Doc::objects()
                .dates("created", DateKind::Month)
                .order_desc(true),
        )
        .await
        .expect("dates(month) desc");
        let rendered_desc: Vec<String> = desc.iter().map(|d| d.to_string()).collect();
        assert_eq!(rendered_desc, vec!["2025-07-01", "2024-03-01"]);
    }

    /// Django `.datetimes("created", "hour")` — distinct truncated
    /// timestamps.
    pub async fn check_datetimes_truncation(pool: &Pool) {
        let hours = fetch_datetimes_pool(
            pool,
            Doc::objects().datetimes("created", DateTimeKind::Hour),
        )
        .await
        .expect("datetimes(hour)");
        let rendered: Vec<String> = hours
            .iter()
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .collect();
        assert_eq!(
            rendered,
            vec!["2024-03-15 10:00", "2024-03-15 14:00", "2025-07-01 09:00"]
        );
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
            r#"DROP TABLE IF EXISTS "d6jd_doc" CASCADE"#,
            r#"CREATE TABLE "d6jd_doc" (
                "id" BIGINT PRIMARY KEY,
                "data" JSONB NOT NULL,
                "created" TIMESTAMPTZ NOT NULL
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

    pg_case!(check_nested_key_traversal);
    pg_case!(check_key_index_traversal);
    pg_case!(check_json_array_length);
    pg_case!(check_negative_index_dialect_matrix);
    pg_case!(check_date_transform_chains);
    pg_case!(check_quarter_dialect_matrix);
    pg_case!(check_dates_truncation);
    pg_case!(check_datetimes_truncation);
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
        sqlx::query(
            "CREATE TABLE d6jd_doc (
                id INTEGER PRIMARY KEY,
                data TEXT NOT NULL,
                created TEXT NOT NULL
            )",
        )
        .execute(&sq)
        .await
        .expect("ddl");
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

    sqlite_case!(check_nested_key_traversal);
    sqlite_case!(check_key_index_traversal);
    sqlite_case!(check_json_array_length);
    sqlite_case!(check_negative_index_dialect_matrix);
    sqlite_case!(check_date_transform_chains);
    sqlite_case!(check_quarter_dialect_matrix);
    sqlite_case!(check_dates_truncation);
    sqlite_case!(check_datetimes_truncation);
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
            "DROP TABLE IF EXISTS d6jd_doc",
            "CREATE TABLE d6jd_doc (
                id BIGINT PRIMARY KEY,
                data JSON NOT NULL,
                created TIMESTAMP NOT NULL
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

    mysql_case!(check_nested_key_traversal);
    mysql_case!(check_key_index_traversal);
    mysql_case!(check_json_array_length);
    mysql_case!(check_negative_index_dialect_matrix);
    mysql_case!(check_date_transform_chains);
    mysql_case!(check_quarter_dialect_matrix);
    mysql_case!(check_dates_truncation);
    mysql_case!(check_datetimes_truncation);
}
