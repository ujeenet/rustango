//! Admin list paging order, filter-keeping links, the mounted prefix,
//! bool checkboxes, soft-deleted rows and filtered facet counts on every
//! backend (#1917 #1916 #1765 #1730 #1918 #2004).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Filter, Op, SqlValue};
use rustango::sql::{Auto, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

const PREFIX: &str = "/adm";

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "adminls_item",
    display = "title",
    admin(
        list_display = "title, flag",
        list_filter = "rank, flag",
        list_per_page = 2,
        ordering = "rank",
        date_hierarchy = "made_on",
        actions = "delete_selected, restore_selected"
    )
)]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    pub flag: bool,
    pub rank: i64,
    pub made_on: chrono::NaiveDate,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn kind_filters(value: &str) -> Vec<Filter> {
    match value {
        "low" => vec![Filter::new("rank", Op::Eq, SqlValue::I64(0))],
        _ => Vec::new(),
    }
}
rustango::register_admin_list_filter!(
    "adminls_item",
    "kind",
    "Kind",
    &[("low", "Low")],
    kind_filters,
);

async fn get(pool: &Pool, uri: &str) -> String {
    let (status, body) = send(pool, Request::builder().uri(uri), Body::empty()).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

async fn post(pool: &Pool, uri: &str, form: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    send(pool, req, Body::from(form.to_owned())).await.0
}

async fn send(pool: &Pool, req: axum::http::request::Builder, body: Body) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix(PREFIX)
        .build();
    let res = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    // Tera escapes `/` in `{{ admin_prefix }}`; undo it to read hrefs.
    let body = String::from_utf8_lossy(&bytes)
        .replace("&#x2F;", "/")
        .replace("&amp;", "&");
    (status, body)
}

async fn seed(pool: &Pool, title: &str, flag: bool) -> i64 {
    *seed_item(pool, title, flag).await.id.get().expect("pk")
}

async fn seed_item(pool: &Pool, title: &str, flag: bool) -> Item {
    let mut item = Item {
        id: Auto::default(),
        title: title.into(),
        flag,
        rank: 0,
        made_on: chrono::NaiveDate::from_ymd_opt(2024, 3, 9).unwrap(),
        deleted_at: None,
    };
    item.insert_pool(pool).await.expect("insert");
    item
}

/// Every `href` (and `<option data-href>`) into the list in the page's
/// main column, bare paths included. The sidebar's model link is skipped.
fn list_links(body: &str) -> Vec<String> {
    let main = body
        .split_once(r#"<main class="content">"#)
        .map_or(body, |(_, m)| m);
    main.split("href=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .filter(|s| {
            s.split('?')
                .next()
                .is_some_and(|p| p.ends_with("/adminls_item"))
        })
        .map(str::to_owned)
        .collect()
}

/// Equal `rank`s tie; the PK breaks the tie, even after an UPDATE moved
/// row one to the end of the PG heap.
async fn equal_sort_keys_page_in_pk_order(pool: &Pool) {
    let mut first = seed_item(pool, "row-a", false).await;
    for t in ["row-b", "row-c", "row-d"] {
        seed(pool, t, false).await;
    }
    first.save_pool(pool).await.expect("touch row one");
    let page1 = get(pool, "/adminls_item").await;
    assert!(
        page1.contains("row-a") && page1.contains("row-b"),
        "{page1}"
    );
    let page2 = get(pool, "/adminls_item?page=2").await;
    assert!(
        page2.contains("row-c") && page2.contains("row-d"),
        "{page2}"
    );
}

/// Pager, facet, date and custom-filter links keep every other filter
/// and carry the mounted prefix.
async fn links_keep_the_whole_filter_state(pool: &Pool) {
    for t in ["it-a", "it-b", "it-c"] {
        seed(pool, t, true).await;
    }
    let body = get(
        pool,
        "/adminls_item?rank=0&kind=low&year=2024&count=skip&q=it",
    )
    .await;
    // The "filtered by" clear link is the one link meant to drop everything.
    let clear = format!(r#"<a href="{PREFIX}/adminls_item">clear</a>"#);
    assert!(body.contains(&clear), "{body}");
    let links = list_links(&body.replace(&clear, ""));
    assert!(!links.is_empty(), "{body}");
    let next = links
        .iter()
        .find(|l| l.contains("page=2"))
        .unwrap_or_else(|| panic!("no next link: {links:?}"));
    for want in ["rank=0", "kind=low", "year=2024", "count=skip"] {
        assert!(next.contains(want), "pager drops {want}: {next}");
    }
    for l in &links {
        assert!(l.starts_with(&format!("{PREFIX}/")), "unprefixed: {l}");
        assert!(l.contains("count=skip"), "drops count: {l}");
        assert_eq!(l.matches("q=it").count(), 1, "q dropped or doubled: {l}");
        if !l.contains("year=") {
            // Only the date strip's "All" link may drop the date.
            assert!(l.contains("kind="), "drops date and kind: {l}");
        }
    }
    let facet_off = links
        .iter()
        .find(|l| !l.contains("rank=") && l.contains("kind=low"))
        .unwrap_or_else(|| panic!("no facet-clear link: {links:?}"));
    assert!(
        facet_off.contains("year=2024"),
        "facet drops date: {facet_off}"
    );
    let date_all = links
        .iter()
        .find(|l| !l.contains("year="))
        .unwrap_or_else(|| panic!("no date All link: {links:?}"));
    assert!(
        date_all.contains("kind=low") && date_all.contains("rank=0"),
        "{date_all}"
    );
    let kind_all = links
        .iter()
        .find(|l| !l.contains("kind=") && l.contains("rank=0"))
        .unwrap_or_else(|| panic!("no custom-filter All link: {links:?}"));
    assert!(kind_all.contains("year=2024"), "{kind_all}");
    // The search form keeps the custom filter and the date as hidden
    // inputs; `q` is its own visible input, never a hidden copy.
    assert!(body.contains(r#"name="kind" value="low""#), "{body}");
    assert!(body.contains(r#"name="year" value="2024""#), "{body}");
    assert!(body.contains(r#"name="q" value="it""#), "{body}");
    assert_eq!(body.matches(r#"name="q""#).count(), 1, "{body}");
}

/// The "+N more" facet link keeps the filter state, and the show-all
/// page lists every value and keeps `facet_show_all` on its links.
async fn facet_show_all_keeps_the_filter_state(pool: &Pool) {
    for rank in 0..17 {
        let mut item = seed_item(pool, &format!("it-{rank}"), true).await;
        item.rank = rank;
        item.save_pool(pool).await.expect("set rank");
    }
    let body = get(pool, "/adminls_item?flag=true&year=2024&count=skip&q=it").await;
    assert!(!body.contains("rank=16"), "not truncated: {body}");
    let links = list_links(&body);
    let more = links
        .iter()
        .find(|l| l.contains("facet_show_all="))
        .unwrap_or_else(|| panic!("no show-all link: {links:?}"));
    for want in [
        "facet_show_all=rank",
        "flag=true",
        "year=2024",
        "count=skip",
        "q=it",
    ] {
        assert!(more.contains(want), "show-all drops {want}: {more}");
    }
    let all = get(pool, more.strip_prefix(PREFIX).unwrap()).await;
    assert!(
        !all.contains(r#"class="facet-more""#),
        "still truncated: {all}"
    );
    assert!(all.contains("rank=16"), "{all}");
    let next = list_links(&all)
        .into_iter()
        .find(|l| l.contains("page=2"))
        .unwrap_or_else(|| panic!("no next link: {all}"));
    assert_eq!(next.matches("facet_show_all=rank").count(), 1, "{next}");
}

/// The bulk-action form posts under the prefix.
async fn action_form_posts_under_the_prefix(pool: &Pool) {
    seed(pool, "x", true).await;
    let body = get(pool, "/adminls_item?rank=0").await;
    assert!(
        body.contains(&format!(r#"action="{PREFIX}/adminls_item/__action""#)),
        "{body}"
    );
}

/// A stored `true` renders a checked box, whatever the backend stores.
async fn edit_form_checks_a_true_bool(pool: &Pool) {
    let on = seed(pool, "on", true).await;
    let off = seed(pool, "off", false).await;
    let body = get(pool, &format!("/adminls_item/{on}/edit")).await;
    assert!(
        body.contains(r#"name="flag" id="flag" value="true" checked"#),
        "{body}"
    );
    let body = get(pool, &format!("/adminls_item/{off}/edit")).await;
    assert!(
        body.contains(r#"name="flag" id="flag" value="true">"#),
        "{body}"
    );
}

/// A bool facet reads `true`/`false` and marks the active value, where
/// SQLite and MySQL hand back `1`/`0`.
async fn bool_facet_reads_true_and_toggles_off(pool: &Pool) {
    seed(pool, "on", true).await;
    seed(pool, "off", false).await;
    let body = get(pool, "/adminls_item?flag=true").await;
    assert!(body.contains(r#"class="active">true</a>"#), "{body}");
    assert!(body.contains(">false</a>"), "{body}");
    let links = list_links(&body);
    assert!(!links.iter().any(|l| l.contains("flag=1")), "{links:?}");
}

/// A soft-deleted row leaves the list, its count and its detail page;
/// `?trashed=1` lists it and `restore_selected` brings it back.
async fn soft_deleted_rows_leave_the_list(pool: &Pool) {
    seed(pool, "alive-row", true).await;
    let gone = seed(pool, "gone-row", true).await;
    let redirect = post(pool, &format!("/adminls_item/{gone}/delete"), "").await;
    assert!(redirect.is_redirection(), "{redirect}");

    let body = get(pool, "/adminls_item").await;
    assert!(
        body.contains("alive-row") && !body.contains("gone-row"),
        "{body}"
    );
    assert!(
        body.contains("1 row"),
        "count includes the deleted row: {body}"
    );
    let detail = format!("/adminls_item/{gone}");
    let (status, _) = send(pool, Request::builder().uri(&detail), Body::empty()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "deleted row has a detail page"
    );

    let trash = get(pool, "/adminls_item?trashed=1").await;
    assert!(
        trash.contains("gone-row") && !trash.contains("alive-row"),
        "{trash}"
    );
    let restored = post(
        pool,
        "/adminls_item/__action",
        &format!("action=restore_selected&_selected={gone}"),
    )
    .await;
    assert!(restored.is_redirection(), "{restored}");
    let body = get(pool, "/adminls_item").await;
    assert!(
        body.contains("gone-row"),
        "restore did not bring it back: {body}"
    );
}

/// Facet and date counts are within the other filters; a facet ignores its own.
async fn facet_and_date_counts_follow_the_filters(pool: &Pool) {
    for (title, flag, rank, y) in [
        ("a", true, 0, 2024),
        ("b", true, 1, 2023),
        ("c", false, 1, 2024),
    ] {
        let mut item = seed_item(pool, title, flag).await;
        item.rank = rank;
        item.made_on = chrono::NaiveDate::from_ymd_opt(y, 3, 9).unwrap();
        item.save_pool(pool).await.expect("set row");
    }
    let body = get(pool, "/adminls_item?flag=true").await;
    let count_after = |link: &str| {
        let at = body
            .find(link)
            .unwrap_or_else(|| panic!("no {link}: {body}"));
        let rest = &body[at..];
        rest[rest.find('(').unwrap() + 1..rest.find(')').unwrap()].to_owned()
    };
    assert_eq!(
        count_after("flag=true&rank=1\""),
        "1",
        "rank facet ignores flag"
    );
    assert_eq!(count_after("flag=true&rank=0\""), "1");
    assert_eq!(
        count_after("?flag=false\""),
        "1",
        "flag facet drops its own filter"
    );
    assert_eq!(count_after(">2024 <small>"), "1", "year count ignores flag");
    assert_eq!(count_after(">2023 <small>"), "1");
    let body = get(pool, "/adminls_item?flag=false&year=2024").await;
    assert!(
        body.contains(">March <small>(1)</small>"),
        "month count ignores flag: {body}"
    );
}

tri_dialect_test! {
    model: Item,
    scenarios: [
        equal_sort_keys_page_in_pk_order,
        links_keep_the_whole_filter_state,
        facet_show_all_keeps_the_filter_state,
        action_form_posts_under_the_prefix,
        edit_form_checks_a_true_bool,
        bool_facet_reads_true_and_toggles_off,
        soft_deleted_rows_leave_the_list,
        facet_and_date_counts_follow_the_filters,
    ],
}
