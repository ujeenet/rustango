//! Admin list paging order, filter-keeping links, the mounted prefix and
//! bool checkboxes on every backend (#1917 #1916 #1765 #1730).

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
        actions = "delete_selected"
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
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix(PREFIX)
        .build();
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    // Tera escapes `/` in `{{ admin_prefix }}`; undo it to read hrefs.
    let body = String::from_utf8_lossy(&bytes)
        .replace("&#x2F;", "/")
        .replace("&amp;", "&");
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
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
    };
    item.insert_pool(pool).await.expect("insert");
    item
}

/// Every `href="…"` value on the page that points into the list.
fn list_links(body: &str) -> Vec<String> {
    body.split("href=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .map(str::to_owned)
        .filter(|s| s.contains("/adminls_item?"))
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
    for t in ["a", "b", "c"] {
        seed(pool, t, true).await;
    }
    let body = get(
        pool,
        "/adminls_item?rank=0&kind=low&year=2024&count=skip&q=",
    )
    .await;
    let links = list_links(&body);
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
    assert!(
        body.contains(&format!(r#"<a href="{PREFIX}/adminls_item">clear</a>"#)),
        "{body}"
    );
    // The search form keeps the custom filter and the date as hidden inputs.
    assert!(body.contains(r#"name="kind" value="low""#), "{body}");
    assert!(body.contains(r#"name="year" value="2024""#), "{body}");
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

tri_dialect_test! {
    model: Item,
    scenarios: [
        equal_sort_keys_page_in_pk_order,
        links_keep_the_whole_filter_state,
        action_form_posts_under_the_prefix,
        edit_form_checks_a_true_bool,
        bool_facet_reads_true_and_toggles_off,
    ],
}
