//! Django 6.0 ORM parity — execution-based verification.
//! Scenario group D: multi-join queries — `select_related` chains,
//! cross-table predicates, F() across joins, and the classic
//! join-duplication trap.
//!
//! Django scenarios covered (docs.djangoproject.com/en/6.0):
//! - `select_related("author__profile__country")` nested stitching
//! - `filter(author__name="Ada")` relation-spanning filter — Django
//!   joins implicitly; rustango pins the documented rejection (the
//!   workaround is an explicit `.join()` + aliased predicate, also
//!   exercised here)
//! - `order_by("author__name")` relation-spanning ordering — same
//!   story as the filter
//! - `filter(score__gt=F("post__views"))` — F() comparing columns
//!   across a join
//! - `annotate(nc=Count("comments"), nl=Count("likes"))` — Django
//!   inflates counts without `distinct=True` (the JOIN-duplication
//!   trap); rustango lowers relation counts to correlated subqueries
//!   so they can never inflate
//!
//! AUDIT NOTES:
//! - Two relation aggregates in ONE query now compose —
//!   `AggregateBuilder::annotate_count` / `annotate_sum` / … chain off
//!   the first relation annotate (#1038, shipped). Each lowers to its own
//!   correlated subquery, so neither inflates.
//! - Ad-hoc `.join()` now composes with `.aggregate()` —
//!   `AggregateQuery` carries `joins`, so Django's join+GROUP BY shape
//!   (`values("author.name").annotate(Count)`) works via an explicit
//!   join + aliased `group_by` (#1040, shipped). The `author__name`
//!   sugar will ride #1031's `resolve_span_chain`.

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use rustango::core::joins::{aliased, col_filter};
    use rustango::core::{Expr, Join, JoinKind, Model as _, Op, SqlValue, WhereExpr};
    use rustango::sql::{raw_execute_pool, ExecError, FetcherPool as _, ForeignKey, Pool};
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6join_country")]
    #[allow(dead_code)]
    pub struct Country {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 2)]
        pub code: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6join_profile")]
    #[allow(dead_code)]
    pub struct Profile {
        #[rustango(primary_key)]
        pub id: i64,
        pub country: ForeignKey<Country>,
        #[rustango(max_length = 100)]
        pub bio: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6join_author")]
    #[allow(dead_code)]
    pub struct Author {
        #[rustango(primary_key)]
        pub id: i64,
        pub profile: ForeignKey<Profile>,
        #[rustango(max_length = 60)]
        pub name: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(
        table = "d6join_post",
        reverse_has(name = "comments", child = "Comment", child_fk_column = "post_id",),
        reverse_has(name = "likes", child = "Like", child_fk_column = "post_id",)
    )]
    #[allow(dead_code)]
    pub struct Post {
        #[rustango(primary_key)]
        pub id: i64,
        pub author: ForeignKey<Author>,
        #[rustango(max_length = 120)]
        pub title: String,
        pub views: i64,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6join_comment")]
    #[allow(dead_code)]
    pub struct Comment {
        #[rustango(primary_key)]
        pub id: i64,
        pub post_id: ForeignKey<Post, i64>,
        pub score: i64,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "d6join_like")]
    #[allow(dead_code)]
    pub struct Like {
        #[rustango(primary_key)]
        pub id: i64,
        pub post_id: ForeignKey<Post, i64>,
    }

    /// Post 1 "Hello" (Ada/US, 10 views): 3 comments (5, 12, 20) + 2
    /// likes. Post 2 "World" (Bob/DE, 50 views): 0 comments + 1 like.
    pub async fn seed(pool: &Pool) {
        for sql in [
            "INSERT INTO d6join_country (id, code) VALUES (1, 'US')",
            "INSERT INTO d6join_country (id, code) VALUES (2, 'DE')",
            "INSERT INTO d6join_profile (id, country, bio) VALUES (1, 1, 'ada bio')",
            "INSERT INTO d6join_profile (id, country, bio) VALUES (2, 2, 'bob bio')",
            "INSERT INTO d6join_author (id, profile, name) VALUES (1, 1, 'Ada')",
            "INSERT INTO d6join_author (id, profile, name) VALUES (2, 2, 'Bob')",
            "INSERT INTO d6join_post (id, author, title, views) VALUES (1, 1, 'Hello', 10)",
            "INSERT INTO d6join_post (id, author, title, views) VALUES (2, 2, 'World', 50)",
            "INSERT INTO d6join_comment (id, post_id, score) VALUES (1, 1, 5)",
            "INSERT INTO d6join_comment (id, post_id, score) VALUES (2, 1, 12)",
            "INSERT INTO d6join_comment (id, post_id, score) VALUES (3, 1, 20)",
            "INSERT INTO d6join_like (id, post_id) VALUES (1, 1)",
            "INSERT INTO d6join_like (id, post_id) VALUES (2, 1)",
            "INSERT INTO d6join_like (id, post_id) VALUES (3, 2)",
        ] {
            raw_execute_pool(pool, sql, vec![]).await.expect("seed");
        }
    }

    /// Django `select_related("author__profile__country")` — one
    /// query, nested loaded objects on every hop.
    pub async fn check_three_hop_select_related_stitching(pool: &Pool) {
        let posts: Vec<Post> = Post::objects()
            .select_related("author__profile__country")
            .order_by(&[("id", false)])
            .fetch(pool)
            .await
            .expect("3-hop fetch");
        assert_eq!(posts.len(), 2);
        let author = posts[0].author.value().expect("hop 1 stitched");
        assert_eq!(author.name, "Ada");
        let profile = author.profile.value().expect("hop 2 stitched");
        assert_eq!(profile.bio, "ada bio");
        let country = profile.country.value().expect("hop 3 stitched");
        assert_eq!(country.code, "US");
    }

    /// Django's `filter(author__name="Ada")` implicit-join filter —
    /// relation-spanning string lookups now resolve the FK chain into
    /// LEFT JOINs + an aliased predicate (#1031). Single-hop, multi-hop
    /// with a lookup suffix, and span-over-a-`select_related`-path (one
    /// JOIN, not two) all work.
    pub async fn check_relation_spanning_filter_joins(pool: &Pool) {
        // Single hop: `author__name = "Ada"` → post 1 only.
        let rows: Vec<Post> = Post::objects()
            .filter("author__name", "Ada")
            .fetch(pool)
            .await
            .expect("single-hop relation-spanning filter");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Hello");

        // Two hops + a lookup suffix: `author__profile__bio__icontains`.
        // Ada's profile bio is "ada bio"; Bob's is "bob bio".
        let rows: Vec<Post> = Post::objects()
            .filter("author__profile__bio__icontains", "ADA")
            .fetch(pool)
            .await
            .expect("two-hop relation-spanning filter with suffix");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Hello");

        // Span over the same path as a `select_related` — the JOIN is
        // deduped (alias `author` emitted once), and the parent rows
        // still stitch.
        let rows: Vec<Post> = Post::objects()
            .select_related("author")
            .filter("author__name", "Bob")
            .fetch(pool)
            .await
            .expect("span + select_related dedupe");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "World");
        assert_eq!(rows[0].author.value().expect("stitched").name, "Bob");
    }

    /// Django's `order_by("author__name")` implicit-join ordering —
    /// relation-spanning order keys now resolve the FK chain into LEFT
    /// JOINs + an aliased ORDER BY term (#1031 Part 2), reusing the same
    /// `resolve_span_chain` walk as the `filter()` path.
    pub async fn check_relation_spanning_order_by_joins(pool: &Pool) {
        // ASC by the related author name: Ada < Bob → "Hello" then "World".
        let rows: Vec<Post> = Post::objects()
            .order_by(&[("author__name", false)])
            .fetch(pool)
            .await
            .expect("relation-spanning order_by ASC");
        let titles: Vec<&str> = rows.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["Hello", "World"]);

        // DESC flips it.
        let rows: Vec<Post> = Post::objects()
            .order_by(&[("author__name", true)])
            .fetch(pool)
            .await
            .expect("relation-spanning order_by DESC");
        let titles: Vec<&str> = rows.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["World", "Hello"]);

        // Span over a `select_related` path — the JOIN is deduped (alias
        // `author` emitted once) and the parent rows still stitch.
        let rows: Vec<Post> = Post::objects()
            .select_related("author")
            .order_by(&[("author__name", true)])
            .fetch(pool)
            .await
            .expect("order_by span + select_related dedupe");
        let titles: Vec<&str> = rows.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["World", "Hello"]);
        assert_eq!(rows[0].author.value().expect("stitched").name, "Bob");

        // A trailing lookup suffix after the terminal column is not a
        // valid order key (order_by has no `__icontains`-style grammar).
        let err = Post::objects()
            .order_by(&[("author__name__icontains", false)])
            .fetch(pool)
            .await
            .map(|rows: Vec<Post>| rows.len())
            .expect_err("order_by suffix must be rejected");
        assert!(
            matches!(err, ExecError::Query(_)),
            "expected a compile-side QueryError, got {err:?}"
        );
    }

    /// The explicit-join workaround for `filter(author__name="Ada")`:
    /// `.join()` + `col_filter` on the alias.
    pub async fn check_cross_table_filter_via_join(pool: &Pool) {
        let posts: Vec<Post> = Post::objects()
            .join(Join {
                target: Author::SCHEMA,
                alias: "a",
                kind: JoinKind::Inner,
                on: WhereExpr::ExprCompare {
                    lhs: aliased("a", "id"),
                    op: Op::Eq,
                    rhs: aliased("d6join_post", "author"),
                },
                project: vec![],
            })
            .where_raw(col_filter("a", "name", Op::Eq, "Ada"))
            .fetch(pool)
            .await
            .expect("cross-table filter via join");
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].title, "Hello");
    }

    /// Django `filter(score__gt=F("post__views"))` — column-to-column
    /// comparison across a join. Post 1 has 10 views → comments with
    /// score 12 and 20 qualify.
    pub async fn check_f_comparison_across_join(pool: &Pool) {
        let comments: Vec<Comment> = Comment::objects()
            .join(Join {
                target: Post::SCHEMA,
                alias: "p",
                kind: JoinKind::Inner,
                on: WhereExpr::ExprCompare {
                    lhs: aliased("p", "id"),
                    op: Op::Eq,
                    rhs: aliased("d6join_comment", "post_id"),
                },
                project: vec![],
            })
            .where_raw(WhereExpr::ExprCompare {
                lhs: Expr::Column("score"),
                op: Op::Gt,
                rhs: aliased("p", "views"),
            })
            .order_by(&[("score", false)])
            .fetch(pool)
            .await
            .expect("F across join");
        let scores: Vec<i64> = comments.into_iter().map(|c| c.score).collect();
        assert_eq!(scores, vec![12, 20]);
    }

    /// The Django join-duplication trap: `annotate(nc=Count("comments"),
    /// nl=Count("likes"))` without `distinct=True` returns 6/6 for a post
    /// with 3 comments × 2 likes. rustango lowers relation aggregates to
    /// correlated subqueries — structurally immune — and #1038 lets two
    /// of them chain in ONE query (`AggregateBuilder::annotate_count`).
    pub async fn check_relation_counts_never_inflate(pool: &Pool) {
        // Two relation counts in ONE query (#1038). Post 1 has 3 comments
        // + 2 likes, Post 2 has 0 comments + 1 like — never 6/6.
        let rows = Post::objects()
            .annotate_count("comments")
            .annotate_count("likes")
            .order_by(&[("id", false)])
            .fetch(pool)
            .await
            .expect("two relation counts in one query");
        let pair = |r: &std::collections::HashMap<String, SqlValue>, k: &str| match r.get(k) {
            Some(SqlValue::I64(n)) => *n,
            other => panic!("expected I64 {k}, got {other:?}"),
        };
        let counts: Vec<(i64, i64)> = rows
            .iter()
            .map(|r| (pair(r, "comments_count"), pair(r, "likes_count")))
            .collect();
        assert_eq!(
            counts,
            vec![(3, 2), (0, 1)],
            "correlated subqueries never inflate to 6/6"
        );

        // Mixed count + sum in one pass: Post 1's comment scores 5+12+20.
        let rows = Post::objects()
            .annotate_count("comments")
            .annotate_sum("comments", "score")
            .order_by(&[("id", false)])
            .fetch(pool)
            .await
            .expect("count + sum in one query");
        assert_eq!(pair(&rows[0], "comments_count"), 3);
        assert_eq!(
            pair(&rows[0], "comments_sum_score"),
            37,
            "sum of comment scores 5+12+20"
        );
    }

    /// Django's most common reporting shape — group by a RELATED column:
    /// `Post.objects.values("author__name").annotate(n=Count("id"))`. #1040.
    /// Explicit `.join()` + aliased `group_by("author.name")` (the
    /// `__`-sugar will ride #1031's resolver later). Each author has one
    /// post (Ada → "Hello", Bob → "World"), so the grouped counts are 1/1.
    pub async fn check_group_by_related_column(pool: &Pool) {
        let rows = Post::objects()
            .join(Join {
                target: Author::SCHEMA,
                alias: "author",
                kind: JoinKind::Inner,
                on: WhereExpr::ExprCompare {
                    lhs: aliased("author", "id"),
                    op: Op::Eq,
                    rhs: aliased("d6join_post", "author"),
                },
                project: vec![],
            })
            .aggregate()
            .group_by("author.name")
            .annotate("n", rustango::core::AggregateExpr::Count(None))
            .fetch(pool)
            .await
            .expect("group by author.name");
        // Grouped column projects under the `<alias>__<col>` key.
        let mut pairs: Vec<(String, i64)> = rows
            .iter()
            .map(|r| {
                let name = match r.get("author__name") {
                    Some(SqlValue::String(s)) => s.clone(),
                    other => panic!("expected String author__name, got {other:?}"),
                };
                let n = match r.get("n") {
                    Some(SqlValue::I64(n)) => *n,
                    other => panic!("expected I64 n, got {other:?}"),
                };
                (name, n)
            })
            .collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![("Ada".to_string(), 1), ("Bob".to_string(), 1)],
            "one post per author, grouped by the joined author.name"
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
            r#"DROP TABLE IF EXISTS "d6join_like" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6join_comment" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6join_post" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6join_author" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6join_profile" CASCADE"#,
            r#"DROP TABLE IF EXISTS "d6join_country" CASCADE"#,
            r#"CREATE TABLE "d6join_country" ("id" BIGINT PRIMARY KEY, "code" TEXT NOT NULL)"#,
            r#"CREATE TABLE "d6join_profile" ("id" BIGINT PRIMARY KEY, "country" BIGINT NOT NULL, "bio" TEXT NOT NULL)"#,
            r#"CREATE TABLE "d6join_author" ("id" BIGINT PRIMARY KEY, "profile" BIGINT NOT NULL, "name" TEXT NOT NULL)"#,
            r#"CREATE TABLE "d6join_post" ("id" BIGINT PRIMARY KEY, "author" BIGINT NOT NULL, "title" TEXT NOT NULL, "views" BIGINT NOT NULL)"#,
            r#"CREATE TABLE "d6join_comment" ("id" BIGINT PRIMARY KEY, "post_id" BIGINT NOT NULL, "score" BIGINT NOT NULL)"#,
            r#"CREATE TABLE "d6join_like" ("id" BIGINT PRIMARY KEY, "post_id" BIGINT NOT NULL)"#,
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

    pg_case!(check_three_hop_select_related_stitching);
    pg_case!(check_relation_spanning_filter_joins);
    pg_case!(check_relation_spanning_order_by_joins);
    pg_case!(check_cross_table_filter_via_join);
    pg_case!(check_f_comparison_across_join);
    pg_case!(check_relation_counts_never_inflate);
    pg_case!(check_group_by_related_column);
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
            "CREATE TABLE d6join_country (id INTEGER PRIMARY KEY, code TEXT NOT NULL)",
            "CREATE TABLE d6join_profile (id INTEGER PRIMARY KEY, country INTEGER NOT NULL, bio TEXT NOT NULL)",
            "CREATE TABLE d6join_author (id INTEGER PRIMARY KEY, profile INTEGER NOT NULL, name TEXT NOT NULL)",
            "CREATE TABLE d6join_post (id INTEGER PRIMARY KEY, author INTEGER NOT NULL, title TEXT NOT NULL, views INTEGER NOT NULL)",
            "CREATE TABLE d6join_comment (id INTEGER PRIMARY KEY, post_id INTEGER NOT NULL, score INTEGER NOT NULL)",
            "CREATE TABLE d6join_like (id INTEGER PRIMARY KEY, post_id INTEGER NOT NULL)",
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

    sqlite_case!(check_three_hop_select_related_stitching);
    sqlite_case!(check_relation_spanning_filter_joins);
    sqlite_case!(check_relation_spanning_order_by_joins);
    sqlite_case!(check_cross_table_filter_via_join);
    sqlite_case!(check_f_comparison_across_join);
    sqlite_case!(check_relation_counts_never_inflate);
    sqlite_case!(check_group_by_related_column);
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
            "DROP TABLE IF EXISTS d6join_like",
            "DROP TABLE IF EXISTS d6join_comment",
            "DROP TABLE IF EXISTS d6join_post",
            "DROP TABLE IF EXISTS d6join_author",
            "DROP TABLE IF EXISTS d6join_profile",
            "DROP TABLE IF EXISTS d6join_country",
            "CREATE TABLE d6join_country (id BIGINT PRIMARY KEY, code VARCHAR(2) NOT NULL)",
            "CREATE TABLE d6join_profile (id BIGINT PRIMARY KEY, country BIGINT NOT NULL, bio VARCHAR(100) NOT NULL)",
            "CREATE TABLE d6join_author (id BIGINT PRIMARY KEY, profile BIGINT NOT NULL, name VARCHAR(60) NOT NULL)",
            "CREATE TABLE d6join_post (id BIGINT PRIMARY KEY, author BIGINT NOT NULL, title VARCHAR(120) NOT NULL, views BIGINT NOT NULL)",
            "CREATE TABLE d6join_comment (id BIGINT PRIMARY KEY, post_id BIGINT NOT NULL, score BIGINT NOT NULL)",
            "CREATE TABLE d6join_like (id BIGINT PRIMARY KEY, post_id BIGINT NOT NULL)",
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

    mysql_case!(check_three_hop_select_related_stitching);
    mysql_case!(check_relation_spanning_filter_joins);
    mysql_case!(check_relation_spanning_order_by_joins);
    mysql_case!(check_cross_table_filter_via_join);
    mysql_case!(check_f_comparison_across_join);
    mysql_case!(check_relation_counts_never_inflate);
    mysql_case!(check_group_by_related_column);
}
