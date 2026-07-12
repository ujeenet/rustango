//! SQLite ORM demo — exercises a wide slice of rustango's ORM surface
//! against an in-memory SQLite database (no docker, no env, no setup).
//!
//! Run with:
//!
//!   PATH="$HOME/.cargo/bin:$PATH" \
//!     cargo run -p rustango --example sqlite_orm_demo --features sqlite
//!
//! Demonstrates:
//!   - Model derivation with `Auto<i64>` PK + `ForeignKey<T>`
//!   - Schema bootstrap via the dialect-aware DDL emitter
//!   - `insert_pool` (INSERT … RETURNING populating Auto PKs)
//!   - `save_pool` / `delete_pool` / `count` / `fetch`
//!   - QuerySet `filter` / `order_by` / `limit` / `offset`
//!   - Filter operators: Eq, Gt, In, Like, ILike, Between
//!   - `select_related` (FK join decoded via LoadRelatedSqlite)
//!   - `fetch_with_prefetch_pool` (parents + their children, two trips)
//!   - `bulk_insert_pool` via the IR (`BulkInsertQuery`)
//!   - Transactions via `transaction_pool` + `PoolTx::Sqlite`
//!   - Raw query / raw execute helpers
//!   - Aggregations via `fetch_aggregate_pool` + `AggregateBuilder`

#![cfg(feature = "sqlite")]

use chrono::{TimeZone, Utc};
use rustango::core::{AggregateExpr, BulkInsertQuery, Model as _, Op, SqlValue};
use rustango::query::QuerySet;
use rustango::sql::{
    raw_execute_pool, raw_query_pool, transaction_pool, Auto, CounterPool, FetcherPool, ForeignKey,
    Pool, PoolTx,
};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "demo_author", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    pub age: i32,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "demo_post", display = "title")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 8000)]
    pub body: String,
    pub author_id: ForeignKey<Author>,
    pub views: i64,
    pub published_at: chrono::DateTime<chrono::Utc>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = open_pool().await?;
    bootstrap_schema(&pool).await?;

    println!("== 1. insert_pool round-trips Auto<i64> via INSERT…RETURNING ==");
    let mut alice = Author {
        id: Auto::Unset,
        name: "Alice".into(),
        age: 32,
    };
    alice.insert_pool(&pool).await?;
    let alice_id = *alice.id.get().expect("Alice has an id");
    println!("inserted Alice with id = {alice_id}");

    let mut bob = Author {
        id: Auto::Unset,
        name: "Bob".into(),
        age: 47,
    };
    bob.insert_pool(&pool).await?;
    let bob_id = *bob.id.get().expect("Bob has an id");
    println!("inserted Bob with id   = {bob_id}");

    println!("\n== 2. bulk_insert_pool via the IR — one round trip for many rows ==");
    let post_seed: Vec<(&str, &str, i64, i64, chrono::DateTime<chrono::Utc>)> = vec![
        (
            "Hello SQLite",
            "rustango talks to sqlite now",
            alice_id,
            0,
            Utc.with_ymd_and_hms(2026, 5, 1, 10, 0, 0).unwrap(),
        ),
        (
            "Auto PKs are nice",
            "INSERT…RETURNING populates the model",
            alice_id,
            5,
            Utc.with_ymd_and_hms(2026, 5, 2, 10, 0, 0).unwrap(),
        ),
        (
            "Bob's announcement",
            "Bob is shipping things",
            bob_id,
            100,
            Utc.with_ymd_and_hms(2026, 5, 3, 10, 0, 0).unwrap(),
        ),
        (
            "Draft",
            "Lorem ipsum",
            bob_id,
            0,
            Utc.with_ymd_and_hms(2026, 5, 4, 10, 0, 0).unwrap(),
        ),
    ];
    let bulk_query = BulkInsertQuery {
        model: Post::SCHEMA,
        // Auto<i64> PK column omitted — DEFAULT (sequence/AUTOINCREMENT) fires.
        columns: vec!["title", "body", "author_id", "views", "published_at"],
        rows: post_seed
            .iter()
            .map(|(t, b, fk, v, ts)| {
                vec![
                    SqlValue::String((*t).into()),
                    SqlValue::String((*b).into()),
                    SqlValue::I64(*fk),
                    SqlValue::I64(*v),
                    SqlValue::DateTime(*ts),
                ]
            })
            .collect(),
        returning: vec![],
        on_conflict: None,
    };
    rustango::sql::bulk_insert_pool(&pool, &bulk_query).await?;
    println!("bulk_insert_pool inserted {} rows", post_seed.len());

    println!("\n== 3. count + fetch ==");
    let total = Post::objects().count(&pool).await?;
    println!("Post count: {total}");
    let all_posts: Vec<Post> = Post::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await?;
    for p in &all_posts {
        println!(
            "  #{} {:?} (views={}, author_pk={})",
            p.id.get().copied().unwrap(),
            p.title,
            p.views,
            p.author_id.pk(),
        );
    }

    println!("\n== 4. filter operators (Eq / Gt / In / Like / ILike / Between) ==");

    let alice_posts: Vec<Post> = Post::objects()
        .filter_op("author_id", Op::Eq, alice_id)
        .order_by(&[("title", false)])
        .fetch(&pool)
        .await?;
    println!("Alice has {} post(s)", alice_posts.len());

    let popular: Vec<Post> = Post::objects()
        .filter_op("views", Op::Gt, 0_i64)
        .fetch(&pool)
        .await?;
    println!("posts with views > 0: {}", popular.len());

    let by_known_authors: Vec<Post> = Post::objects()
        .filter_op(
            "author_id",
            Op::In,
            SqlValue::List(vec![SqlValue::I64(alice_id), SqlValue::I64(bob_id)]),
        )
        .fetch(&pool)
        .await?;
    println!("posts by Alice or Bob: {}", by_known_authors.len());

    let titles_with_pks: Vec<Post> = Post::objects()
        .filter_op("title", Op::Like, "%PKs%")
        .fetch(&pool)
        .await?;
    println!("LIKE '%PKs%': {}", titles_with_pks.len());

    // ILIKE — translated to LOWER(col) LIKE LOWER(?) on SQLite.
    let titles_ilike: Vec<Post> = Post::objects()
        .filter_op("title", Op::ILike, "%hello%")
        .fetch(&pool)
        .await?;
    println!("ILIKE '%hello%': {}", titles_ilike.len());

    let mid_views: Vec<Post> = Post::objects()
        .filter_op(
            "views",
            Op::Between,
            SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(50)]),
        )
        .fetch(&pool)
        .await?;
    println!("BETWEEN 1 AND 50 views: {}", mid_views.len());

    println!("\n== 5. order_by + limit + offset ==");
    let newest_two: Vec<Post> = Post::objects()
        .order_by(&[("published_at", true)])
        .limit(2)
        .fetch(&pool)
        .await?;
    for p in &newest_two {
        println!("  newest: {}", p.title);
    }
    let skip_one: Vec<Post> = Post::objects()
        .order_by(&[("id", false)])
        .offset(1)
        .limit(2)
        .fetch(&pool)
        .await?;
    println!("skip 1 row, take 2: {}", skip_one.len());

    println!("\n== 6. save_pool — UPDATE one row ==");
    let mut first = Post::objects()
        .order_by(&[("id", false)])
        .limit(1)
        .fetch(&pool)
        .await?
        .pop()
        .expect("at least one post");
    first.views = 999;
    first.save_pool(&pool).await?;
    let refetched: Post = QuerySet::<Post>::new()
        .filter_op("id", Op::Eq, *first.id.get().unwrap())
        .fetch(&pool)
        .await?
        .pop()
        .unwrap();
    println!("after save_pool, views = {}", refetched.views);
    assert_eq!(refetched.views, 999);

    println!("\n== 7. transaction_pool — BEGIN / COMMIT around two writes ==");
    {
        let mut tx = transaction_pool(&pool).await?;
        match &mut tx {
            PoolTx::Sqlite(t) => {
                sqlx::query("UPDATE demo_post SET views = views + 1 WHERE author_id = ?")
                    .bind(alice_id)
                    .execute(&mut **t)
                    .await?;
                sqlx::query("UPDATE demo_author SET age = age + 1 WHERE id = ?")
                    .bind(alice_id)
                    .execute(&mut **t)
                    .await?;
            }
            #[cfg(feature = "postgres")]
            PoolTx::Postgres(_) => unreachable!("pool is sqlite"),
            #[cfg(feature = "mysql")]
            PoolTx::Mysql(_) => unreachable!("pool is sqlite"),
        }
        tx.commit().await?;
    }
    let alice_after: Author = QuerySet::<Author>::new()
        .filter_op("id", Op::Eq, alice_id)
        .fetch(&pool)
        .await?
        .pop()
        .unwrap();
    println!("Alice's age after commit: {}", alice_after.age);
    assert_eq!(alice_after.age, 33);

    println!("\n== 8. select_related — FK join populates ForeignKey<Author>::value() ==");
    let joined: Vec<Post> = Post::objects()
        .select_related("author_id")
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await?;
    for p in &joined {
        let author_name = p
            .author_id
            .value()
            .map(|a| a.name.as_str())
            .unwrap_or("<not joined>");
        println!("  post '{}' by {}", p.title, author_name);
    }

    println!("\n== 9. fetch_with_prefetch_pool — parents + their posts ==");
    let bundles: Vec<(Author, Vec<Post>)> =
        rustango::sql::fetch_with_prefetch_pool(Author::objects(), "author_id", &pool).await?;
    for (a, ps) in &bundles {
        println!("  {} ({} posts)", a.name, ps.len());
    }

    println!("\n== 10. aggregate over Author — MIN / MAX / AVG age ==");
    // `.values(&[])` = a table-wide *scalar* aggregate (Django's
    // `.aggregate(Min("age"))`): no base columns, no GROUP BY, one row.
    // Without it, `.annotate(...)` alone groups by every model column.
    let agg_query = Author::objects()
        .values(&[])
        .annotate("min_age", AggregateExpr::Min("age"))
        .annotate("max_age", AggregateExpr::Max("age"))
        .annotate("avg_age", AggregateExpr::Avg("age"))
        .compile()?;
    let row: Vec<(i32, i32, f64)> =
        rustango::sql::fetch_aggregate_pool::<(i32, i32, f64)>(&pool, &agg_query).await?;
    if let Some((min, max, avg)) = row.first().copied() {
        println!("authors: min_age={min}, max_age={max}, avg_age={avg:.1}");
    }

    println!("\n== 11. raw_query_pool / raw_execute_pool ==");
    let raw_total: Vec<(i64,)> =
        raw_query_pool("SELECT COUNT(*) FROM demo_post", vec![], &pool).await?;
    println!("raw_query_pool COUNT(*): {}", raw_total[0].0);

    let raw_affected = raw_execute_pool(
        &pool,
        "UPDATE demo_post SET views = 0 WHERE views < ?",
        vec![SqlValue::I64(50)],
    )
    .await?;
    println!("raw_execute_pool reset {raw_affected} rows");

    println!("\n== 12. delete_pool removes a row ==");
    let doomed = Post::objects()
        .filter_op("title", Op::Eq, "Draft")
        .fetch(&pool)
        .await?
        .pop()
        .expect("Draft post exists");
    let removed = doomed.delete_pool(&pool).await?;
    println!("delete_pool removed {removed} row");
    let after = Post::objects().count(&pool).await?;
    println!("Post count after delete: {after}");

    println!("\n== ✓ All 12 sections completed against in-memory SQLite ==");
    Ok(())
}

async fn open_pool() -> Result<Pool, Box<dyn std::error::Error>> {
    // Single-connection in-memory DB so every section sees the same
    // database. Default sqlx pool would open multiple anonymous DBs.
    let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    Ok(sqlite.into())
}

async fn bootstrap_schema(pool: &Pool) -> Result<(), Box<dyn std::error::Error>> {
    use rustango::migrate::ddl;
    let dialect = pool.dialect();
    for schema in [Author::SCHEMA, Post::SCHEMA] {
        let sql = ddl::create_table_sql_with_dialect(dialect, schema);
        raw_execute_pool(pool, &sql, vec![]).await?;
    }
    // SQLite has no `ALTER TABLE … ADD CONSTRAINT FOREIGN KEY …` —
    // FK constraints have to be declared inline at CREATE TABLE
    // time. The framework's emitter is built for PG/MySQL, so we
    // skip this on SQLite. The demo doesn't depend on FK enforcement
    // (every insert references an already-inserted parent).
    if dialect.name() != "sqlite" {
        for schema in [Author::SCHEMA, Post::SCHEMA] {
            for sql in ddl::create_constraints_sql_with_dialect(dialect, schema) {
                raw_execute_pool(pool, &sql, vec![]).await?;
            }
        }
    }
    Ok(())
}
