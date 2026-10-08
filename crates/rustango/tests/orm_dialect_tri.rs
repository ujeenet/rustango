//! ORM writers that must give the same result on every backend
//! (0.59.11 dialect batch). One body per scenario, three dialects.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::joins::aliased;
use rustango::core::{
    AggregateExpr, ConflictClause, Filter, InsertQuery, Model as _, Op, SearchClause, SqlValue,
    WhereExpr,
};
use rustango::sql::{
    Auto, CounterPool as _, FetcherPool as _, InsertReturningPool, Pool, UpdaterPool as _,
};
use rustango::testkit::matrix::fresh_table;
use rustango::{tri_dialect_test, Model};

/// A natural string PK: no `id` column at all.
#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_code")]
#[rustango(app = "orm_dialect_tri")]
pub struct Code {
    #[rustango(primary_key, max_length = 32)]
    pub code: String,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_post")]
#[rustango(app = "orm_dialect_tri")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)]
    pub slug: String,
    #[rustango(max_length = 64)]
    pub title: String,
    pub parent_id: Option<i64>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_blob")]
#[rustango(app = "orm_dialect_tri")]
pub struct Blob {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub token: uuid::Uuid,
    pub data: Vec<u8>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_meas")]
#[rustango(app = "orm_dialect_tri")]
pub struct Meas {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub n: i64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub taken_on: chrono::NaiveDate,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_reading")]
#[rustango(app = "orm_dialect_tri")]
pub struct Reading {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub meas: rustango::sql::ForeignKey<Meas>,
}

/// Seed one `Meas` row at the RFC 3339 instant `at`.
/// Comments ending in `\`, which closed nothing on MySQL (#2232).
#[derive(Model, Debug, Clone)]
#[rustango(table = "orm_dialect_tri_noted")]
#[rustango(app = "orm_dialect_tri")]
#[rustango(db_table_comment = "C:\\dir\\")]
pub struct Noted {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(db_comment = "C:\\path 'q' \\'\\")]
    pub n: i64,
}

/// Seed one `Meas` row at the RFC 3339 instant `at`, plus a `Reading`
/// of it.
async fn seed_meas(pool: &Pool, n: i64, at: &str) {
    let at: chrono::DateTime<chrono::Utc> = at.parse().expect("instant");
    let mut m = Meas {
        id: Auto::default(),
        n,
        at,
        taken_on: at.date_naive(),
    };
    m.insert_pool(pool).await.expect("seed meas");
    Reading {
        id: Auto::default(),
        meas: rustango::sql::ForeignKey::unloaded(m.id.get().copied().expect("id")),
    }
    .insert_pool(pool)
    .await
    .expect("seed reading");
}

fn post(slug: &str, parent_id: Option<i64>) -> Post {
    Post {
        id: Auto::default(),
        slug: slug.into(),
        title: slug.into(),
        parent_id,
    }
}

async fn setup(pool: &Pool) {
    fresh_table::<Code>(pool).await;
    fresh_table::<Post>(pool).await;
    fresh_table::<Blob>(pool).await;
    rustango::testkit::matrix::drop_table(pool, Reading::SCHEMA.table).await;
    fresh_table::<Meas>(pool).await;
    fresh_table::<Reading>(pool).await;
    fresh_table::<Noted>(pool).await;
}

async fn posts(pool: &Pool) -> Vec<Post> {
    Post::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("fetch posts")
}

/// The PK a single-row insert reports, whatever the backend's shape.
fn reported_id(r: InsertReturningPool) -> i64 {
    #[allow(unused_imports)]
    use rustango::sql::sqlx::Row as _;
    #[allow(unreachable_patterns)]
    match r {
        #[cfg(feature = "postgres")]
        InsertReturningPool::PgRow(row) => row.try_get("id").expect("id"),
        #[cfg(feature = "mysql")]
        InsertReturningPool::MySqlAutoId(id) => id,
        #[cfg(feature = "sqlite")]
        InsertReturningPool::SqliteRow(row) => row.try_get("id").expect("id"),
        _ => unreachable!(),
    }
}

/// #1887: MySQL wrote `id = id`, so a model without an `id` column failed.
async fn insert_or_ignore_on_a_natural_pk(pool: &Pool) {
    let first = Code {
        code: "a".into(),
        n: 1,
    };
    assert!(first.insert_or_ignore(pool).await.expect("first insert"));
    let dup = Code {
        code: "a".into(),
        n: 2,
    };
    assert!(
        !dup.insert_or_ignore(pool).await.expect("duplicate insert"),
        "a skipped row must report false"
    );
    let rows: Vec<Code> = Code::objects().fetch(pool).await.expect("fetch");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].n, 1, "the duplicate must not overwrite");
}

/// #1887: MySQL counted the skip as an affected row and said true.
async fn insert_or_ignore_reports_a_skip_on_an_auto_pk(pool: &Pool) {
    assert!(post("x", None)
        .insert_or_ignore(pool)
        .await
        .expect("insert"));
    assert!(
        !post("x", None).insert_or_ignore(pool).await.expect("dup"),
        "a skipped row must report false"
    );
    assert_eq!(Post::objects().count(pool).await.expect("count"), 1);

    // A skip returns no row, as an empty RETURNING does.
    let q = InsertQuery::new(
        Post::SCHEMA,
        vec!["slug", "title"],
        vec![SqlValue::from("x"), SqlValue::from("again")],
    )
    .returning(vec!["id"])
    .on_conflict(ConflictClause::DoNothing);
    let got = rustango::sql::insert_returning_pool(pool, &q).await;
    assert!(is_row_not_found(&got), "{got:?}");
}

fn is_row_not_found<T>(r: &Result<T, rustango::sql::ExecError>) -> bool {
    matches!(
        r,
        Err(rustango::sql::ExecError::Driver(
            rustango::sql::sqlx::Error::RowNotFound
        ))
    )
}

/// A skip in a transaction is `RowNotFound`, and the skip's MySQL session
/// id must not leak into the next insert's reported PK.
async fn skip_in_a_tx_leaves_no_stale_id(pool: &Pool) {
    post("x", None).insert_pool(pool).await.expect("seed");
    let skip = InsertQuery::new(
        Post::SCHEMA,
        vec!["slug", "title"],
        vec![SqlValue::from("x"), SqlValue::from("again")],
    )
    .returning(vec!["id"])
    .on_conflict(ConflictClause::DoNothing);
    let explicit = InsertQuery::new(
        Post::SCHEMA,
        vec!["id", "slug", "title"],
        vec![SqlValue::I64(50), SqlValue::from("y"), SqlValue::from("y")],
    )
    .returning(vec!["id"]);
    let mut tx = rustango::sql::transaction_pool(pool).await.expect("begin");
    let got = rustango::sql::insert_returning_tx(&mut tx, &skip).await;
    assert!(is_row_not_found(&got), "{got:?}");
    let r = rustango::sql::insert_returning_tx(&mut tx, &explicit)
        .await
        .expect("explicit pk");
    assert_eq!(reported_id(r), 50, "the explicit PK, not a stale id");
    tx.commit().await.expect("commit");
}

/// #1887: MySQL's LAST_INSERT_ID() named a stale row after an update.
async fn upsert_reports_the_updated_row(pool: &Pool) {
    post("a", None).insert_pool(pool).await.expect("seed a");
    post("b", None).insert_pool(pool).await.expect("seed b");
    let a_id = posts(pool).await[0].id.get().copied().expect("a id");

    let q = InsertQuery::new(
        Post::SCHEMA,
        vec!["slug", "title"],
        vec![SqlValue::from("a"), SqlValue::from("A2")],
    )
    .returning(vec!["id"])
    .on_conflict(ConflictClause::DoUpdate {
        target: vec!["slug"],
        update_columns: vec!["title"],
    });
    let r = rustango::sql::insert_returning_pool(pool, &q)
        .await
        .expect("upsert");
    assert_eq!(reported_id(r), a_id, "the id must name the updated row");
    assert_eq!(posts(pool).await[0].title, "A2");
}

/// #1888: PG typed an all-NULL VALUES column as text.
async fn bulk_update_sets_null_in_every_row(pool: &Pool) {
    post("a", Some(5)).insert_pool(pool).await.expect("seed a");
    post("b", Some(6)).insert_pool(pool).await.expect("seed b");
    let mut rows = posts(pool).await;
    for r in &mut rows {
        r.parent_id = None;
    }
    let n = Post::bulk_update(&rows, &["parent_id"], pool)
        .await
        .expect("bulk_update to NULL");
    assert_eq!(n, 2);
    assert!(posts(pool).await.iter().all(|p| p.parent_id.is_none()));
}

/// Seed `(slug, title)` rows in order.
async fn seed(pool: &Pool, rows: &[(&str, &str)]) {
    for (slug, title) in rows {
        let mut p = post(slug, None);
        p.title = (*title).into();
        p.insert_pool(pool).await.expect("seed");
    }
}

fn slugs(rows: &[Post]) -> Vec<&str> {
    let mut out: Vec<&str> = rows.iter().map(|p| p.slug.as_str()).collect();
    out.sort_unstable();
    out
}

/// `<table>.id = <alias>.id`, for a derived-table join on the PK.
fn same_id(alias: &'static str) -> WhereExpr {
    WhereExpr::ExprCompare {
        lhs: aliased(alias, "id"),
        op: Op::Eq,
        rhs: aliased("orm_dialect_tri_post", "id"),
    }
}

/// #1890: a union's first branch dropped its derived-table join.
async fn union_keeps_the_first_branch_join(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    let only_a = Post::objects().filter("slug", "a").compile().expect("sub");
    let rows = Post::objects()
        .join_sub(only_a, "s", same_id("s"))
        .union(Post::objects().filter("slug", "b"))
        .fetch(pool)
        .await
        .expect("union");
    assert_eq!(slugs(&rows), ["a", "b"]);
}

/// #1890: `values_list_flat` on a union projected only part of it.
async fn union_values_list_flat_in_a_subquery(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    let ids = Post::objects()
        .filter("slug", "a")
        .union(Post::objects().filter("slug", "b"))
        .values_list_flat("id")
        .compile()
        .expect("ids");
    let rows = Post::objects()
        .where_in_subquery("id", ids)
        .fetch(pool)
        .await
        .expect("IN (union)");
    assert_eq!(slugs(&rows), ["a", "b"]);
}

/// #1890: a paginated union counted its first branch only.
async fn paginated_union_counts_every_branch(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    let qs = Post::objects()
        .filter("slug", "a")
        .union(Post::objects().filter("slug", "b"))
        .order_by(&[("id", false)])
        .limit(1);
    let page = rustango::sql::fetch_paginated_pool(qs, pool)
        .await
        .expect("paginated union");
    assert_eq!(page.total, 2, "the total spans both branches");
    assert_eq!(slugs(&page.rows), ["a"]);
}

/// #1890: a union's page with an offset still counts every branch.
async fn paginated_union_with_an_offset(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    let union = || {
        Post::objects()
            .filter("slug", "a")
            .union(Post::objects().filter("slug", "b"))
            .union(Post::objects().filter("slug", "c"))
            .order_by(&[("id", false)])
    };
    let page = rustango::sql::fetch_paginated_pool(union().offset(1).limit(1), pool)
        .await
        .expect("offset + limit");
    assert_eq!(page.total, 3);
    assert_eq!(slugs(&page.rows), ["b"]);
    // SQLite needs a LIMIT before OFFSET.
    let page = rustango::sql::fetch_paginated_pool(union().offset(1), pool)
        .await
        .expect("offset only");
    assert_eq!(page.total, 3);
    assert_eq!(slugs(&page.rows), ["b", "c"]);
}

/// #1890: a union's first branch dropped its DISTINCT.
async fn union_all_keeps_the_first_branch_distinct(pool: &Pool) {
    seed(pool, &[("a", "apple"), ("b", "apple"), ("c", "banana")]).await;
    let mut titles = Post::objects()
        .filter("title", "apple")
        .distinct()
        .union_all(Post::objects().filter("slug", "c"))
        .values_list(&["title"])
        .fetch(pool)
        .await
        .expect("union all");
    titles.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(
        titles,
        [
            vec![SqlValue::from("apple")],
            vec![SqlValue::from("banana")]
        ]
    );
}

/// #1890: the MySQL / SQLite `distinct_on` fallback dropped search and
/// derived-table joins.
async fn distinct_on_keeps_search_and_derived_joins(pool: &Pool) {
    seed(pool, &[("a", "apple"), ("b", "apple"), ("c", "banana")]).await;
    let by_title = || {
        Post::objects()
            .distinct_on(&["title"])
            .order_by(&[("title", false), ("id", false)])
    };

    let mut q = by_title().compile().expect("compile");
    q.search = Some(SearchClause {
        columns: vec!["title"],
        query: "ban".into(),
    });
    let rows: Vec<Post> = rustango::sql::select_rows_pool(pool, &q)
        .await
        .expect("search");
    assert_eq!(slugs(&rows), ["c"]);

    let only_b = Post::objects().filter("slug", "b").compile().expect("sub");
    let rows = by_title()
        .join_sub(only_b, "s", same_id("s"))
        .fetch(pool)
        .await
        .expect("join");
    assert_eq!(slugs(&rows), ["b"]);
}

/// #1966: a DISTINCT page counted the rows before DISTINCT.
async fn paginated_distinct_counts_distinct_rows(pool: &Pool) {
    seed_meas(pool, 1, "2024-01-01T00:00:00Z").await;
    seed_meas(pool, 2, "2024-01-02T00:00:00Z").await;
    let first = Meas::objects().fetch(pool).await.expect("meas")[0].id;
    Reading {
        id: Auto::default(),
        meas: rustango::sql::ForeignKey::unloaded(first.get().copied().expect("id")),
    }
    .insert_pool(pool)
    .await
    .expect("second reading");
    // Joined to its 3 readings, the 2 measurements are 3 rows before DISTINCT.
    let readings = Reading::objects().compile().expect("sub");
    let on = WhereExpr::ExprCompare {
        lhs: aliased("r", "meas"),
        op: Op::Eq,
        rhs: aliased("orm_dialect_tri_meas", "id"),
    };
    let qs = Meas::objects()
        .join_sub(readings, "r", on)
        .distinct()
        .order_by(&[("id", false)])
        .limit(1);
    let page = rustango::sql::fetch_paginated_pool(qs, pool)
        .await
        .expect("paginated distinct");
    assert_eq!((page.total, page.rows.len()), (2, 1));

    // The count subquery repeats the WHERE binds; swapped, `n >= 3 AND n <= 2`.
    seed_meas(pool, 3, "2024-01-03T00:00:00Z").await;
    let second = Meas::objects()
        .filter("n", 2_i64)
        .fetch(pool)
        .await
        .expect("n=2")[0]
        .id;
    Reading {
        id: Auto::default(),
        meas: rustango::sql::ForeignKey::unloaded(second.get().copied().expect("id")),
    }
    .insert_pool(pool)
    .await
    .expect("another reading");
    let readings = Reading::objects().compile().expect("sub");
    let on = WhereExpr::ExprCompare {
        lhs: aliased("r", "meas"),
        op: Op::Eq,
        rhs: aliased("orm_dialect_tri_meas", "id"),
    };
    let qs = Meas::objects()
        .join_sub(readings, "r", on)
        .filter_op("n", Op::Gte, 2_i64)
        .filter_op("n", Op::Lte, 3_i64)
        .distinct()
        .order_by(&[("id", false)])
        .offset(1)
        .limit(1);
    let page = rustango::sql::fetch_paginated_pool(qs, pool)
        .await
        .expect("filtered distinct page");
    let ns: Vec<i64> = page.rows.iter().map(|m| m.n).collect();
    assert_eq!((page.total, ns), (2, vec![3]));

    seed(pool, &[("a", "apple"), ("b", "apple"), ("c", "banana")]).await;
    let qs = Post::objects()
        .distinct_on(&["title"])
        .order_by(&[("title", false), ("id", false)])
        .limit(1);
    let page = rustango::sql::fetch_paginated_pool(qs, pool)
        .await
        .expect("paginated distinct_on");
    assert_eq!((page.total, slugs(&page.rows)), (2, vec!["a"]));
}

/// #1890: `paginate()` sent no ORDER BY, so a page followed heap order.
async fn paginate_orders_by_pk(pool: &Pool) {
    seed(pool, &[("a", "a"), ("b", "b"), ("c", "c")]).await;
    // An UPDATE moves the row to the end of a PostgreSQL heap.
    let mut a = posts(pool).await.remove(0);
    a.title = "a2".into();
    a.save_pool(pool).await.expect("update a");
    let (rows, total) = Post::objects()
        .paginate(1, 2, pool)
        .await
        .expect("paginate");
    assert_eq!(total, 3);
    let got: Vec<&str> = rows.iter().map(|p| p.slug.as_str()).collect();
    assert_eq!(got, ["a", "b"], "the first page is the two lowest PKs");
}

/// #1901: `values()` read a Uuid or bytes column as Null (SQLite and PG)
/// or a Uuid as text (MySQL).
async fn values_decode_uuid_and_bytes(pool: &Pool) {
    let tok = uuid::uuid!("6f1c2a4e-9b7d-4c3a-8e21-0d5f4b6a7c89");
    let mut b = Blob {
        id: Auto::default(),
        token: tok,
        data: vec![0, 1, 255],
    };
    b.insert_pool(pool).await.expect("seed");
    let want = [SqlValue::Uuid(tok), SqlValue::Binary(vec![0, 1, 255])];

    let list = Blob::objects()
        .values_list(&["token", "data"])
        .fetch(pool)
        .await
        .expect("values_list");
    assert_eq!(list, [want.to_vec()]);

    let dict = Blob::objects()
        .values_dict(&["token", "data"])
        .fetch(pool)
        .await
        .expect("values_dict");
    assert_eq!(dict[0]["token"], want[0]);
    assert_eq!(dict[0]["data"], want[1]);

    let agg = Blob::objects()
        .values(&["token"])
        .annotate("n", AggregateExpr::Count(None))
        .fetch(pool)
        .await
        .expect("aggregate");
    assert_eq!(agg[0]["token"], want[0]);
}

/// #2229: search and `__icontains` on an int or UUID column failed on PG.
async fn ilike_on_int_and_uuid_columns(pool: &Pool) {
    let mut ids = Vec::new();
    for tok in [
        uuid::uuid!("6f1c2a4e-9b7d-4c3a-8e21-0d5f4b6a7c89"),
        uuid::uuid!("00000000-0000-4000-8000-000000000000"),
    ] {
        let mut b = Blob {
            id: Auto::default(),
            token: tok,
            data: vec![],
        };
        b.insert_pool(pool).await.expect("seed");
        ids.push(*b.id.get().expect("id"));
    }
    let got = |rows: Vec<Blob>| -> Vec<i64> { rows.iter().map(|b| *b.id.get().unwrap()).collect() };

    for (col, query, want) in [
        ("id", ids[1].to_string(), ids[1]),
        ("token", "2A4E-9B7D".to_owned(), ids[0]),
    ] {
        let mut q = Blob::objects().compile().expect("compile");
        q.search = Some(SearchClause {
            columns: vec![col],
            query: query.clone(),
        });
        let rows = rustango::sql::select_rows_pool(pool, &q).await;
        assert_eq!(got(rows.expect("search")), [want], "search {col}");

        // The lookup builder rejects a string on these fields; the IR does not.
        let mut q = Blob::objects().compile().expect("compile");
        q.where_clause =
            WhereExpr::Predicate(Filter::new(col, Op::ILikeEscaped, format!("%{query}%")));
        let rows = rustango::sql::select_rows_pool(pool, &q).await;
        assert_eq!(got(rows.expect("ilike")), [want], "ilike {col}");
    }
}

/// #2232: MySQL inlined the delimiter with only `'` doubled, so a `\`
/// broke the statement.
async fn string_agg_delimiter_with_backslash(pool: &Pool) {
    seed(pool, &[("a", "x"), ("b", "y")]).await;
    for delim in ["\\", "\\'), x", "'\\'"] {
        let rows = Post::objects()
            .aggregate()
            .values(&[])
            .annotate(
                "t",
                AggregateExpr::string_agg_ordered("title", delim, &[("title", false)]),
            )
            .fetch(pool)
            .await
            .expect("string_agg");
        assert_eq!(
            rows[0]["t"],
            SqlValue::String(format!("x{delim}y")),
            "{delim}"
        );
    }
}

/// #2232: a comment ending in `\` must create the table and read back as written.
async fn comment_with_backslash(pool: &Pool) {
    if pool.dialect().name() != "mysql" {
        return; // `setup` already created the table on every backend.
    }
    let sql = format!(
        "SELECT CAST(COLUMN_COMMENT AS CHAR) FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = {} AND COLUMN_NAME = 'n'",
        pool.dialect().placeholder(1)
    );
    let got: Vec<(String,)> = rustango::sql::raw_query_pool(
        &sql,
        vec![SqlValue::String(Noted::SCHEMA.table.into())],
        pool,
    )
    .await
    .expect("read comment");
    assert_eq!(got, [("C:\\path 'q' \\'\\".to_owned(),)]);
}

/// #2004: `values()` read date and timestamp cells as Null on PG and MySQL.
async fn values_decode_dates_and_timestamps(pool: &Pool) {
    if pool.dialect().name() == "sqlite" {
        return; // SQLite stores both as text.
    }
    seed_meas(pool, 0, "2024-01-06T12:30:00Z").await;
    let dict = Meas::objects()
        .values_dict(&["at", "taken_on"])
        .fetch(pool)
        .await
        .expect("values_dict");
    let at: chrono::DateTime<chrono::Utc> = "2024-01-06T12:30:00Z".parse().unwrap();
    assert_eq!(dict[0]["at"], SqlValue::DateTime(at));
    assert_eq!(dict[0]["taken_on"], SqlValue::Date(at.date_naive()));
}

/// #1900: MySQL's `/` gave 3.5 for two integers, stored as 4.
async fn integer_division_truncates(pool: &Pool) {
    use rustango::core::{BinOp, Expr};
    seed_meas(pool, 7, "2024-01-01T00:00:00Z").await;
    let half = Expr::BinOp {
        left: Box::new(Expr::Column("n")),
        op: BinOp::Div,
        right: Box::new(Expr::Literal(SqlValue::I64(2))),
    };
    Meas::objects()
        .update()
        .set_expr("n", half)
        .execute_pool(pool)
        .await
        .expect("update");
    let rows: Vec<Meas> = Meas::objects().fetch(pool).await.expect("fetch");
    assert_eq!(rows[0].n, 3);
}

/// #1900: PostgreSQL's `__second` rounded 59.7 up to 60.
async fn second_lookup_truncates(pool: &Pool) {
    seed_meas(pool, 0, "2024-01-01T10:00:59.7Z").await;
    let n = Meas::objects()
        .filter("at__second", 59_i64)
        .count(pool)
        .await
        .expect("count");
    assert_eq!(n, 1);
}

/// #1900: a date lookup across a relation reads the joined column.
async fn relation_date_lookup(pool: &Pool) {
    seed_meas(pool, 0, "2024-01-06T23:30:00Z").await;
    let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 6).unwrap();
    for (key, v) in [
        ("meas__at__date", SqlValue::Date(day)),
        ("meas__at__hour", SqlValue::I64(23)),
        ("meas__taken_on__day", SqlValue::I64(6)),
    ] {
        let n = Reading::objects().filter(key, v).count(pool).await;
        assert_eq!(n.expect("count"), 1, "{key}");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        insert_or_ignore_on_a_natural_pk,
        insert_or_ignore_reports_a_skip_on_an_auto_pk,
        skip_in_a_tx_leaves_no_stale_id,
        upsert_reports_the_updated_row,
        bulk_update_sets_null_in_every_row,
        union_keeps_the_first_branch_join,
        union_values_list_flat_in_a_subquery,
        paginated_union_counts_every_branch,
        paginated_union_with_an_offset,
        union_all_keeps_the_first_branch_distinct,
        distinct_on_keeps_search_and_derived_joins,
        paginate_orders_by_pk,
        paginated_distinct_counts_distinct_rows,
        values_decode_uuid_and_bytes,
        ilike_on_int_and_uuid_columns,
        string_agg_delimiter_with_backslash,
        comment_with_backslash,
        values_decode_dates_and_timestamps,
        integer_division_truncates,
        second_lookup_truncates,
        relation_date_lookup,
    ],
}

/// #1900: PostgreSQL date lookups followed the session TimeZone, while
/// MySQL and SQLite read the stored UTC value.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn pg_date_lookups_use_utc() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    seed_meas(&pool, 0, "2024-01-06T23:30:00Z").await;
    let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 6).unwrap();
    // sqlx starts every session in UTC; SET LOCAL ends with the tx.
    let mut tx = pool.as_postgres().expect("pg").begin().await.unwrap();
    rustango::sql::sqlx::query("SET LOCAL TIME ZONE 'Europe/Kyiv'")
        .execute(&mut *tx)
        .await
        .unwrap();
    for (key, v) in [
        ("at__date", SqlValue::Date(day)),
        ("at__day", SqlValue::I64(6)),
        ("at__hour", SqlValue::I64(23)),
        ("at__week_day", SqlValue::I64(6)),
    ] {
        let n = Meas::objects()
            .filter(key, v.clone())
            .count_on(&mut *tx)
            .await;
        assert_eq!(n.unwrap(), 1, "{key}");
        // The same lookup through a relation reads the joined column.
        let span = format!("meas__{key}");
        let n = Reading::objects().filter(&span, v).count_on(&mut *tx).await;
        assert_eq!(n.unwrap(), 1, "{span}");
    }
}

/// #1900: a DATE has no zone, so the UTC shift must skip it. On a Tokyo
/// session `date AT TIME ZONE 'UTC'` is the day before.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn pg_date_column_is_not_shifted() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    seed_meas(&pool, 0, "2024-01-06T12:00:00Z").await;
    let mut tx = pool.as_postgres().expect("pg").begin().await.unwrap();
    rustango::sql::sqlx::query("SET LOCAL TIME ZONE 'Asia/Tokyo'")
        .execute(&mut *tx)
        .await
        .unwrap();
    for key in ["taken_on__day", "meas__taken_on__day"] {
        let n = if key.starts_with("meas") {
            Reading::objects()
                .filter(key, 6_i64)
                .count_on(&mut *tx)
                .await
        } else {
            Meas::objects().filter(key, 6_i64).count_on(&mut *tx).await
        };
        assert_eq!(n.unwrap(), 1, "{key}");
    }
}

/// #2004: a PG `timestamp` (no zone) cell decodes as UTC, not Null.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn pg_values_decode_a_timestamp_without_zone() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    seed_meas(&pool, 0, "2024-01-06T12:30:00Z").await;
    let pg = pool.as_postgres().expect("pg");
    // The model maps `at` to `timestamptz`; turn it into a zoneless `timestamp`.
    rustango::sql::sqlx::query(
        "ALTER TABLE orm_dialect_tri_meas ALTER COLUMN at TYPE timestamp USING at AT TIME ZONE 'UTC'",
    )
    .execute(pg)
    .await
    .unwrap();
    let dict = Meas::objects()
        .values_dict(&["at"])
        .fetch(&pool)
        .await
        .expect("values_dict");
    let at: chrono::DateTime<chrono::Utc> = "2024-01-06T12:30:00Z".parse().unwrap();
    assert_eq!(dict[0]["at"], SqlValue::DateTime(at));
}

/// #1935: `upsert` took a field `index(unique)` or a partial unique index
/// as its conflict target. `upsert` is PostgreSQL-only.
#[cfg(feature = "postgres")]
mod upsert_target_pg {
    use super::*;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "orm_dialect_tri_tagged")]
    #[rustango(app = "orm_dialect_tri")]
    pub struct Tagged {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 64, index(unique))]
        pub slug: String,
    }

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "orm_dialect_tri_partial")]
    #[rustango(app = "orm_dialect_tri")]
    #[rustango(unique_when(columns = "slug", condition = "slug <> ''"))]
    pub struct Partial {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 64)]
        pub slug: String,
    }

    async fn pg_pool() -> Option<Pool> {
        let pool = rustango::testkit::matrix::Backend::Postgres.pool().await;
        if pool.is_none() {
            eprintln!("DATABASE_URL not set — skipping");
        }
        pool
    }

    /// An upsert on a set PK must update that row, not insert a second.
    #[tokio::test]
    async fn upsert_ignores_a_field_unique_index() {
        let _guard = rustango::testkit::matrix::live_lock().lock().await;
        let Some(pool) = pg_pool().await else { return };
        fresh_table::<Tagged>(&pool).await;
        let mut t = Tagged {
            id: Auto::default(),
            slug: "a".into(),
        };
        t.insert_pool(&pool).await.expect("seed");
        t.slug = "b".into();
        t.upsert(pool.as_postgres().expect("pg"))
            .await
            .expect("upsert");
        let rows: Vec<Tagged> = Tagged::objects().fetch(&pool).await.expect("fetch");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "b");
    }

    #[tokio::test]
    async fn upsert_ignores_a_partial_unique_index() {
        let _guard = rustango::testkit::matrix::live_lock().lock().await;
        let Some(pool) = pg_pool().await else { return };
        fresh_table::<Partial>(&pool).await;
        let mut p = Partial {
            id: Auto::default(),
            slug: "a".into(),
        };
        p.insert_pool(&pool).await.expect("seed");
        p.slug = "b".into();
        p.upsert(pool.as_postgres().expect("pg"))
            .await
            .expect("upsert");
        let rows: Vec<Partial> = Partial::objects().fetch(&pool).await.expect("fetch");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "b");
    }
}

/// #1899: NUMERIC affinity stores a whole decimal as INTEGER, which the
/// SQLite row decoder (admin, API) showed as null.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_whole_decimal_is_not_null() {
    let pool = rustango::sql::sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    let row = rustango::sql::sqlx::query("SELECT CAST('7' AS NUMERIC) AS n")
        .fetch_one(&pool)
        .await
        .expect("row");
    let mut f = *Code::SCHEMA.field("n").expect("field");
    f.ty = rustango::core::FieldType::Decimal;
    let f: &'static _ = Box::leak(Box::new(f));
    let json = rustango::sql::row_to_json_sqlite(&row, &[f]);
    assert_eq!(json["n"], "7");
}
