//! Django 6.0 ORM parity — execution-based verification.
//! Scenario group E: set operations — union / union_all /
//! intersection / difference with per-branch and combined-result
//! ordering.
//!
//! Django scenarios covered (docs.djangoproject.com/en/6.0):
//! - `qs1.union(qs2)` dedups; `union(all=True)` keeps duplicates
//! - per-branch `ORDER BY`/`LIMIT` (each branch wraps in parens)
//! - `.order_by()/.limit()` AFTER `.union()` applies to the combined
//!   result (Django's documented compound semantics)
//! - `qs1.intersection(qs2)` / `qs1.difference(qs2)`
//!
//! Dialect floor: MySQL needs 8.0.31+ for native INTERSECT / EXCEPT
//! (docker-compose pins `mysql:8.0`, currently ≥ 8.0.31 — older
//! servers surface a driver syntax error; that floor is recorded in
//! the parity audit rather than branch-handled here).

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use rustango::core::SetOp;
    use rustango::sql::{raw_execute_pool, FetcherPool as _, Pool};
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6set_item")]
    #[allow(dead_code)]
    pub struct Item {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 40)]
        pub name: String,
        pub rnk: i64,
        #[rustango(max_length = 10)]
        pub grp: String,
    }

    pub async fn seed(pool: &Pool) {
        for sql in [
            "INSERT INTO d6set_item (id, name, rnk, grp) VALUES (1, 'a1', 1, 'a')",
            "INSERT INTO d6set_item (id, name, rnk, grp) VALUES (2, 'a2', 2, 'a')",
            "INSERT INTO d6set_item (id, name, rnk, grp) VALUES (3, 'a3', 3, 'a')",
            "INSERT INTO d6set_item (id, name, rnk, grp) VALUES (4, 'b1', 4, 'b')",
            "INSERT INTO d6set_item (id, name, rnk, grp) VALUES (5, 'b2', 5, 'b')",
        ] {
            raw_execute_pool(pool, sql, vec![]).await.expect("seed");
        }
    }

    fn sorted_names(rows: Vec<Item>) -> Vec<String> {
        let mut names: Vec<String> = rows.into_iter().map(|i| i.name).collect();
        names.sort();
        names
    }

    /// Branches overlap on the three 'a' rows: UNION dedups them,
    /// UNION ALL keeps both copies.
    pub async fn check_union_dedups_vs_union_all(pool: &Pool) {
        let union: Vec<Item> = Item::objects()
            .filter("grp", "a")
            .union(Item::objects().filter("rnk__lte", 4_i64))
            .fetch(pool)
            .await
            .expect("UNION fetch");
        assert_eq!(sorted_names(union), vec!["a1", "a2", "a3", "b1"]);

        let union_all: Vec<Item> = Item::objects()
            .filter("grp", "a")
            .union_all(Item::objects().filter("rnk__lte", 4_i64))
            .fetch(pool)
            .await
            .expect("UNION ALL fetch");
        assert_eq!(union_all.len(), 7, "3 + 4 with duplicates kept");
    }

    /// Per-branch ORDER BY + LIMIT (Django 4.0+ component-queryset
    /// slicing — `qs1[:2].union(qs2[:1])`). Both the head queryset and
    /// the argument branches carry their OWN parenthesized
    /// branch-scoped ORDER BY/LIMIT (#1032 + #1034):
    /// - #1032: each derived-table wrapper carries an alias
    ///   (`__rustango_bN`) so MySQL accepts it (error 1248 otherwise).
    /// - #1034: clauses set BEFORE the first set-op call scope to the
    ///   FIRST queryset; clauses set AFTER apply to the combined
    ///   result (see `check_outer_order_limit_on_combined`).
    pub async fn check_per_branch_order_and_limit(pool: &Pool) {
        // Other-branch clauses are branch-scoped: all of 'a' + top-1
        // of 'b' by rank desc. Works on every backend now that the
        // wrapper carries an alias (#1032).
        let rows: Vec<Item> = Item::objects()
            .filter("grp", "a")
            .union_all(
                Item::objects()
                    .filter("grp", "b")
                    .order_by(&[("rnk", true)])
                    .limit(1),
            )
            .fetch(pool)
            .await
            .expect("other-branch order/limit fetch");
        assert_eq!(sorted_names(rows), vec!["a1", "a2", "a3", "b2"]);

        // First-queryset clauses are now ALSO branch-scoped (#1034):
        // top-2 of 'a' by rank asc (a1, a2) PLUS all of 'b' (b1, b2) —
        // NOT "top-2 of the combined set".
        let rows: Vec<Item> = Item::objects()
            .filter("grp", "a")
            .order_by(&[("rnk", false)])
            .limit(2)
            .union_all(Item::objects().filter("grp", "b"))
            .fetch(pool)
            .await
            .expect("first-queryset order/limit fetch");
        assert_eq!(
            sorted_names(rows),
            vec!["a1", "a2", "b1", "b2"],
            "first-queryset LIMIT scopes to the FIRST branch (top-2 of 'a' + all of 'b')"
        );
    }

    /// Django: ordering/slicing applied AFTER `.union()` operates on
    /// the combined resultset.
    pub async fn check_outer_order_limit_on_combined(pool: &Pool) {
        let rows: Vec<Item> = Item::objects()
            .filter("grp", "a")
            .union(Item::objects().filter("grp", "b"))
            .order_by(&[("rnk", true)])
            .limit(3)
            .fetch(pool)
            .await
            .expect("outer order/limit fetch");
        let ranks: Vec<i64> = rows.into_iter().map(|i| i.rnk).collect();
        assert_eq!(ranks, vec![5, 4, 3], "top-3 of the COMBINED set, desc");
    }

    /// `qs.intersection(other)` — rows in both branches.
    pub async fn check_intersection(pool: &Pool) {
        let rows: Vec<Item> = Item::objects()
            .filter("grp", "a")
            .intersection(Item::objects().filter("rnk__lte", 1_i64))
            .fetch(pool)
            .await
            .expect("INTERSECT fetch (MySQL needs 8.0.31+)");
        assert_eq!(sorted_names(rows), vec!["a1"]);
    }

    /// `qs.difference(other)` — rows in the first branch only.
    pub async fn check_difference(pool: &Pool) {
        let rows: Vec<Item> = Item::objects()
            .filter("grp", "a")
            .difference(Item::objects().filter("rnk__lte", 2_i64))
            .fetch(pool)
            .await
            .expect("EXCEPT fetch (MySQL needs 8.0.31+)");
        assert_eq!(sorted_names(rows), vec!["a3"]);
    }

    /// `with_compound` — the fallible pre-compiled-branch form.
    pub async fn check_with_compound_precompiled_branch(pool: &Pool) {
        let branch = Item::objects()
            .filter("grp", "b")
            .compile()
            .expect("branch compile");
        let rows: Vec<Item> = Item::objects()
            .filter("grp", "a")
            .with_compound(SetOp::Union, branch)
            .fetch(pool)
            .await
            .expect("with_compound fetch");
        assert_eq!(rows.len(), 5);
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
            r#"DROP TABLE IF EXISTS "d6set_item" CASCADE"#,
            r#"CREATE TABLE "d6set_item" (
                "id" BIGINT PRIMARY KEY,
                "name" VARCHAR(40) NOT NULL,
                "rnk" BIGINT NOT NULL,
                "grp" VARCHAR(10) NOT NULL
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

    pg_case!(check_union_dedups_vs_union_all);
    pg_case!(check_per_branch_order_and_limit);
    pg_case!(check_outer_order_limit_on_combined);
    pg_case!(check_intersection);
    pg_case!(check_difference);
    pg_case!(check_with_compound_precompiled_branch);
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
            "CREATE TABLE d6set_item (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                rnk INTEGER NOT NULL,
                grp TEXT NOT NULL
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

    sqlite_case!(check_union_dedups_vs_union_all);
    sqlite_case!(check_per_branch_order_and_limit);
    sqlite_case!(check_outer_order_limit_on_combined);
    sqlite_case!(check_intersection);
    sqlite_case!(check_difference);
    sqlite_case!(check_with_compound_precompiled_branch);
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
            "DROP TABLE IF EXISTS d6set_item",
            "CREATE TABLE d6set_item (
                id BIGINT PRIMARY KEY,
                name VARCHAR(40) NOT NULL,
                rnk BIGINT NOT NULL,
                grp VARCHAR(10) NOT NULL
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

    mysql_case!(check_union_dedups_vs_union_all);
    mysql_case!(check_per_branch_order_and_limit);
    mysql_case!(check_outer_order_limit_on_combined);
    mysql_case!(check_intersection);
    mysql_case!(check_difference);
    mysql_case!(check_with_compound_precompiled_branch);
}
