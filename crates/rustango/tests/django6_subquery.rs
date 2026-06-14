//! Django 6.0 ORM parity — execution-based verification.
//! Scenario group B: correlated subqueries — `Exists` / `OuterRef` /
//! `Subquery` / `IN (SELECT …)` and Django's exclude-on-multi-valued-
//! relation semantics.
//!
//! Django scenarios covered (docs.djangoproject.com/en/6.0):
//! - `filter(Exists(Book.objects.filter(author=OuterRef("pk"))))`
//! - the relation-spanning exclude trap: `exclude(books__published=False)`
//!   means "authors with NO unpublished book" (NOT EXISTS), not
//!   "authors having some book that isn't unpublished"
//! - `filter(author__in=Subquery(...))` → `where_in_subquery`
//! - count comparator over a relation (`annotate(Count) + filter` /
//!   Eloquent `has('books', '>=', 2)`)
//! - `annotate(books_count=Count("books"))` eager count column
//! - scalar `Subquery()` embedded in a WHERE comparison
//! - scalar `Subquery()` projected as an annotation column
//!   (`annotate(newest=Subquery(...))`) via `annotate_subquery` /
//!   `scalar_subquery` (#1036)
//! - `OuterRef` outside a subquery is a programming error (pinned)
//!
//! AUDIT NOTES (compile-time API absences, no runtime pin possible):
//! - Composite `(a, b) IN (SELECT …)` tuple-membership has no API;
//!   the workaround is a correlated `EXISTS` with a multi-predicate
//!   WHERE (exactly what `where_has_filter` emits — see
//!   `check_exclude_relation_spanning_semantics`).

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use rustango::core::subquery::{outer_ref, subquery};
    use rustango::core::{Expr, Op, SqlValue, WhereExpr};
    use rustango::sql::{
        raw_execute_pool, Auto, ExecError, FetcherPool as _, ForeignKey, Pool, SqlError,
    };
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(
        table = "d6sub_author",
        reverse_has(name = "books", child = "Book", child_fk_column = "author_id",)
    )]
    #[allow(dead_code)]
    pub struct Author {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 60)]
        pub name: String,
        pub active: bool,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6sub_book")]
    #[allow(dead_code)]
    pub struct Book {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        pub author_id: ForeignKey<Author, i64>,
        #[rustango(max_length = 120)]
        pub title: String,
        pub published: bool,
    }

    /// Ada(active): 1 published + 1 draft. Bob(inactive): 1 published.
    /// Cara(active): no books. Dan(active): 2 drafts + 1 published.
    pub async fn seed(pool: &Pool) {
        for sql in [
            "INSERT INTO d6sub_author (id, name, active) VALUES (1, 'Ada', TRUE)",
            "INSERT INTO d6sub_author (id, name, active) VALUES (2, 'Bob', FALSE)",
            "INSERT INTO d6sub_author (id, name, active) VALUES (3, 'Cara', TRUE)",
            "INSERT INTO d6sub_author (id, name, active) VALUES (4, 'Dan', TRUE)",
            "INSERT INTO d6sub_book (id, author_id, title, published) VALUES (1, 1, 'Rust 101', TRUE)",
            "INSERT INTO d6sub_book (id, author_id, title, published) VALUES (2, 1, 'Drafts', FALSE)",
            "INSERT INTO d6sub_book (id, author_id, title, published) VALUES (3, 2, 'SQL Bits', TRUE)",
            "INSERT INTO d6sub_book (id, author_id, title, published) VALUES (4, 4, 'D1', FALSE)",
            "INSERT INTO d6sub_book (id, author_id, title, published) VALUES (5, 4, 'D2', FALSE)",
            "INSERT INTO d6sub_book (id, author_id, title, published) VALUES (6, 4, 'D3', TRUE)",
        ] {
            raw_execute_pool(pool, sql, vec![]).await.expect("seed");
        }
    }

    fn names(rows: Vec<Author>) -> Vec<String> {
        rows.into_iter().map(|a| a.name).collect()
    }

    /// Django: `Author.objects.filter(Exists(Book.objects.filter(
    /// author=OuterRef("pk"))))` — hand-built correlated EXISTS via
    /// `outer_ref` + `where_exists`.
    pub async fn check_exists_with_outer_ref(pool: &Pool) {
        let inner = Book::objects()
            .where_raw(WhereExpr::ExprCompare {
                lhs: Expr::Column("author_id"),
                op: Op::Eq,
                rhs: outer_ref("id"),
            })
            .compile()
            .expect("inner compile");
        let rows: Vec<Author> = Author::objects()
            .where_exists(inner)
            .order_by(&[("name", false)])
            .fetch_pool(pool)
            .await
            .expect("EXISTS fetch");
        assert_eq!(names(rows), vec!["Ada", "Bob", "Dan"]);
    }

    /// Same scenario through the typed-column form
    /// (`Book::author_id.eq_expr(outer_ref("id"))`) and negated via
    /// `where_not_exists` — Django `~Exists(...)`.
    pub async fn check_not_exists_typed_outer_ref(pool: &Pool) {
        use rustango::core::Column as _;
        let inner = Book::objects()
            .where_(Book::author_id.eq_expr(outer_ref("id")))
            .compile()
            .expect("inner compile");
        let rows: Vec<Author> = Author::objects()
            .where_not_exists(inner)
            .fetch_pool(pool)
            .await
            .expect("NOT EXISTS fetch");
        assert_eq!(names(rows), vec!["Cara"]);
    }

    /// The Django relation-spanning exclude trap. With Ada holding
    /// BOTH a published and an unpublished book:
    /// - `filter(books__published=False)` keeps authors with at least
    ///   one unpublished book → Ada, Dan (rustango:
    ///   `where_has_filter`).
    /// - `exclude(books__published=False)` keeps authors with NO
    ///   unpublished book at all → Bob, Cara — *including* bookless
    ///   Cara (rustango: `where_doesnt_have_filter` emits the same
    ///   NOT EXISTS Django lowers to). String-keyed `exclude()` itself
    ///   is missing despite the audit claim — issue #1030.
    pub async fn check_exclude_relation_spanning_semantics(pool: &Pool) {
        let unpublished = || {
            Book::objects()
                .filter("published", false)
                .compile()
                .expect("inner compile")
        };
        let has: Vec<Author> = Author::objects()
            .where_has_filter("books", unpublished())
            .order_by(&[("name", false)])
            .fetch_pool(pool)
            .await
            .expect("whereHas fetch");
        assert_eq!(names(has), vec!["Ada", "Dan"], "filter() direction");

        let doesnt: Vec<Author> = Author::objects()
            .where_doesnt_have_filter("books", unpublished())
            .order_by(&[("name", false)])
            .fetch_pool(pool)
            .await
            .expect("whereDoesntHave fetch");
        assert_eq!(
            names(doesnt),
            vec!["Bob", "Cara"],
            "exclude() direction must be NOT EXISTS — bookless Cara included"
        );
    }

    /// Django: `Book.objects.filter(author__in=Author.objects.filter(
    /// active=True).values("id"))` → `IN (SELECT id FROM …)`.
    pub async fn check_in_subquery(pool: &Pool) {
        let active_ids = Author::objects()
            .filter("active", true)
            .values_list_flat("id")
            .compile()
            .expect("inner compile");
        let rows: Vec<Book> = Book::objects()
            .where_in_subquery("author_id", active_ids)
            .fetch_pool(pool)
            .await
            .expect("IN subquery fetch");
        assert_eq!(rows.len(), 5, "Ada's 2 + Dan's 3 books");

        let active_ids = Author::objects()
            .filter("active", true)
            .values_list_flat("id")
            .compile()
            .expect("inner compile");
        let rows: Vec<Book> = Book::objects()
            .where_not_in_subquery("author_id", active_ids)
            .fetch_pool(pool)
            .await
            .expect("NOT IN subquery fetch");
        assert_eq!(rows.len(), 1, "only Bob's book");
    }

    /// Django: `annotate(n=Count("books")).filter(n__gte=2)` /
    /// Eloquent `has('books', '>=', 2)` — correlated COUNT comparator.
    pub async fn check_where_has_count(pool: &Pool) {
        let rows: Vec<Author> = Author::objects()
            .where_has_count("books", Op::Gte, 2)
            .order_by(&[("name", false)])
            .fetch_pool(pool)
            .await
            .expect("has-count fetch");
        assert_eq!(names(rows), vec!["Ada", "Dan"]);
    }

    /// Django: `annotate(books_count=Count("books"))` — projected
    /// eager count. rustango lowers to a correlated scalar subquery
    /// (never a JOIN), so it can't double-count.
    pub async fn check_annotate_count(pool: &Pool) {
        let rows = Author::objects()
            .annotate_count("books")
            .order_by(&[("name", false)])
            .fetch(pool)
            .await
            .expect("annotate_count fetch");
        assert_eq!(rows.len(), 4);
        let counts: Vec<i64> = rows
            .iter()
            .map(|r| match r.get("books_count") {
                Some(SqlValue::I64(n)) => *n,
                other => panic!("expected I64 books_count, got {other:?}"),
            })
            .collect();
        assert_eq!(counts, vec![2, 1, 0, 3], "Ada, Bob, Cara, Dan");
    }

    /// Django: `filter(author=Subquery(Author.objects.order_by(
    /// "-id").values("id")[:1]))` — scalar subquery embedded in a
    /// WHERE comparison. Newest author by id is Dan (id 4).
    pub async fn check_scalar_subquery_in_where(pool: &Pool) {
        let newest_author = Author::objects()
            .order_by(&[("id", true)])
            .limit(1)
            .values_list_flat("id")
            .compile()
            .expect("inner compile");
        let rows: Vec<Book> = Book::objects()
            .where_raw(WhereExpr::ExprCompare {
                lhs: Expr::Column("author_id"),
                op: Op::Eq,
                rhs: subquery(newest_author),
            })
            .fetch_pool(pool)
            .await
            .expect("scalar subquery fetch");
        assert_eq!(rows.len(), 3, "Dan's 3 books");
    }

    /// Django: `Author.objects.annotate(newest=Subquery(
    /// Book.objects.filter(author=OuterRef("pk")).order_by("-id")
    /// .values("title")[:1]))` — a correlated scalar subquery projected
    /// as a column (#1036). Each author's newest book title; bookless
    /// Cara projects NULL. Lowers through `RelatedAggregate` — same
    /// per-row scalar path as `annotate_count`, no JOIN, no writer
    /// changes.
    pub async fn check_scalar_subquery_annotation(pool: &Pool) {
        use rustango::core::Column as _;
        let newest = Book::objects()
            .where_(Book::author_id.eq_expr(outer_ref("id")))
            .order_by(&[("id", true)])
            .limit(1)
            .values_list_flat("title")
            .compile()
            .expect("inner compile");
        let rows = Author::objects()
            .annotate_subquery("newest", newest)
            .order_by(&[("name", false)])
            .fetch(pool)
            .await
            .expect("scalar-subquery annotation fetch");
        assert_eq!(rows.len(), 4);
        let titles: Vec<Option<String>> = rows
            .iter()
            .map(|r| match r.get("newest") {
                Some(SqlValue::String(s)) => Some(s.clone()),
                Some(SqlValue::Null) | None => None,
                other => panic!("expected String/Null newest, got {other:?}"),
            })
            .collect();
        assert_eq!(
            titles,
            vec![
                Some("Drafts".to_owned()),   // Ada — newest of {Rust 101, Drafts}
                Some("SQL Bits".to_owned()), // Bob
                None,                        // Cara — no books → NULL
                Some("D3".to_owned()),       // Dan — newest of {D1, D2, D3}
            ],
            "newest book title per author (Ada, Bob, Cara, Dan)"
        );
    }

    /// `OuterRef` outside any subquery wrapper is a programming error
    /// — pinned: the writer rejects it with a clear error instead of
    /// emitting broken SQL (Django raises ValueError at evaluation).
    pub async fn check_outer_ref_outside_subquery_errors(pool: &Pool) {
        let err = Author::objects()
            .where_raw(WhereExpr::ExprCompare {
                lhs: Expr::Column("id"),
                op: Op::Eq,
                rhs: outer_ref("id"),
            })
            .fetch_pool(pool)
            .await
            .map(|rows: Vec<Author>| rows.len())
            .expect_err("OuterRef outside a subquery must error");
        match err {
            ExecError::Sql(SqlError::OuterRefOutsideSubquery { column }) => {
                assert_eq!(column, "id");
            }
            other => panic!("expected OuterRefOutsideSubquery, got {other:?}"),
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
            r#"DROP TABLE IF EXISTS "d6sub_book" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6sub_author" CASCADE"#,
            r#"CREATE TABLE "d6sub_author" (
                "id" BIGINT PRIMARY KEY,
                "name" VARCHAR(60) NOT NULL,
                "active" BOOLEAN NOT NULL
            )"#,
            r#"CREATE TABLE "d6sub_book" (
                "id" BIGINT PRIMARY KEY,
                "author_id" BIGINT NOT NULL,
                "title" VARCHAR(120) NOT NULL,
                "published" BOOLEAN NOT NULL
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

    pg_case!(check_exists_with_outer_ref);
    pg_case!(check_not_exists_typed_outer_ref);
    pg_case!(check_exclude_relation_spanning_semantics);
    pg_case!(check_in_subquery);
    pg_case!(check_where_has_count);
    pg_case!(check_annotate_count);
    pg_case!(check_scalar_subquery_in_where);
    pg_case!(check_scalar_subquery_annotation);
    pg_case!(check_outer_ref_outside_subquery_errors);
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
            "CREATE TABLE d6sub_author (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                active INTEGER NOT NULL
            )",
            "CREATE TABLE d6sub_book (
                id INTEGER PRIMARY KEY,
                author_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                published INTEGER NOT NULL
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

    sqlite_case!(check_exists_with_outer_ref);
    sqlite_case!(check_not_exists_typed_outer_ref);
    sqlite_case!(check_exclude_relation_spanning_semantics);
    sqlite_case!(check_in_subquery);
    sqlite_case!(check_where_has_count);
    sqlite_case!(check_annotate_count);
    sqlite_case!(check_scalar_subquery_in_where);
    sqlite_case!(check_scalar_subquery_annotation);
    sqlite_case!(check_outer_ref_outside_subquery_errors);
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
            "DROP TABLE IF EXISTS d6sub_book",
            "DROP TABLE IF EXISTS d6sub_author",
            "CREATE TABLE d6sub_author (
                id BIGINT PRIMARY KEY,
                name VARCHAR(60) NOT NULL,
                active BOOLEAN NOT NULL
            )",
            "CREATE TABLE d6sub_book (
                id BIGINT PRIMARY KEY,
                author_id BIGINT NOT NULL,
                title VARCHAR(120) NOT NULL,
                published BOOLEAN NOT NULL
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

    mysql_case!(check_exists_with_outer_ref);
    mysql_case!(check_not_exists_typed_outer_ref);
    mysql_case!(check_exclude_relation_spanning_semantics);
    mysql_case!(check_in_subquery);
    mysql_case!(check_where_has_count);
    mysql_case!(check_annotate_count);
    mysql_case!(check_scalar_subquery_in_where);
    mysql_case!(check_scalar_subquery_annotation);
    mysql_case!(check_outer_ref_outside_subquery_errors);
}
