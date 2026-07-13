//! Django 6.0 ORM parity — execution-based verification.
//! Scenario group C (window functions) + K (distinct_on interplay).
//!
//! Django scenarios covered (docs.djangoproject.com/en/6.0):
//! - `Window(Rank(), partition_by=..., order_by=...)` (+ dense_rank,
//!   ntile, lag-with-default, first_value with a ROWS frame)
//! - aggregates as window expressions: `Window(Sum("points"), …)` —
//!   partitioned running total via `sum_over` (#1035 Part A)
//! - `.distinct("tenant_id")` first-row-per-group (PG `DISTINCT ON`
//!   native; MySQL/SQLite via the ROW_NUMBER fallback)
//! - top-N-per-group: Django filters on a window annotation via an
//!   outer queryset; rustango's equivalent is `join_lateral`
//!   (PG + MySQL 8.0.14+ only — pinned `LateralJoinNotSupported` on
//!   SQLite)
//!
//! AUDIT NOTES (compile-time API absences, no runtime pin possible):
//! - ERGONOMICS DIVERGENCE (execution-verified): Django annotates a
//!   window onto a plain queryset; rustango requires the explicit
//!   `.aggregate().group_by(<every projected column>)` shape —
//!   `.values(cols)` + window-only annotate is rejected with
//!   `QueryError::ValuesRequiresAggregate`.
//! - A windowed query can now be re-embedded as a subquery source (#1035):
//!   `join_sub` / `left_join_sub` accept an `AggregateQuery` (what a window
//!   compiles to) via `impl Into<DerivedSource>`, so Django's
//!   `qs.annotate(rank=Window(...))` then `.filter(rank__lte=N)` outer-wrap
//!   has a direct equivalent — see
//!   `subquery_join_sqlite_live::window_derived_table_yields_top_n_per_group`.
//!   `join_lateral` remains an alternative (PG/MySQL only).

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use std::collections::HashMap;

    use rustango::core::joins::aliased;
    use rustango::core::window::{
        dense_rank, first_value, lag, ntile, rank, sum_over, FrameBoundary, FrameKind, WindowFrame,
    };
    use rustango::core::{Join, JoinKind, Model as _, Op, SqlValue, WhereExpr};
    use rustango::sql::{raw_execute_pool, Auto, ExecError, FetcherPool as _, Pool, SqlError};
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6win_tenant")]
    #[allow(dead_code)]
    pub struct Tenant {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 40)]
        pub name: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6win_score")]
    #[allow(dead_code)]
    pub struct Score {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        pub tenant_id: i64,
        #[rustango(max_length = 40)]
        pub player: String,
        pub points: i64,
        pub day: chrono::NaiveDate,
    }

    /// Tenant 1: alice 30→35 over two days, bob 20→15, carol 10.
    /// Tenant 2: dave 100, eve 50, frank 50 (tie), gina 25.
    pub async fn seed(pool: &Pool) {
        for sql in [
            "INSERT INTO d6win_tenant (id, name) VALUES (1, 'acme')",
            "INSERT INTO d6win_tenant (id, name) VALUES (2, 'globex')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (1, 1, 'alice', 30, '2026-01-01')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (2, 1, 'alice', 35, '2026-01-02')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (3, 1, 'bob', 20, '2026-01-01')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (4, 1, 'bob', 15, '2026-01-02')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (5, 1, 'carol', 10, '2026-01-01')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (6, 2, 'dave', 100, '2026-01-01')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (7, 2, 'eve', 50, '2026-01-01')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (8, 2, 'frank', 50, '2026-01-01')",
            "INSERT INTO d6win_score (id, tenant_id, player, points, day) VALUES (9, 2, 'gina', 25, '2026-01-01')",
        ] {
            raw_execute_pool(pool, sql, vec![]).await.expect("seed");
        }
    }

    type Row = HashMap<String, SqlValue>;

    fn as_i64(row: &Row, key: &str) -> i64 {
        match row.get(key) {
            Some(SqlValue::I64(n)) => *n,
            Some(SqlValue::I32(n)) => i64::from(*n),
            // SUM over a BIGINT column widens per dialect: PG → NUMERIC,
            // MySQL → DECIMAL, SQLite → INTEGER. Normalize for assertions.
            Some(SqlValue::F64(f)) => *f as i64,
            Some(SqlValue::Decimal(d)) => d
                .to_string()
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("unparseable decimal at `{key}`: {d}"))
                as i64,
            other => panic!("expected integer at `{key}`, got {other:?}"),
        }
    }

    /// `Window(Rank(), partition_by="tenant_id", order_by="-points")`
    /// — full table, both partitions in one pass.
    pub async fn check_rank_partitioned(pool: &Pool) {
        let rows = Score::objects()
            .aggregate()
            .group_by("tenant_id")
            .group_by("player")
            .group_by("points")
            .annotate(
                "r",
                rank()
                    .partition_by("tenant_id")
                    .order_by(&[("points", true)])
                    .into(),
            )
            .order_by(&[("tenant_id", false), ("points", true), ("player", false)])
            .fetch(pool)
            .await
            .expect("rank window");
        assert_eq!(rows.len(), 9);
        let ranks: Vec<i64> = rows.iter().map(|r| as_i64(r, "r")).collect();
        // t1: 35,30,20,15,10 → 1..5; t2: 100,50,50,25 → 1,2,2,4 (RANK skips).
        assert_eq!(ranks, vec![1, 2, 3, 4, 5, 1, 2, 2, 4]);
    }

    /// DENSE_RANK on the tied partition — no rank skipping (1,2,2,3).
    pub async fn check_dense_rank_ties(pool: &Pool) {
        let rows = Score::objects()
            .aggregate()
            .group_by("player")
            .group_by("points")
            .annotate("d", dense_rank().order_by(&[("points", true)]).into())
            .filter("tenant_id", Op::Eq, 2_i64)
            .order_by(&[("points", true), ("player", false)])
            .fetch(pool)
            .await
            .expect("dense_rank window");
        let dense: Vec<i64> = rows.iter().map(|r| as_i64(r, "d")).collect();
        assert_eq!(dense, vec![1, 2, 2, 3]);
    }

    /// `Lag("points", default=0)` partitioned per player — Django's
    /// previous-row navigation with an out-of-range default.
    pub async fn check_lag_with_default(pool: &Pool) {
        let rows = Score::objects()
            .aggregate()
            .group_by("player")
            .group_by("points")
            .group_by("day")
            .annotate(
                "prev",
                lag("points", 1, Some(SqlValue::I64(0)))
                    .partition_by("player")
                    .order_by(&[("day", false)])
                    .into(),
            )
            .filter(
                "player",
                Op::In,
                SqlValue::List(vec![
                    SqlValue::String("alice".into()),
                    SqlValue::String("bob".into()),
                ]),
            )
            .order_by(&[("player", false), ("day", false)])
            .fetch(pool)
            .await
            .expect("lag window");
        let prev: Vec<i64> = rows.iter().map(|r| as_i64(r, "prev")).collect();
        // alice d1 (no prev → 0), alice d2 (prev 30), bob d1 → 0, bob d2 → 20.
        assert_eq!(prev, vec![0, 30, 0, 20]);
    }

    /// FIRST_VALUE with an explicit ROWS frame — every row sees its
    /// player's first-day points.
    pub async fn check_first_value_rows_frame(pool: &Pool) {
        let rows = Score::objects()
            .aggregate()
            .group_by("player")
            .group_by("points")
            .group_by("day")
            .annotate(
                "first_pts",
                first_value("points")
                    .partition_by("player")
                    .order_by(&[("day", false)])
                    .frame(WindowFrame {
                        kind: FrameKind::Rows,
                        start: FrameBoundary::UnboundedPreceding,
                        end: Some(FrameBoundary::CurrentRow),
                    })
                    .into(),
            )
            .filter(
                "player",
                Op::In,
                SqlValue::List(vec![
                    SqlValue::String("alice".into()),
                    SqlValue::String("bob".into()),
                ]),
            )
            .order_by(&[("player", false), ("day", false)])
            .fetch(pool)
            .await
            .expect("first_value window");
        let firsts: Vec<i64> = rows.iter().map(|r| as_i64(r, "first_pts")).collect();
        assert_eq!(firsts, vec![30, 30, 20, 20]);
    }

    /// `Ntile(2)` halves tenant 1's five rows into buckets 1,1,1,2,2.
    pub async fn check_ntile_buckets(pool: &Pool) {
        let rows = Score::objects()
            .aggregate()
            .group_by("player")
            .group_by("points")
            .annotate("bucket", ntile(2).order_by(&[("points", true)]).into())
            .filter("tenant_id", Op::Eq, 1_i64)
            .order_by(&[("points", true)])
            .fetch(pool)
            .await
            .expect("ntile window");
        let buckets: Vec<i64> = rows.iter().map(|r| as_i64(r, "bucket")).collect();
        assert_eq!(buckets, vec![1, 1, 1, 2, 2]);
    }

    /// Django 6.0 `Window(Sum("points"), partition_by="player",
    /// order_by="day")` — a partitioned running total (#1035 Part A).
    /// Tenant 1, per player by day: the cumulative points carry the
    /// default `RANGE UNBOUNDED PRECEDING .. CURRENT ROW` frame.
    pub async fn check_sum_over_running_total(pool: &Pool) {
        let rows = Score::objects()
            .aggregate()
            .group_by("player")
            .group_by("day")
            .group_by("points")
            .annotate(
                "running",
                sum_over("points")
                    .partition_by("player")
                    .order_by(&[("day", false)])
                    .into(),
            )
            .filter("tenant_id", Op::Eq, 1_i64)
            .order_by(&[("player", false), ("day", false)])
            .fetch(pool)
            .await
            .expect("sum-over running total");
        let running: Vec<i64> = rows.iter().map(|r| as_i64(r, "running")).collect();
        // alice 30→(30,65), bob 20→(20,35), carol 10→(10).
        assert_eq!(running, vec![30, 65, 20, 35, 10]);
    }

    /// Django `.distinct("tenant_id")` + ordering — best score row per
    /// tenant. PG runs native `DISTINCT ON`; MySQL/SQLite go through
    /// the ROW_NUMBER fallback. Identical rows on all three.
    pub async fn check_distinct_on_first_row_per_tenant(pool: &Pool) {
        let rows: Vec<Score> = Score::objects()
            .distinct_on(&["tenant_id"])
            .order_by(&[("tenant_id", false), ("points", true)])
            .fetch(pool)
            .await
            .expect("distinct_on fetch");
        let players: Vec<(i64, String, i64)> = rows
            .into_iter()
            .map(|s| (s.tenant_id, s.player, s.points))
            .collect();
        assert_eq!(
            players,
            vec![(1, "alice".into(), 35), (2, "dave".into(), 100)]
        );
    }

    /// `distinct_on` + a join now works on every backend (#1039): PG runs
    /// native `DISTINCT ON`; MySQL/SQLite go through the ROW_NUMBER
    /// fallback, which emits the join inside the windowed subquery and
    /// qualifies the model's own columns so they don't collide with the
    /// joined table. Both tenants have a matching `Tenant`, so the INNER
    /// JOIN drops nobody → same shape as the no-join distinct_on.
    pub async fn check_distinct_on_with_join_dialect_matrix(pool: &Pool) {
        let rows: Vec<Score> = Score::objects()
            .distinct_on(&["tenant_id"])
            .order_by(&[("tenant_id", false), ("points", true)])
            .join(Join {
                target: Tenant::SCHEMA,
                alias: "t",
                // Inside an `on` predicate bare columns resolve to the
                // joined alias — the outer side is explicitly aliased.
                kind: JoinKind::Inner,
                on: WhereExpr::ExprCompare {
                    lhs: aliased("t", "id"),
                    op: Op::Eq,
                    rhs: aliased("d6win_score", "tenant_id"),
                },
                project: vec![],
            })
            .fetch(pool)
            .await
            .expect("distinct_on + join on every backend");
        let got: Vec<(i64, String, i64)> = rows
            .into_iter()
            .map(|s| (s.tenant_id, s.player, s.points))
            .collect();
        assert_eq!(
            got,
            vec![(1, "alice".into(), 35), (2, "dave".into(), 100)],
            "best-scoring row per tenant, with the tenant join applied"
        );
    }

    /// Django top-N-per-group (`annotate(rank=Window(...))` +
    /// outer-filter) — rustango's equivalent is a correlated LATERAL
    /// join: top-2 scores for each tenant. PG + MySQL 8.0.14+ execute;
    /// SQLite pins `LateralJoinNotSupported`.
    pub async fn check_top_n_per_group_via_lateral(pool: &Pool) {
        let top2 = Score::objects()
            .where_raw(WhereExpr::ExprCompare {
                lhs: aliased("d6win_score", "tenant_id"),
                op: Op::Eq,
                rhs: aliased("d6win_tenant", "id"),
            })
            .order_by(&[("points", true)])
            .limit(2)
            .compile()
            .expect("inner compile");
        let res = Tenant::objects()
            .join_lateral(top2, "top", WhereExpr::And(vec![]))
            .fetch(pool)
            .await;
        if pool.dialect().name() == "sqlite" {
            let err = res
                .map(|rows: Vec<Tenant>| rows.len())
                .expect_err("LATERAL must be rejected on sqlite");
            match err {
                ExecError::Sql(SqlError::LateralJoinNotSupported { dialect }) => {
                    assert_eq!(dialect, "sqlite");
                }
                other => panic!(
                    "expected LateralJoinNotSupported on sqlite, got {other:?} — \
                     if LATERAL now works there, update the Django 6.0 parity audit"
                ),
            }
        } else {
            let rows: Vec<Tenant> = res.expect("LATERAL top-2 per tenant");
            // 2 tenants × top-2 lateral rows each → 4 joined rows.
            assert_eq!(rows.len(), 4, "top-2 per tenant via LATERAL");
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
        for sql in [
            r#"DROP TABLE IF EXISTS "d6win_score" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6win_tenant" CASCADE"#,
            r#"CREATE TABLE "d6win_tenant" (
                "id" BIGINT PRIMARY KEY,
                "name" VARCHAR(40) NOT NULL
            )"#,
            r#"CREATE TABLE "d6win_score" (
                "id" BIGINT PRIMARY KEY,
                "tenant_id" BIGINT NOT NULL,
                "player" VARCHAR(40) NOT NULL,
                "points" BIGINT NOT NULL,
                "day" DATE NOT NULL
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

    pg_case!(check_rank_partitioned);
    pg_case!(check_dense_rank_ties);
    pg_case!(check_lag_with_default);
    pg_case!(check_first_value_rows_frame);
    pg_case!(check_ntile_buckets);
    pg_case!(check_sum_over_running_total);
    pg_case!(check_distinct_on_first_row_per_tenant);
    pg_case!(check_distinct_on_with_join_dialect_matrix);
    pg_case!(check_top_n_per_group_via_lateral);
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
            "CREATE TABLE d6win_tenant (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            )",
            "CREATE TABLE d6win_score (
                id INTEGER PRIMARY KEY,
                tenant_id INTEGER NOT NULL,
                player TEXT NOT NULL,
                points INTEGER NOT NULL,
                day TEXT NOT NULL
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

    sqlite_case!(check_rank_partitioned);
    sqlite_case!(check_dense_rank_ties);
    sqlite_case!(check_lag_with_default);
    sqlite_case!(check_first_value_rows_frame);
    sqlite_case!(check_ntile_buckets);
    sqlite_case!(check_sum_over_running_total);
    sqlite_case!(check_distinct_on_first_row_per_tenant);
    sqlite_case!(check_distinct_on_with_join_dialect_matrix);
    sqlite_case!(check_top_n_per_group_via_lateral);
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
            "DROP TABLE IF EXISTS d6win_score",
            "DROP TABLE IF EXISTS d6win_tenant",
            "CREATE TABLE d6win_tenant (
                id BIGINT PRIMARY KEY,
                name VARCHAR(40) NOT NULL
            )",
            "CREATE TABLE d6win_score (
                id BIGINT PRIMARY KEY,
                tenant_id BIGINT NOT NULL,
                player VARCHAR(40) NOT NULL,
                points BIGINT NOT NULL,
                day DATE NOT NULL
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

    mysql_case!(check_rank_partitioned);
    mysql_case!(check_dense_rank_ties);
    mysql_case!(check_lag_with_default);
    mysql_case!(check_first_value_rows_frame);
    mysql_case!(check_ntile_buckets);
    mysql_case!(check_sum_over_running_total);
    mysql_case!(check_distinct_on_first_row_per_tenant);
    mysql_case!(check_distinct_on_with_join_dialect_matrix);
    mysql_case!(check_top_n_per_group_via_lateral);
}
