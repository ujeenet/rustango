//! Multi-hop `select_related` — closes #297 / T2.2.
//!
//! Pins the emitted LEFT JOIN chain for 2-hop and 3-hop chains across
//! Postgres + MySQL + SQLite, including the alias-prefixed projection
//! and the inter-hop ON predicate that joins each hop to the previous
//! hop's alias (not back to the main table).

use rustango::query::QuerySet;
use rustango::sql::{Dialect, ForeignKey, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "srm_country")]
#[allow(dead_code)]
pub struct Country {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 2)]
    code: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "srm_profile")]
#[allow(dead_code)]
pub struct Profile {
    #[rustango(primary_key)]
    id: i64,
    pub country: ForeignKey<Country>,
    #[rustango(max_length = 200)]
    bio: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "srm_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    id: i64,
    pub profile: ForeignKey<Profile>,
    #[rustango(max_length = 80)]
    name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "srm_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    pub author: ForeignKey<Author>,
    #[rustango(max_length = 200)]
    title: String,
}

fn compile_pg(qs: QuerySet<Post>) -> String {
    let q = qs.compile().unwrap();
    Postgres.compile_select(&q).unwrap().sql
}

fn compile_my(qs: QuerySet<Post>) -> String {
    let q = qs.compile().unwrap();
    MySql.compile_select(&q).unwrap().sql
}

fn compile_sqlite(qs: QuerySet<Post>) -> String {
    let q = qs.compile().unwrap();
    Sqlite.compile_select(&q).unwrap().sql
}

// ---------- Single-hop bit-identity with pre-T2.2 ----------

#[test]
fn single_hop_emits_one_left_join_unchanged_from_v041() {
    let sql = compile_pg(Post::objects().select_related("author"));
    // One LEFT JOIN, alias = field name.
    assert!(
        sql.contains(r#"LEFT JOIN "srm_author" AS "author""#),
        "PG single-hop alias: {sql}"
    );
    // ON joins the main table to the alias.
    assert!(
        sql.contains(r#""srm_post"."author" = "author"."id""#),
        "PG single-hop ON: {sql}"
    );
}

// ---------- Multi-hop JOIN emission ----------

#[test]
fn two_hop_chain_emits_two_left_joins_on_pg() {
    let sql = compile_pg(Post::objects().select_related("author__profile"));
    // First hop: post.author_id → author.id
    assert!(
        sql.contains(r#"LEFT JOIN "srm_author" AS "author""#),
        "PG 2-hop first join: {sql}"
    );
    assert!(
        sql.contains(r#""srm_post"."author" = "author"."id""#),
        "PG 2-hop first ON: {sql}"
    );
    // Second hop: author.profile_id → author__profile.id
    assert!(
        sql.contains(r#"LEFT JOIN "srm_profile" AS "author__profile""#),
        "PG 2-hop second join: {sql}"
    );
    assert!(
        sql.contains(r#""author"."profile" = "author__profile"."id""#),
        "PG 2-hop second ON (must reference prior alias, not main table): {sql}"
    );
}

#[test]
fn three_hop_chain_emits_three_left_joins_on_pg() {
    let sql = compile_pg(Post::objects().select_related("author__profile__country"));
    assert!(
        sql.contains(r#"LEFT JOIN "srm_author" AS "author""#),
        "PG 3-hop a: {sql}"
    );
    assert!(
        sql.contains(r#"LEFT JOIN "srm_profile" AS "author__profile""#),
        "PG 3-hop b: {sql}"
    );
    assert!(
        sql.contains(r#"LEFT JOIN "srm_country" AS "author__profile__country""#),
        "PG 3-hop c: {sql}"
    );
    // Each successive hop's ON references the prior alias.
    assert!(
        sql.contains(r#""author__profile"."country" = "author__profile__country"."id""#),
        "PG 3-hop final ON references prior alias: {sql}"
    );
}

#[test]
fn three_hop_chain_emits_per_dialect_quoted_joins() {
    let qs = || Post::objects().select_related("author__profile__country");
    let my = compile_my(qs());
    let lite = compile_sqlite(qs());
    // MySQL uses backticks.
    assert!(
        my.contains("LEFT JOIN `srm_author` AS `author`"),
        "MySQL: {my}"
    );
    assert!(
        my.contains("LEFT JOIN `srm_profile` AS `author__profile`"),
        "MySQL: {my}"
    );
    assert!(
        my.contains("LEFT JOIN `srm_country` AS `author__profile__country`"),
        "MySQL: {my}"
    );
    assert!(
        my.contains("`author__profile`.`country` = `author__profile__country`.`id`"),
        "MySQL: {my}"
    );
    // SQLite uses double-quotes.
    assert!(
        lite.contains(r#"LEFT JOIN "srm_author" AS "author""#),
        "SQLite: {lite}"
    );
    assert!(
        lite.contains(r#"LEFT JOIN "srm_profile" AS "author__profile""#),
        "SQLite: {lite}"
    );
    assert!(
        lite.contains(r#"LEFT JOIN "srm_country" AS "author__profile__country""#),
        "SQLite: {lite}"
    );
}

#[test]
fn multi_hop_projection_includes_each_target_columns() {
    let sql = compile_pg(Post::objects().select_related("author__profile__country"));
    // The author's name + the profile's bio + the country's code all
    // appear in the SELECT projection under their alias prefixes.
    assert!(sql.contains(r#""author"."name""#), "PG author col: {sql}");
    assert!(
        sql.contains(r#""author__profile"."bio""#),
        "PG profile col: {sql}"
    );
    assert!(
        sql.contains(r#""author__profile__country"."code""#),
        "PG country col: {sql}"
    );
}

// ---------- Validation ----------

#[test]
fn typo_first_hop_errors_with_clear_chain_position() {
    let q = Post::objects().select_related("authr__profile").compile();
    let err = q.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("authr"), "should name the bad hop: {msg}");
    assert!(
        msg.contains("hop 1") || msg.contains("authr__profile"),
        "should locate the failure within the chain: {msg}"
    );
}

#[test]
fn typo_second_hop_reports_correct_parent_model() {
    let q = Post::objects().select_related("author__bogus").compile();
    let err = q.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("bogus"), "should name the bad hop: {msg}");
}

#[test]
fn non_fk_field_in_chain_errors() {
    // `title` is a non-FK string column on Post — using it as the
    // first hop must fail with a clear "not a ForeignKey" message.
    let q = Post::objects().select_related("title").compile();
    let err = q.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("ForeignKey"),
        "should mention it's not an FK: {msg}"
    );
}

#[test]
fn empty_hop_in_chain_is_rejected() {
    // `author____profile` — empty middle hop should error before
    // resolving FKs.
    let q = Post::objects()
        .select_related("author____profile")
        .compile();
    let err = q.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("empty"),
        "should call out the empty hop: {msg}"
    );
}

// ---------- Multiple chains compose ----------

#[test]
fn two_separate_chains_compose_into_distinct_alias_trees() {
    // `.select_related("author")` + `.select_related("author__profile")`
    // — the first emits an `author` alias; the second emits both
    // `author` (deduplicated by the writer? — actually the current
    // writer keeps both; just verify both alias names appear in their
    // own joins).
    let sql = compile_pg(
        Post::objects()
            .select_related("author")
            .select_related("author__profile"),
    );
    assert!(sql.contains(r#"AS "author""#), "first chain: {sql}");
    assert!(
        sql.contains(r#"AS "author__profile""#),
        "second chain: {sql}"
    );
}

// ---------- Decoder-side recursive stitching (#451) ----------
//
// T2.2 (#308) shipped the multi-hop JOIN *emission* (pinned above);
// #451 is the decoder-side gap: a fetched `Post` must come back with
// `post.author -> profile -> country` all populated as nested loaded
// objects, not just the columns sitting in the row. Live on SQLite
// (the cheapest dialect) so the recursive `__rustango_load_related*`
// is exercised end-to-end.
#[cfg(feature = "sqlite")]
mod sqlite_live {
    use super::Post;
    use rustango::sql::{sqlx, FetcherPool as _, Pool};

    async fn seeded_pool() -> Pool {
        let sq = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite mem pool");
        for ddl in [
            "CREATE TABLE srm_country (id INTEGER PRIMARY KEY, code TEXT NOT NULL)",
            "CREATE TABLE srm_profile (id INTEGER PRIMARY KEY, country INTEGER NOT NULL, bio TEXT NOT NULL)",
            "CREATE TABLE srm_author (id INTEGER PRIMARY KEY, profile INTEGER NOT NULL, name TEXT NOT NULL)",
            "CREATE TABLE srm_post (id INTEGER PRIMARY KEY, author INTEGER NOT NULL, title TEXT NOT NULL)",
        ] {
            sqlx::query(ddl).execute(&sq).await.expect("ddl");
        }
        for dml in [
            "INSERT INTO srm_country (id, code) VALUES (1, 'US')",
            "INSERT INTO srm_profile (id, country, bio) VALUES (1, 1, 'hello-bio')",
            "INSERT INTO srm_author (id, profile, name) VALUES (1, 1, 'Ada')",
            "INSERT INTO srm_post (id, author, title) VALUES (1, 1, 'Hello')",
        ] {
            sqlx::query(dml).execute(&sq).await.expect("seed");
        }
        Pool::Sqlite(sq)
    }

    #[tokio::test]
    async fn three_hop_select_related_stitches_nested_objects() {
        let pool = seeded_pool().await;
        let posts: Vec<Post> = Post::objects()
            .select_related("author__profile__country")
            .fetch(&pool)
            .await
            .expect("fetch");
        assert_eq!(posts.len(), 1);
        let post = &posts[0];

        // hop 1 — author (worked pre-#451).
        let author = post.author.value().expect("hop 1: author stitched");
        assert_eq!(author.name, "Ada");
        // hop 2 — THE #451 GAP: profile was never stitched onto author
        // before the recursive `__rustango_load_related`.
        let profile = author.profile.value().expect("hop 2: profile stitched");
        assert_eq!(profile.bio, "hello-bio");
        // hop 3 — country stitched onto profile.
        let country = profile.country.value().expect("hop 3: country stitched");
        assert_eq!(country.code, "US");
    }

    #[tokio::test]
    async fn single_hop_does_not_over_stitch() {
        // Regression guard: select_related("author") must stitch the
        // author but leave the author's own FK (profile) UNLOADED — the
        // leaf-alias change must not accidentally pull deeper relations.
        let pool = seeded_pool().await;
        let posts: Vec<Post> = Post::objects()
            .select_related("author")
            .fetch(&pool)
            .await
            .expect("fetch");
        let author = posts[0].author.value().expect("author stitched");
        assert_eq!(author.name, "Ada");
        assert!(
            !author.profile.is_loaded(),
            "profile must stay unloaded when only `author` is select_related"
        );
    }
}

// Same 3-hop stitch, proven LIVE on Postgres + MySQL so the recursive
// decoder is verified on every dialect, not just SQLite. Both skip
// silently when their DB URL is unset (offline / CI-without-DB), matching
// the rest of the live suite; CI's `e2e (postgres|mysql)` jobs set them.
#[cfg(feature = "postgres")]
mod pg_live {
    use super::Post;
    use rustango::sql::{sqlx, FetcherPool as _, Pool};

    const DDL: [&str; 8] = [
        "DROP TABLE IF EXISTS srm_post",
        "DROP TABLE IF EXISTS srm_author",
        "DROP TABLE IF EXISTS srm_profile",
        "DROP TABLE IF EXISTS srm_country",
        // BIGINT (int8) — the model's `id: i64` / `ForeignKey<_>` PKs
        // decode as i64; PG is strict about int4-vs-int8.
        "CREATE TABLE srm_country (id BIGINT PRIMARY KEY, code TEXT NOT NULL)",
        "CREATE TABLE srm_profile (id BIGINT PRIMARY KEY, country BIGINT NOT NULL, bio TEXT NOT NULL)",
        "CREATE TABLE srm_author (id BIGINT PRIMARY KEY, profile BIGINT NOT NULL, name TEXT NOT NULL)",
        "CREATE TABLE srm_post (id BIGINT PRIMARY KEY, author BIGINT NOT NULL, title TEXT NOT NULL)",
    ];
    const SEED: [&str; 4] = [
        "INSERT INTO srm_country (id, code) VALUES (1, 'US')",
        "INSERT INTO srm_profile (id, country, bio) VALUES (1, 1, 'hello-bio')",
        "INSERT INTO srm_author (id, profile, name) VALUES (1, 1, 'Ada')",
        "INSERT INTO srm_post (id, author, title) VALUES (1, 1, 'Hello')",
    ];

    #[tokio::test]
    async fn three_hop_select_related_stitches_nested_objects() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL unset — skipping PG multi-hop live test");
            return;
        };
        let pg = sqlx::PgPool::connect(&url).await.expect("connect PG");
        for s in DDL.iter().chain(SEED.iter()) {
            sqlx::query(s).execute(&pg).await.expect("setup");
        }
        let pool = Pool::Postgres(pg);
        let posts: Vec<Post> = Post::objects()
            .select_related("author__profile__country")
            .fetch(&pool)
            .await
            .expect("fetch");
        assert_eq!(posts.len(), 1);
        let author = posts[0].author.value().expect("hop 1: author");
        assert_eq!(author.name, "Ada");
        let profile = author.profile.value().expect("hop 2: profile");
        assert_eq!(profile.bio, "hello-bio");
        let country = profile.country.value().expect("hop 3: country");
        assert_eq!(country.code, "US");
    }
}

#[cfg(feature = "mysql")]
mod my_live {
    use super::Post;
    use rustango::sql::{sqlx, FetcherPool as _, Pool};

    #[tokio::test]
    async fn three_hop_select_related_stitches_nested_objects() {
        let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
            eprintln!("MYSQL_TEST_URL unset — skipping MySQL multi-hop live test");
            return;
        };
        let my = sqlx::MySqlPool::connect(&url).await.expect("connect MySQL");
        let ddl = [
            "DROP TABLE IF EXISTS srm_post",
            "DROP TABLE IF EXISTS srm_author",
            "DROP TABLE IF EXISTS srm_profile",
            "DROP TABLE IF EXISTS srm_country",
            "CREATE TABLE srm_country (id BIGINT PRIMARY KEY, code VARCHAR(8) NOT NULL)",
            "CREATE TABLE srm_profile (id BIGINT PRIMARY KEY, country BIGINT NOT NULL, bio VARCHAR(200) NOT NULL)",
            "CREATE TABLE srm_author (id BIGINT PRIMARY KEY, profile BIGINT NOT NULL, name VARCHAR(80) NOT NULL)",
            "CREATE TABLE srm_post (id BIGINT PRIMARY KEY, author BIGINT NOT NULL, title VARCHAR(200) NOT NULL)",
            "INSERT INTO srm_country (id, code) VALUES (1, 'US')",
            "INSERT INTO srm_profile (id, country, bio) VALUES (1, 1, 'hello-bio')",
            "INSERT INTO srm_author (id, profile, name) VALUES (1, 1, 'Ada')",
            "INSERT INTO srm_post (id, author, title) VALUES (1, 1, 'Hello')",
        ];
        for s in ddl {
            sqlx::query(s).execute(&my).await.expect("setup");
        }
        let pool = Pool::Mysql(my);
        let posts: Vec<Post> = Post::objects()
            .select_related("author__profile__country")
            .fetch(&pool)
            .await
            .expect("fetch");
        assert_eq!(posts.len(), 1);
        let author = posts[0].author.value().expect("hop 1: author");
        assert_eq!(author.name, "Ada");
        let profile = author.profile.value().expect("hop 2: profile");
        assert_eq!(profile.bio, "hello-bio");
        let country = profile.country.value().expect("hop 3: country");
        assert_eq!(country.code, "US");
    }
}
