//! `update()` / `delete()` honour `limit`, `offset` and `order_by` on
//! every backend (#1666). They used to drop them and touch every row.

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use rustango::core::QueryError;
    use rustango::sql::{delete_pool, FetcherPool as _, Pool, UpdaterPool as _};
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "bdml_item")]
    #[allow(dead_code)]
    pub struct Item {
        #[rustango(primary_key)]
        pub id: i64,
        pub rank: i64,
        #[rustango(max_length = 10)]
        pub tag: String,
    }

    async fn seed(pool: &Pool) {
        for (id, rank) in [(1, 50), (2, 10), (3, 40), (4, 20), (5, 30)] {
            Item {
                id,
                rank,
                tag: "new".into(),
            }
            .insert_pool(pool)
            .await
            .expect("seed");
        }
    }

    async fn ids(pool: &Pool) -> Vec<i64> {
        let rows: Vec<Item> = Item::objects()
            .order_by(&[("id", false)])
            .fetch(pool)
            .await
            .unwrap();
        rows.into_iter().map(|r| r.id).collect()
    }

    async fn ids_tagged(pool: &Pool, tag: &str) -> Vec<i64> {
        let rows: Vec<Item> = Item::objects()
            .filter("tag", tag)
            .order_by(&[("id", false)])
            .fetch(pool)
            .await
            .unwrap();
        rows.into_iter().map(|r| r.id).collect()
    }

    pub async fn check_limit_delete_removes_one(pool: &Pool) {
        seed(pool).await;
        let q = Item::objects().limit(1).compile_delete().unwrap();
        let n = delete_pool(pool, &q).await.expect("limited delete");
        assert_eq!(n, 1, "limit(1) deletes one row");
        assert_eq!(ids(pool).await.len(), 4);
    }

    pub async fn check_ordered_limited_delete(pool: &Pool) {
        seed(pool).await;
        // Two highest ranks are ids 1 (50) and 3 (40).
        let q = Item::objects()
            .order_by(&[("rank", true)])
            .limit(2)
            .compile_delete()
            .unwrap();
        assert_eq!(delete_pool(pool, &q).await.unwrap(), 2);
        assert_eq!(ids(pool).await, vec![2, 4, 5]);
    }

    pub async fn check_filtered_offset_delete(pool: &Pool) {
        seed(pool).await;
        // rank > 15 by rank asc: 4 (20), 5 (30), 3 (40), 1 (50); skip 2, take 1 -> id 3.
        let q = Item::objects()
            .filter("rank__gt", 15_i64)
            .order_by(&[("rank", false)])
            .limit(1)
            .offset(2)
            .compile_delete()
            .unwrap();
        assert_eq!(delete_pool(pool, &q).await.unwrap(), 1);
        assert_eq!(ids(pool).await, vec![1, 2, 4, 5]);
    }

    pub async fn check_offset_only_delete(pool: &Pool) {
        seed(pool).await;
        // Keep the two lowest ranks (ids 2, 4), delete the rest.
        let q = Item::objects()
            .order_by(&[("rank", false)])
            .offset(2)
            .compile_delete()
            .unwrap();
        assert_eq!(delete_pool(pool, &q).await.unwrap(), 3);
        assert_eq!(ids(pool).await, vec![2, 4]);
    }

    pub async fn check_ordered_limited_update(pool: &Pool) {
        seed(pool).await;
        // Two lowest ranks are ids 2 (10) and 4 (20).
        let n = Item::objects()
            .order_by(&[("rank", false)])
            .limit(2)
            .update()
            .set("tag", "hit")
            .execute_pool(pool)
            .await
            .expect("limited update");
        assert_eq!(n, 2);
        assert_eq!(ids_tagged(pool, "hit").await, vec![2, 4]);
    }

    pub async fn check_filtered_limited_update_binds(pool: &Pool) {
        seed(pool).await;
        // SET bind first, then the inner WHERE bind: placeholder order must hold.
        let n = Item::objects()
            .filter("rank__gte", 30_i64)
            .order_by(&[("rank", true)])
            .limit(1)
            .update()
            .set("tag", "top")
            .execute_pool(pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(ids_tagged(pool, "top").await, vec![1]);
    }

    pub async fn check_unbounded_paths_unchanged(pool: &Pool) {
        seed(pool).await;
        // order_by alone does not bound anything.
        let q = Item::objects()
            .filter("rank__lt", 25_i64)
            .order_by(&[("rank", true)])
            .compile_delete()
            .unwrap();
        assert_eq!(delete_pool(pool, &q).await.unwrap(), 2);
        let q = Item::objects().none().limit(3).compile_delete().unwrap();
        assert_eq!(delete_pool(pool, &q).await.unwrap(), 0);
        assert_eq!(ids(pool).await, vec![1, 3, 5]);
    }

    pub async fn check_set_operation_bound_rejected(pool: &Pool) {
        seed(pool).await;
        let err = Item::objects()
            .filter("id", 1_i64)
            .union(Item::objects().filter("id", 2_i64))
            .limit(1)
            .compile_delete()
            .unwrap_err();
        assert!(
            matches!(err, QueryError::BoundedDmlUnsupported { .. }),
            "got {err:?}"
        );
        assert_eq!(ids(pool).await.len(), 5);
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
        let pg = sqlx::PgPool::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}"));
        for sql in [
            r#"DROP TABLE IF EXISTS "bdml_item" CASCADE"#,
            r#"CREATE TABLE "bdml_item" (
                "id" BIGINT PRIMARY KEY,
                "rank" BIGINT NOT NULL,
                "tag" VARCHAR(10) NOT NULL
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
                    eprintln!("DATABASE_URL not set — skipping the PG arm of this scenario");
                    return;
                };
                scenarios::$name(&pool).await;
            }
        };
    }

    pg_case!(check_limit_delete_removes_one);
    pg_case!(check_ordered_limited_delete);
    pg_case!(check_filtered_offset_delete);
    pg_case!(check_offset_only_delete);
    pg_case!(check_ordered_limited_update);
    pg_case!(check_filtered_limited_update_binds);
    pg_case!(check_unbounded_paths_unchanged);
    pg_case!(check_set_operation_bound_rejected);
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
            "CREATE TABLE bdml_item (
                id INTEGER PRIMARY KEY,
                rank INTEGER NOT NULL,
                tag TEXT NOT NULL
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
                scenarios::$name(&pool).await;
            }
        };
    }

    sqlite_case!(check_limit_delete_removes_one);
    sqlite_case!(check_ordered_limited_delete);
    sqlite_case!(check_filtered_offset_delete);
    sqlite_case!(check_offset_only_delete);
    sqlite_case!(check_ordered_limited_update);
    sqlite_case!(check_filtered_limited_update_binds);
    sqlite_case!(check_unbounded_paths_unchanged);
    sqlite_case!(check_set_operation_bound_rejected);
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
        let my = sqlx::MySqlPool::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("MYSQL_TEST_URL is set but unreachable ({url}): {e}"));
        for sql in [
            "DROP TABLE IF EXISTS bdml_item",
            "CREATE TABLE bdml_item (
                id BIGINT PRIMARY KEY,
                `rank` BIGINT NOT NULL,
                tag VARCHAR(10) NOT NULL
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
                    eprintln!("MYSQL_TEST_URL unset — skipping the MySQL arm of this scenario");
                    return;
                };
                scenarios::$name(&pool).await;
            }
        };
    }

    mysql_case!(check_limit_delete_removes_one);
    mysql_case!(check_ordered_limited_delete);
    mysql_case!(check_filtered_offset_delete);
    mysql_case!(check_offset_only_delete);
    mysql_case!(check_ordered_limited_update);
    mysql_case!(check_filtered_limited_update_binds);
    mysql_case!(check_unbounded_paths_unchanged);
    mysql_case!(check_set_operation_bound_rejected);
}
