//! Django 6.0 ORM parity — execution-based verification.
//! Scenario groups A (conditional aggregation) + F (grouping shapes).
//!
//! Each scenario body is shared across the three backends; the
//! cfg-gated modules at the bottom own pool construction + DDL and
//! call into `scenarios::check_*`. Where rustango diverges from
//! Django 6.0 on a given dialect, the scenario pins the *current*
//! error so the suite goes red the moment the gap is fixed (and the
//! parity audit row must then be updated).
//!
//! Django scenarios covered (docs.djangoproject.com/en/6.0):
//! - `aggregate(total=Count("id"), published=Count("id", filter=Q(...)))`
//! - `Sum("price", filter=Q(...), default=0)`
//! - `Count("category", distinct=True)`
//! - compound `Q` trees inside `filter=`
//! - `.values("category").annotate(n=Count("id"), max_price=Max("price"))`
//! - filter-on-annotation → HAVING routing (WHERE vs HAVING split)
//! - `.alias()` non-projected annotations
//! - `StdDev` (sample) — dialect support matrix

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use std::collections::HashMap;

    use rustango::core::aggregates::{count, count_all, count_distinct, max, stddev, sum};
    use rustango::core::{Column as _, Op, SqlValue};
    use rustango::sql::{Auto, ExecError, Pool, SqlError};
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6agg_post")]
    #[allow(dead_code)]
    pub struct Post {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 20)]
        pub status: String,
        pub is_active: bool,
        pub price: i64,
        #[rustango(max_length = 40)]
        pub category: String,
    }

    /// (status, is_active, price, category) — 6 rows, 2 categories.
    const SEED: [(&str, bool, i64, &str); 6] = [
        ("published", true, 100, "tech"),
        ("published", true, 200, "tech"),
        ("draft", true, 50, "tech"),
        ("published", false, 300, "life"),
        ("draft", false, 80, "life"),
        ("review", true, 120, "life"),
    ];

    pub async fn seed(pool: &Pool) {
        for (status, active, price, cat) in SEED {
            let mut p = Post {
                id: Auto::default(),
                status: status.into(),
                is_active: active,
                price,
                category: cat.into(),
            };
            p.save_pool(pool).await.expect("seed row");
        }
    }

    type Row = HashMap<String, SqlValue>;

    /// Aggregate outputs vary in numeric type by backend (PG `SUM` on
    /// BIGINT → NUMERIC/Decimal, MySQL → DECIMAL, SQLite → INTEGER) —
    /// normalize for assertions.
    fn as_i64(row: &Row, key: &str) -> i64 {
        match row.get(key) {
            Some(SqlValue::I64(n)) => *n,
            Some(SqlValue::I32(n)) => i64::from(*n),
            Some(SqlValue::F64(f)) => *f as i64,
            Some(SqlValue::Decimal(d)) => d
                .to_string()
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("unparseable decimal at `{key}`: {d}"))
                as i64,
            other => panic!("expected numeric at `{key}`, got {other:?}"),
        }
    }

    fn as_f64(row: &Row, key: &str) -> f64 {
        match row.get(key) {
            Some(SqlValue::F64(f)) => *f,
            Some(SqlValue::I64(n)) => *n as f64,
            Some(SqlValue::Decimal(d)) => d
                .to_string()
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("unparseable decimal at `{key}`: {d}")),
            other => panic!("expected numeric at `{key}`, got {other:?}"),
        }
    }

    fn as_str<'r>(row: &'r Row, key: &str) -> &'r str {
        match row.get(key) {
            Some(SqlValue::String(s)) => s,
            other => panic!("expected string at `{key}`, got {other:?}"),
        }
    }

    /// Django: `aggregate(total=Count("id"), published=Count("id",
    /// filter=Q(status="published")))` — total + conditional count in
    /// one round trip. PG/SQLite emit `FILTER (WHERE …)`; MySQL is
    /// rewritten to `COUNT(CASE WHEN … THEN id END)`.
    ///
    /// DIVERGENCE NOTE (Django 6.0 audit §26): bare
    /// `.aggregate().annotate(...)` is rustango's Shape-3 (GROUP BY
    /// every scalar column → per-row results, i.e. Django's
    /// `.annotate()`); Django's single-row `aggregate()` shape
    /// requires the explicit `.values(&[])` empty projection.
    pub async fn check_total_vs_filtered_count(pool: &Pool) {
        let rows = Post::objects()
            .aggregate()
            .values(&[])
            .annotate("total", count_all().into())
            .annotate(
                "published",
                count("id").filter(Post::status.eq("published")).into(),
            )
            .fetch(pool)
            .await
            .expect("filtered-count aggregate");
        assert_eq!(rows.len(), 1, "global aggregate is one row: {rows:?}");
        assert_eq!(as_i64(&rows[0], "total"), 6);
        assert_eq!(as_i64(&rows[0], "published"), 3);
    }

    /// Django: `Sum("price", filter=Q(status="archived"), default=0)`
    /// — no row matches, so the COALESCE default must surface instead
    /// of NULL. Pins the `COALESCE(SUM(...) FILTER (...), 0)` wrap
    /// order.
    pub async fn check_filtered_sum_default(pool: &Pool) {
        let rows = Post::objects()
            .aggregate()
            .values(&[])
            .annotate(
                "archived_revenue",
                sum("price")
                    .filter(Post::status.eq("archived"))
                    .default(0_i64)
                    .into(),
            )
            .fetch(pool)
            .await
            .expect("filtered sum with default");
        assert_eq!(rows.len(), 1);
        assert_eq!(as_i64(&rows[0], "archived_revenue"), 0);
    }

    /// Django: `Count("category", distinct=True)` — 6 rows over 2
    /// distinct categories.
    pub async fn check_count_distinct(pool: &Pool) {
        let rows = Post::objects()
            .aggregate()
            .values(&[])
            .annotate("cats", count_distinct("category").into())
            .fetch(pool)
            .await
            .expect("count distinct");
        assert_eq!(rows.len(), 1);
        assert_eq!(as_i64(&rows[0], "cats"), 2);
    }

    /// Compound boolean tree inside `filter=` — exercises the MySQL
    /// CASE-WHEN rewrite with nested AND/OR. Active AND (published OR
    /// review) → rows 1, 2, 6.
    pub async fn check_compound_filter_predicate(pool: &Pool) {
        let rows = Post::objects()
            .aggregate()
            .values(&[])
            .annotate(
                "n",
                count("id")
                    .filter(
                        Post::is_active
                            .eq(true)
                            .and(Post::status.eq("published").or(Post::status.eq("review"))),
                    )
                    .into(),
            )
            .fetch(pool)
            .await
            .expect("compound FILTER predicate");
        assert_eq!(as_i64(&rows[0], "n"), 3);
    }

    /// Django Shape 2: `.values("category").annotate(...)` → GROUP BY
    /// category. Both groups have 3 rows; max prices differ.
    pub async fn check_values_annotate_group_by(pool: &Pool) {
        let rows = Post::objects()
            .values(&["category"])
            .annotate("n", count_all().into())
            .annotate("max_price", max("price").into())
            .order_by(&[("category", false)])
            .fetch(pool)
            .await
            .expect("values().annotate() group by");
        assert_eq!(rows.len(), 2, "two categories: {rows:?}");
        assert_eq!(as_str(&rows[0], "category"), "life");
        assert_eq!(as_i64(&rows[0], "n"), 3);
        assert_eq!(as_i64(&rows[0], "max_price"), 300);
        assert_eq!(as_str(&rows[1], "category"), "tech");
        assert_eq!(as_i64(&rows[1], "n"), 3);
        assert_eq!(as_i64(&rows[1], "max_price"), 200);
    }

    /// WHERE vs HAVING routing: a filter on a real column lands in
    /// WHERE, a filter on the annotation alias lands in HAVING.
    /// Published-only per category: tech=2, life=1 → HAVING n >= 2
    /// keeps tech alone.
    pub async fn check_filter_on_annotation_routes_to_having(pool: &Pool) {
        let rows = Post::objects()
            .values(&["category"])
            .annotate("n", count_all().into())
            .filter("status", Op::Eq, "published")
            .filter("n", Op::Gte, 2_i64)
            .fetch(pool)
            .await
            .expect("WHERE + HAVING split");
        assert_eq!(rows.len(), 1, "only tech survives HAVING: {rows:?}");
        assert_eq!(as_str(&rows[0], "category"), "tech");
        assert_eq!(as_i64(&rows[0], "n"), 2);
    }

    /// Django 3.2+ `.alias()` — usable in filter/order_by but omitted
    /// from the SELECT projection.
    pub async fn check_alias_is_not_projected(pool: &Pool) {
        let rows = Post::objects()
            .values(&["category"])
            .alias("c", count_all().into())
            .filter("c", Op::Gte, 1_i64)
            .order_by(&[("c", true)])
            .fetch(pool)
            .await
            .expect("alias non-projected");
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(
                !row.contains_key("c"),
                "alias must not be projected: {row:?}"
            );
        }
    }

    /// `StdDev` (sample) dialect matrix: native on PG + MySQL 8;
    /// SQLite has no built-in stddev — rustango pins the documented
    /// rejection (matches Django, which also errors on SQLite).
    pub async fn check_stddev_dialect_matrix(pool: &Pool) {
        let res = Post::objects()
            .aggregate()
            .values(&[])
            .annotate("sd", stddev("price").into())
            .fetch(pool)
            .await;
        if pool.dialect().name() == "sqlite" {
            let err = res.expect_err("stddev must be rejected on sqlite");
            match err {
                ExecError::Sql(SqlError::AggregateNotSupported { aggregate, dialect }) => {
                    assert!(
                        aggregate.to_uppercase().contains("STDDEV"),
                        "unexpected aggregate name: {aggregate}"
                    );
                    assert_eq!(dialect, "sqlite");
                }
                other => panic!(
                    "expected AggregateNotSupported on sqlite, got {other:?} — \
                     if stddev now works there, update the Django 6.0 parity audit"
                ),
            }
        } else {
            let rows = res.expect("stddev on PG/MySQL");
            let sd = as_f64(&rows[0], "sd");
            // prices 100,200,50,300,80,120 → sample stddev ≈ 92.61
            assert!(
                (sd - 92.61).abs() < 0.5,
                "sample stddev of seed prices ≈ 92.61, got {sd}"
            );
        }
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
        sqlx::query(r#"DROP TABLE IF EXISTS "d6agg_post" CASCADE"#)
            .execute(&pg)
            .await
            .unwrap();
        sqlx::query(
            r#"CREATE TABLE "d6agg_post" (
                "id" BIGSERIAL PRIMARY KEY,
                "status" VARCHAR(20) NOT NULL,
                "is_active" BOOLEAN NOT NULL,
                "price" BIGINT NOT NULL,
                "category" VARCHAR(40) NOT NULL
            )"#,
        )
        .execute(&pg)
        .await
        .unwrap();
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

    pg_case!(check_total_vs_filtered_count);
    pg_case!(check_filtered_sum_default);
    pg_case!(check_count_distinct);
    pg_case!(check_compound_filter_predicate);
    pg_case!(check_values_annotate_group_by);
    pg_case!(check_filter_on_annotation_routes_to_having);
    pg_case!(check_alias_is_not_projected);
    pg_case!(check_stddev_dialect_matrix);
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
            "CREATE TABLE d6agg_post (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT NOT NULL,
                is_active INTEGER NOT NULL,
                price INTEGER NOT NULL,
                category TEXT NOT NULL
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

    sqlite_case!(check_total_vs_filtered_count);
    sqlite_case!(check_filtered_sum_default);
    sqlite_case!(check_count_distinct);
    sqlite_case!(check_compound_filter_predicate);
    sqlite_case!(check_values_annotate_group_by);
    sqlite_case!(check_filter_on_annotation_routes_to_having);
    sqlite_case!(check_alias_is_not_projected);
    sqlite_case!(check_stddev_dialect_matrix);
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
        sqlx::query("DROP TABLE IF EXISTS d6agg_post")
            .execute(&my)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE d6agg_post (
                id BIGINT AUTO_INCREMENT PRIMARY KEY,
                status VARCHAR(20) NOT NULL,
                is_active BOOLEAN NOT NULL,
                price BIGINT NOT NULL,
                category VARCHAR(40) NOT NULL
            )",
        )
        .execute(&my)
        .await
        .unwrap();
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

    mysql_case!(check_total_vs_filtered_count);
    mysql_case!(check_filtered_sum_default);
    mysql_case!(check_count_distinct);
    mysql_case!(check_compound_filter_predicate);
    mysql_case!(check_values_annotate_group_by);
    mysql_case!(check_filter_on_annotation_routes_to_having);
    mysql_case!(check_alias_is_not_projected);
    mysql_case!(check_stddev_dialect_matrix);
}
