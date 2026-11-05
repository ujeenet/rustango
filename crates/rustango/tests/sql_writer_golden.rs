//! Byte-for-byte SQL text and bind order of a broad query set on all
//! three dialects, against a fixture captured before the writer went
//! alloc-free. Re-bless with `RUSTANGO_BLESS_SQL_GOLDEN=1` only on purpose.

use rustango::core::aggregates::{count_all, sum};
use rustango::core::case::{case, value};
use rustango::core::{
    Assignment, BulkInsertQuery, BulkUpdateQuery, Column as _, ConflictClause, CountQuery,
    DeleteQuery, Filter, InsertQuery, Model as _, Op, SelectQuery, SqlValue, UpdateQuery,
    WhereExpr, F,
};
use rustango::query::Q;
use rustango::sql::{CompiledStatement, Dialect, ForeignKey, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "gold_author", app = "sql_writer_golden")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 100)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gold_category", app = "sql_writer_golden")]
pub struct Category {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 50)]
    pub slug: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gold_post", app = "sql_writer_golden")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    pub author: ForeignKey<Author, i64>,
    pub category: Option<ForeignKey<Category, i64>>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 50)]
    pub slug: String,
    pub body: String,
    pub views: i64,
    pub published: bool,
    pub created: chrono::DateTime<chrono::Utc>,
    pub data: serde_json::Value,
}

type Case = (
    &'static str,
    Box<dyn Fn(&dyn Dialect) -> Result<CompiledStatement, SqlError>>,
);

fn sel(q: SelectQuery) -> Box<dyn Fn(&dyn Dialect) -> Result<CompiledStatement, SqlError>> {
    Box::new(move |d| d.compile_select(&q))
}

fn list(values: &[i64]) -> SqlValue {
    SqlValue::List(values.iter().map(|v| SqlValue::I64(*v)).collect())
}

fn ts() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn cases() -> Vec<Case> {
    let mut c: Vec<Case> = Vec::new();
    // The blog-index shapes.
    c.push((
        "index_list_select_related",
        sel(Post::objects()
            .select_related("author")
            .select_related("category")
            .filter("published", true)
            .order_by(&[("created", true)])
            .limit(20)
            .offset(40)
            .compile()
            .unwrap()),
    ));
    c.push((
        "get_by_slug",
        sel(Post::objects()
            .filter("slug", "post-500")
            .limit(2)
            .compile()
            .unwrap()),
    ));
    c.push((
        "values_list",
        sel(Post::objects()
            .filter("published", true)
            .order_by(&[("views", true)])
            .values_list(&["id", "slug", "title"])
            .compile()
            .unwrap()),
    ));
    // Lookups.
    let lookups: &[(&str, SqlValue)] = &[
        ("title__iexact", "Hi".into()),
        ("title__contains", "a%b_c\\d".into()),
        ("title__icontains", "rust".into()),
        ("title__startswith", "r".into()),
        ("title__istartswith", "r".into()),
        ("title__endswith", "s".into()),
        ("title__iendswith", "s".into()),
        ("title__like", "%x%".into()),
        ("title__not_ilike", "%x%".into()),
        ("views__gt", 1_i64.into()),
        ("views__gte", 1_i64.into()),
        ("views__lt", 1_i64.into()),
        ("views__lte", 1_i64.into()),
        ("views__ne", 1_i64.into()),
        ("views__in", list(&[1, 2, 3])),
        ("views__not_in", list(&[4, 5])),
        ("views__in", list(&[])),
        ("category__isnull", true.into()),
        ("category__isnull", false.into()),
        ("views__between", list(&[1, 9])),
        ("views__not_range", list(&[1, 9])),
        ("created__year", 2024_i32.into()),
        ("created__month__gte", 6_i32.into()),
        ("created__date", ts().date_naive().into()),
        ("created__gt", ts().into()),
        ("author__name__icontains", "ann".into()),
        (
            "data__contains",
            SqlValue::Json(serde_json::json!({"k": 1})),
        ),
        ("data", SqlValue::Json(serde_json::json!([1, 2]))),
        ("category", SqlValue::Null),
    ];
    for (key, v) in lookups {
        let q = Post::objects().filter(key, v.clone()).compile();
        let q = match q {
            Ok(q) => q,
            Err(e) => panic!("{key}: {e}"),
        };
        c.push((key, sel(q)));
    }
    let strs = |v: &[&str]| SqlValue::List(v.iter().map(|s| SqlValue::from(*s)).collect());
    let ops: &[(&str, &str, Op, SqlValue)] = &[
        ("op_json_has_key", "data", Op::JsonHasKey, "k".into()),
        (
            "op_json_has_any",
            "data",
            Op::JsonHasAnyKey,
            strs(&["a", "b"]),
        ),
        (
            "op_json_has_all",
            "data",
            Op::JsonHasAllKeys,
            strs(&["a", "b"]),
        ),
        (
            "op_json_contained_by",
            "data",
            Op::JsonContainedBy,
            SqlValue::Json(serde_json::json!({"k": [1]})),
        ),
        ("op_regex", "title", Op::Regex, "^a".into()),
        ("op_not_iregex", "title", Op::NotIRegex, "^a".into()),
        (
            "op_is_distinct_from",
            "views",
            Op::IsDistinctFrom,
            3_i64.into(),
        ),
        (
            "op_is_not_distinct_from",
            "category",
            Op::IsNotDistinctFrom,
            SqlValue::Null,
        ),
        ("op_like_escaped", "title", Op::LikeEscaped, "a%".into()),
        ("op_ilike_escaped", "title", Op::ILikeEscaped, "a_".into()),
    ];
    for (name, field, op, v) in ops {
        let q = Post::objects().filter_op(*field, *op, v.clone()).compile();
        match q {
            Ok(q) => c.push((name, sel(q))),
            Err(e) => panic!("{name}: {e}"),
        }
    }
    c.push((
        "exclude_and_q_or",
        sel(Post::objects()
            .exclude("views__lt", 10_i64)
            .where_raw(
                (Q::eq("slug", "a") | (Q::gt("views", 3_i64) & !Q::like("title", "x%"))).into(),
            )
            .compile()
            .unwrap()),
    ));
    c.push((
        "xor",
        sel(
            SelectQuery::new(Post::SCHEMA).where_clause(WhereExpr::Xor(vec![
                Post::slug.eq("a").into(),
                Post::published.eq(true).into(),
            ])),
        ),
    ));
    c.push((
        "distinct_order_random",
        sel(Post::objects().distinct().order_random().compile().unwrap()),
    ));
    c.push((
        "distinct_on",
        sel(Post::objects()
            .distinct_on(&["author"])
            .order_by(&[("author", false), ("created", true)])
            .compile()
            .unwrap()),
    ));
    c.push((
        "offset_only",
        sel(Post::objects().offset(5).compile().unwrap()),
    ));
    c.push((
        "union",
        sel(Post::objects()
            .filter("views__gt", 1_i64)
            .union(Post::objects().filter("slug", "z"))
            .compile()
            .unwrap()),
    ));
    c.push((
        "only_defer",
        sel(Post::objects().defer(&["body", "data"]).compile().unwrap()),
    ));
    c.push((
        "in_subquery",
        sel(Post::objects()
            .where_in_subquery(
                "author",
                Author::objects()
                    .filter("name__startswith", "A")
                    .values_list_flat("id")
                    .compile()
                    .unwrap(),
            )
            .compile()
            .unwrap()),
    ));
    c.push((
        "exists",
        sel(Author::objects()
            .where_exists(Post::objects().filter("published", true).compile().unwrap())
            .compile()
            .unwrap()),
    ));
    // Aggregates and counts.
    let agg = Post::objects()
        .filter("published", true)
        .aggregate()
        .group_by("author")
        .annotate("n", count_all().into())
        .annotate("total", sum("views").into())
        .filter("n", Op::Gt, 2_i64)
        .compile()
        .unwrap();
    c.push((
        "aggregate_having",
        Box::new(move |d| d.compile_aggregate(&agg)),
    ));
    let count = CountQuery::from_select(
        Post::objects()
            .filter("views__gt", 5_i64)
            .compile()
            .unwrap(),
    );
    c.push(("count", Box::new(move |d| d.compile_count(&count))));
    let count_join = CountQuery::from_select(
        Post::objects()
            .select_related("author")
            .limit(10)
            .compile()
            .unwrap(),
    );
    c.push((
        "count_join_limit",
        Box::new(move |d| d.compile_count(&count_join)),
    ));
    let exists = CountQuery::exists(Post::objects().filter("slug", "x").compile().unwrap());
    c.push(("exists_count", Box::new(move |d| d.compile_count(&exists))));
    // Writes.
    let mut ins = InsertQuery::new(
        Post::SCHEMA,
        vec!["author", "title", "slug", "views", "published", "data"],
        vec![
            1_i64.into(),
            "T".into(),
            "t".into(),
            0_i64.into(),
            false.into(),
            SqlValue::Json(serde_json::json!({"a": null})),
        ],
    );
    ins.returning = vec!["id"];
    c.push((
        "insert_returning",
        Box::new(move |d| d.compile_insert(&ins)),
    ));
    let mut ins_conflict = InsertQuery::new(
        Post::SCHEMA,
        vec!["id", "slug", "views"],
        vec![1_i64.into(), "s".into(), 2_i64.into()],
    );
    ins_conflict.on_conflict = Some(ConflictClause::DoUpdate {
        target: vec!["id"],
        update_columns: vec!["slug", "views"],
    });
    c.push((
        "insert_upsert",
        Box::new(move |d| d.compile_insert(&ins_conflict)),
    ));
    let mut bulk = BulkInsertQuery::new(
        Post::SCHEMA,
        vec!["id", "slug", "category"],
        vec![
            vec![1_i64.into(), "a".into(), SqlValue::Null],
            vec![2_i64.into(), "b".into(), 3_i64.into()],
        ],
    );
    bulk.on_conflict = Some(ConflictClause::DoNothing);
    c.push((
        "bulk_insert_ignore",
        Box::new(move |d| d.compile_bulk_insert(&bulk)),
    ));
    let bulk_up = BulkUpdateQuery::new(
        Post::SCHEMA,
        vec!["slug", "views"],
        vec![
            vec![1_i64.into(), "a".into(), 5_i64.into()],
            vec![2_i64.into(), "b".into(), 6_i64.into()],
        ],
    );
    c.push((
        "bulk_update",
        Box::new(move |d| d.compile_bulk_update(&bulk_up)),
    ));
    let upd = UpdateQuery::new(
        Post::SCHEMA,
        vec![
            Assignment::new("views", F("views") + 1_i64),
            Assignment::new(
                "title",
                case()
                    .when(Post::published.eq(true), value("P"))
                    .default(value("D")),
            ),
            Assignment::new("category", SqlValue::Null),
        ],
        WhereExpr::Predicate(Filter::new("id", Op::In, list(&[1, 2]))),
    );
    c.push((
        "update_expr_case",
        Box::new(move |d| d.compile_update(&upd)),
    ));
    let upd_qs = Post::objects()
        .filter("slug__startswith", "old")
        .update()
        .set("published", false)
        .compile()
        .unwrap();
    c.push((
        "update_queryset",
        Box::new(move |d| d.compile_update(&upd_qs)),
    ));
    let upd_limited = Post::objects()
        .order_by(&[("created", false)])
        .limit(3)
        .update()
        .set("views", 0_i64)
        .compile();
    if let Ok(q) = upd_limited {
        c.push(("update_limited", Box::new(move |d| d.compile_update(&q))));
    }
    let del = DeleteQuery::new(
        Post::SCHEMA,
        WhereExpr::Predicate(Filter::new("views", Op::Lt, SqlValue::I64(0))),
    );
    c.push(("delete", Box::new(move |d| d.compile_delete(&del))));
    let del_qs = Post::objects().filter("author__name", "x").compile_delete();
    if let Ok(q) = del_qs {
        c.push((
            "delete_queryset_rel",
            Box::new(move |d| d.compile_delete(&q)),
        ));
    }
    let weird = SelectQuery::new(Author::SCHEMA).where_clause(WhereExpr::Predicate(Filter::new(
        "name",
        Op::Eq,
        SqlValue::String("q\"`'".into()),
    )));
    c.push(("string_with_quotes", sel(weird)));
    let ident = SelectQuery::new(Author::SCHEMA).where_clause(WhereExpr::Predicate(Filter::new(
        "we\"ird`col",
        Op::Eq,
        SqlValue::I64(1),
    )));
    c.push(("ident_with_quotes", sel(ident)));
    c
}

fn render() -> String {
    let dialects: [&dyn Dialect; 3] = [&Postgres, &MySql, &Sqlite];
    let mut out = String::new();
    for (name, f) in cases() {
        for d in dialects {
            out.push_str(&format!("== {name} / {} ==\n", d.name()));
            match f(d) {
                Ok(s) => out.push_str(&format!("{}\n{:?}\n", s.sql, s.params)),
                Err(e) => out.push_str(&format!("ERR {e}\n")),
            }
        }
    }
    out
}

#[test]
fn sql_text_matches_the_golden_fixture() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sql_writer_golden.txt"
    );
    let got = render();
    if std::env::var_os("RUSTANGO_BLESS_SQL_GOLDEN").is_some() {
        std::fs::write(path, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(path).unwrap();
    for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
        assert_eq!(g, w, "line {} differs", i + 1);
    }
    assert_eq!(got, want, "line count differs");
}
