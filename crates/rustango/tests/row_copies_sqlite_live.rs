//! List pages and prefetch results stay byte-identical to their goldens,
//! so a copy-saving change cannot alter what renders.
//! `RUSTANGO_UPDATE_GOLDEN=1` rewrites `tests/golden/row_copies/`.

#![cfg(all(
    feature = "sqlite",
    feature = "admin",
    feature = "template_views",
    feature = "serializer"
))]

#[path = "support/row_copies.rs"]
mod fixture;

use std::fmt::Write as _;
use std::path::PathBuf;

use fixture::{RcChild, RcParent, RcTag};
use rustango::sql::{fetch_with_prefetch_pool, fetch_with_prefetch_related};

fn assert_golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/row_copies")
        .join(name);
    if std::env::var_os("RUSTANGO_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if expected != actual {
        let at = expected
            .bytes()
            .zip(actual.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(expected.len().min(actual.len()));
        let lo = at.saturating_sub(80);
        panic!(
            "{name} differs at byte {at}:\nexpected …{}…\nactual   …{}…",
            &expected[lo..(at + 80).min(expected.len())],
            &actual[lo..(at + 80).min(actual.len())],
        );
    }
}

/// `parent: child ids` lines, in result order.
fn stitched<P, C>(rows: &[(P, Vec<C>)], pk: fn(&P) -> i64, child: fn(&C) -> i64) -> String {
    let mut out = String::new();
    for (p, kids) in rows {
        let ids: Vec<String> = kids.iter().map(|c| child(c).to_string()).collect();
        writeln!(out, "{}: {}", pk(p), ids.join(",")).unwrap();
    }
    out
}

#[tokio::test]
async fn admin_list_page_is_unchanged() {
    let app = fixture::admin_app(fixture::seeded_pool().await);
    let page = fixture::get(&app, "/rc_post").await;
    // The sidebar lists every model the build registers: compare the list only.
    let start = page.find("<main class=\"content\">").expect("main");
    let end = page.find("</main>").expect("main end") + "</main>".len();
    assert_golden("admin_list.html", &format!("{}\n", &page[start..end]));
}

#[tokio::test]
async fn list_view_page_is_unchanged() {
    let app = fixture::list_view_app(fixture::seeded_pool().await);
    assert_golden("list_view.html", &fixture::get(&app, "/posts").await);
}

#[tokio::test]
async fn viewset_lists_are_unchanged() {
    let pool = fixture::seeded_pool().await;
    let plain = fixture::get(&fixture::viewset_app(pool.clone()), "/api/posts").await;
    assert_golden("viewset_list.json", &plain);
    let ser = fixture::get(&fixture::viewset_serializer_app(pool), "/api/posts?page=2").await;
    assert_golden("viewset_serializer_list.json", &ser);
}

#[tokio::test]
async fn prefetch_results_are_unchanged() {
    let pool = fixture::seeded_pool().await;
    let fk: Vec<(RcParent, Vec<RcChild>)> = fetch_with_prefetch_pool(
        RcParent::objects().order_by(&[("id", false)]),
        "parent",
        &pool,
    )
    .await
    .unwrap();
    let m2m: Vec<(RcParent, Vec<RcTag>)> = fetch_with_prefetch_related(
        RcParent::objects().order_by(&[("id", false)]),
        "tags",
        &pool,
    )
    .await
    .unwrap();
    assert_eq!(fk.len(), fixture::PARENTS as usize);
    assert!(fk
        .iter()
        .all(|(_, k)| k.len() == fixture::CHILDREN_PER_PARENT as usize));
    let mut text = stitched(&fk, |p| p.id, |c| c.id);
    text.push_str(&stitched(&m2m, |p| p.id, |t| t.id));
    assert_golden("prefetch.txt", &text);
}
