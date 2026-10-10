//! Admin list paging order, filter-keeping links, the mounted prefix,
//! bool checkboxes, soft-deleted rows, filtered facet counts, encoded PK
//! redirects, URL filter allow-list, NULL and empty facets and capped actions on
//! every backend (#1917 #1916 #1765 #1730 #1918 #2004 #1862 #2031 #2006 #2049 #2081).

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

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "adminls_slug",
    admin(
        list_display = "title",
        list_filter = "rank",
        formfield_overrides = "token:password"
    )
)]
#[allow(dead_code)]
pub struct Slugged {
    #[rustango(primary_key, max_length = 32)]
    pub slug: String,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(max_length = 64)]
    pub token: String,
    pub rank: Option<i64>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "adminls_tag", display = "name", admin(list_filter = "tag"))]
#[allow(dead_code)]
pub struct Tagged {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    #[rustango(max_length = 16)]
    pub tag: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "adminls_owner", display = "name")]
#[allow(dead_code)]
pub struct Owner {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

/// An FK facet with more values than the facet shows (#2344).
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "adminls_pet",
    admin(
        list_display = "name",
        list_filter = "owner_id, name",
        list_per_page = 1,
        ordering = "id"
    )
)]
#[allow(dead_code)]
pub struct Pet {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
    #[rustango(fk = "adminls_owner", on = "id")]
    pub owner_id: i64,
}

/// Owner ids whose row the admin read for a display name.
static OWNERS_READ: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());
/// Scenarios on [`OWNERS_READ`] run one at a time.
static OWNERS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn record_owner_read(_: &axum::http::request::Parts, row: Option<&serde_json::Value>) -> bool {
    if let Some(id) = row
        .and_then(|r| r.get("id"))
        .and_then(serde_json::Value::as_i64)
    {
        OWNERS_READ.lock().unwrap().push(id);
    }
    true
}
rustango::register_admin_object_permission!("adminls_owner", "view", record_owner_read);

async fn seed_slug(pool: &Pool, slug: &str, token: &str, rank: Option<i64>) {
    let s = Slugged {
        slug: slug.into(),
        title: format!("{slug}-title"),
        token: token.into(),
        rank,
    };
    s.insert_pool(pool).await.expect("insert slug");
}

/// No searchable column: only a number.
#[derive(Model, Debug, Clone)]
#[rustango(table = "adminls_counter")]
#[allow(dead_code)]
pub struct Counter {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub qty: i32,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Counter>(pool).await;
    rustango::testkit::matrix::fresh_table::<Slugged>(pool).await;
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    rustango::testkit::matrix::fresh_table::<Tagged>(pool).await;
    rustango::testkit::matrix::drop_table(pool, "adminls_pet").await;
    rustango::testkit::matrix::fresh_table::<Owner>(pool).await;
    rustango::testkit::matrix::fresh_table::<Pet>(pool).await;
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
    // The trash list offers only restore, and its action keeps `?trashed=1`.
    assert!(body.contains(r#"value="delete_selected""#), "{body}");
    assert!(
        trash.contains(r#"value="restore_selected""#)
            && !trash.contains(r#"value="delete_selected""#)
            && trash.contains(r#"name="trashed" value="1""#),
        "{trash}"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/adminls_item/__action")
        .header("content-type", "application/x-www-form-urlencoded");
    let form = format!("trashed=1&action=restore_selected&_selected={gone}");
    let res = rustango::admin::Builder::new(pool.clone())
        .admin_prefix(PREFIX)
        .build()
        .oneshot(req.body(Body::from(form)).unwrap())
        .await
        .unwrap();
    assert!(res.status().is_redirection(), "{}", res.status());
    assert_eq!(
        res.headers()["location"],
        format!("{PREFIX}/adminls_item?trashed=1")
    );
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

/// A CR/LF in a string PK is percent-encoded into the redirect, not a panic (#1862).
async fn string_pk_redirect_is_encoded(pool: &Pool) {
    let req = Request::builder()
        .method("POST")
        .uri("/adminls_slug")
        .header("content-type", "application/x-www-form-urlencoded");
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix(PREFIX)
        .build();
    let body = Body::from("slug=a%0D%0Ab%2Fc&title=t&token=k&_continue=1");
    let res = app.oneshot(req.body(body).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let to = res.headers()["location"].to_str().unwrap().to_owned();
    assert_eq!(to, format!("{PREFIX}/adminls_slug/a%0D%0Ab%2Fc"));
    // The encoded segment routes back to the row (404 otherwise).
    get(pool, to.strip_prefix(PREFIX).unwrap()).await;
}

/// A secret or unshown field is no URL filter; a displayed one is (#2031).
async fn url_filters_skip_secret_and_unshown_fields(pool: &Pool) {
    seed_slug(pool, "x", "alpha", Some(1)).await;
    seed_slug(pool, "y", "beta", Some(2)).await;
    let body = get(pool, "/adminls_slug?token=alpha").await;
    assert!(
        body.contains("x-title") && body.contains("y-title"),
        "{body}"
    );
    let body = get(pool, "/adminls_slug?title=x-title").await;
    assert!(
        body.contains("x-title") && !body.contains("y-title"),
        "{body}"
    );
}

/// The NULL facet links `?rank__isnull=1`, which lists only NULL rows (#2006).
async fn null_facet_lists_the_null_rows(pool: &Pool) {
    seed_slug(pool, "x", "k", None).await;
    seed_slug(pool, "y", "k", Some(3)).await;
    let body = get(pool, "/adminls_slug").await;
    assert!(body.contains("/adminls_slug?rank__isnull=1"), "{body}");
    let body = get(pool, "/adminls_slug?rank__isnull=1").await;
    assert!(
        body.contains("x-title") && !body.contains("y-title"),
        "{body}"
    );
    // The active NULL value toggles back off.
    assert!(body.contains(r#"href="/adm/adminls_slug""#), "{body}");
}

/// The empty-string facet links `?tag__isempty=1`, which lists only those rows (#2081).
async fn empty_facet_lists_the_empty_rows(pool: &Pool) {
    for (name, tag) in [("plain-row", ""), ("red-row", "red")] {
        let mut t = Tagged {
            id: Auto::default(),
            name: name.into(),
            tag: tag.into(),
        };
        t.insert_pool(pool).await.expect("insert tag");
    }
    let body = get(pool, "/adminls_tag").await;
    assert!(body.contains("/adminls_tag?tag__isempty=1"), "{body}");
    let body = get(pool, "/adminls_tag?tag__isempty=1").await;
    assert!(
        body.contains("plain-row") && !body.contains("red-row"),
        "{body}"
    );
    // The active empty value toggles back off.
    assert!(body.contains(r#"href="/adm/adminls_tag""#), "{body}");
}

/// A facet reads only the values it shows, not every one; the "+N more"
/// count stays exact and an active value past the cut still shows (#2344).
async fn facet_reads_only_the_values_it_shows(pool: &Pool) {
    let _g = OWNERS_LOCK.lock().await;
    let mut owners = Vec::new();
    for i in 0..20 {
        let mut o = Owner {
            id: Auto::default(),
            name: format!("owner-{i:02}"),
        };
        o.insert_pool(pool).await.expect("insert owner");
        let id = *o.id.get().expect("pk");
        let mut p = Pet {
            id: Auto::default(),
            name: format!("pet-{i:02}"),
            owner_id: id,
        };
        p.insert_pool(pool).await.expect("insert pet");
        owners.push(id);
    }
    // Equal counts tie-break by key, so the last owner is past the cut.
    let last = *owners.last().unwrap();

    OWNERS_READ.lock().unwrap().clear();
    let body = get(pool, "/adminls_pet").await;
    let read = std::mem::take(&mut *OWNERS_READ.lock().unwrap());
    assert!(!read.contains(&last), "read past the cut: {read:?}");
    assert!(body.contains("+5 more"), "{body}");

    let body = get(pool, &format!("/adminls_pet?owner_id={last}")).await;
    assert!(body.contains("owner-19"), "active FK value hidden: {body}");
    let body = get(pool, "/adminls_pet?name=pet-19").await;
    assert!(
        body.contains(r#"class="active">pet-19<"#),
        "active value hidden: {body}"
    );
    assert!(body.contains("+5 more"), "{body}");
}

/// An FK facet value past the cut is reachable through its show-all link (#2350).
async fn fk_facet_past_the_cut_is_reachable(pool: &Pool) {
    for i in 0..20 {
        let mut o = Owner {
            id: Auto::default(),
            name: format!("owner-{i:02}"),
        };
        o.insert_pool(pool).await.expect("insert owner");
        let mut p = Pet {
            id: Auto::default(),
            name: format!("pet-{i:02}"),
            owner_id: *o.id.get().expect("pk"),
        };
        p.insert_pool(pool).await.expect("insert pet");
    }
    let body = get(pool, "/adminls_pet").await;
    assert!(!body.contains("owner-19"), "not truncated: {body}");
    let more = body
        .split("href=\"")
        .filter_map(|s| s.split('"').next())
        .find(|l| l.contains("facet_show_all=owner_id"))
        .unwrap_or_else(|| panic!("no FK show-all link: {body}"));
    let all = get(pool, more.strip_prefix(PREFIX).unwrap()).await;
    assert!(all.contains("owner-19"), "{all}");
}

/// The capped facet counts NULL as one more value (#2344).
async fn capped_facet_counts_null_as_a_value(pool: &Pool) {
    for i in 0..20 {
        seed_slug(pool, &format!("r{i:02}"), "k", Some(i)).await;
    }
    seed_slug(pool, "n1", "k", None).await;
    seed_slug(pool, "n2", "k", None).await;
    // 21 values, 15 shown.
    let body = get(pool, "/adminls_slug").await;
    assert!(body.contains("+6 more"), "{body}");
}

/// `?owner_id=01` names a shown value; the facet lists it once (#2344).
async fn noncanonical_active_value_is_listed_once(pool: &Pool) {
    let _g = OWNERS_LOCK.lock().await;
    let mut first = None;
    for i in 0..20 {
        let mut o = Owner {
            id: Auto::default(),
            name: format!("owner-{i:02}"),
        };
        o.insert_pool(pool).await.expect("insert owner");
        let id = *o.id.get().expect("pk");
        first.get_or_insert(id);
        let mut p = Pet {
            id: Auto::default(),
            name: format!("pet-{i:02}"),
            owner_id: id,
        };
        p.insert_pool(pool).await.expect("insert pet");
    }
    let first = first.unwrap();
    let canonical = get(pool, &format!("/adminls_pet?owner_id={first}")).await;
    let padded = get(pool, &format!("/adminls_pet?owner_id=0{first}")).await;
    assert_eq!(
        padded.matches("owner-00").count(),
        canonical.matches("owner-00").count(),
        "{padded}"
    );
}

/// A bulk action past the bind-safe key cap is a 400 and writes nothing (#2049).
async fn bulk_action_selection_is_capped(pool: &Pool) {
    let id = seed(pool, "kept-row", false).await;
    let mut form = String::from("action=delete_selected");
    for _ in 0..10_001 {
        form.push_str(&format!("&_selected={id}"));
    }
    let req = Request::builder()
        .method("POST")
        .uri("/adminls_item/__action")
        .header("content-type", "application/x-www-form-urlencoded");
    let (status, body) = send(pool, req, Body::from(form)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(get(pool, "/adminls_item").await.contains("kept-row"));
}

/// A query with no searchable column matches nothing, in autocomplete and
/// the changelist (#2391).
async fn autocomplete_without_search_columns_is_empty(pool: &Pool) {
    let mut c = Counter {
        id: Auto::default(),
        qty: 7,
    };
    c.insert_pool(pool).await.expect("insert");
    let results = |body: String| {
        let json: serde_json::Value = serde_json::from_str(&body).expect("json");
        json["results"].as_array().expect("results").len()
    };
    let all = get(pool, "/adminls_counter/__autocomplete").await;
    assert_eq!(results(all), 1, "no query lists the rows");
    let none = get(pool, "/adminls_counter/__autocomplete?q=zzz").await;
    assert_eq!(results(none), 0);

    // The changelist agrees: the writer, not each view, decides.
    let link = format!("adminls_counter/{}\"", c.id.get().expect("pk"));
    assert!(get(pool, "/adminls_counter").await.contains(&link));
    let searched = get(pool, "/adminls_counter?q=zzz").await;
    assert!(
        !searched.contains(&link),
        "a search with no column listed the row"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        autocomplete_without_search_columns_is_empty,
        equal_sort_keys_page_in_pk_order,
        links_keep_the_whole_filter_state,
        facet_show_all_keeps_the_filter_state,
        action_form_posts_under_the_prefix,
        edit_form_checks_a_true_bool,
        bool_facet_reads_true_and_toggles_off,
        soft_deleted_rows_leave_the_list,
        facet_and_date_counts_follow_the_filters,
        string_pk_redirect_is_encoded,
        url_filters_skip_secret_and_unshown_fields,
        null_facet_lists_the_null_rows,
        empty_facet_lists_the_empty_rows,
        facet_reads_only_the_values_it_shows,
        capped_facet_counts_null_as_a_value,
        fk_facet_past_the_cut_is_reachable,
        noncanonical_active_value_is_listed_once,
        bulk_action_selection_is_capped,
    ],
}
