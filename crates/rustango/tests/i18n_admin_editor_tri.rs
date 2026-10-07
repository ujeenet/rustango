//! The translations editor on all three backends: emptying one locale's
//! cell removes that override only (#2091).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin",
    feature = "testkit"
))]

use rustango::i18n::admin::{apply_edits, apply_form, editor_rows};
use rustango::i18n::db::ensure_table_pool;
use rustango::sql::Pool;
use rustango::tri_dialect_test;

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, "rustango_translations").await;
    ensure_table_pool(pool).await.expect("ensure_table");
}

fn edit(locale: &str, key: &str, value: &str) -> (String, String, String) {
    (locale.to_owned(), key.to_owned(), value.to_owned())
}

/// A grid cell as posted: its value and the value the page showed.
fn cell(locale: &str, key: &str, value: &str, shown: &str) -> [(String, String); 2] {
    [
        (format!("tr:{locale}:{key}"), value.to_owned()),
        (format!("orig:{locale}:{key}"), shown.to_owned()),
    ]
}

/// #2091: an emptied cell was skipped, so the override could not be removed.
async fn emptied_cell_drops_that_locales_override(pool: &Pool) {
    let seed = [edit("en", "greet", "Hello"), edit("fr", "greet", "Salut")];
    apply_edits(pool, &seed, "alice").await.expect("seed");
    // en untouched, fr emptied, and an untouched blank de gap.
    let form: Vec<_> = [
        cell("en", "greet", "Hello", "Hello"),
        cell("fr", "greet", "", "Salut"),
        cell("de", "greet", "", ""),
    ]
    .concat();
    apply_form(pool, &form, "bob").await.expect("save");
    assert_eq!(
        editor_rows(pool).await.expect("rows"),
        [edit("en", "greet", "Hello")]
    );
}

/// A gap filled after the page loaded was shown blank: a stale save keeps it.
async fn stale_blank_keeps_an_override_filled_meanwhile(pool: &Pool) {
    let seed = [edit("en", "greet", "Hello")];
    apply_edits(pool, &seed, "alice").await.expect("seed");
    // The page loads with fr blank; then another superuser fills fr.
    apply_edits(pool, &[edit("fr", "greet", "Salut")], "carol")
        .await
        .expect("fill");
    let form: Vec<_> = [
        cell("en", "greet", "Hello", "Hello"),
        cell("fr", "greet", "", ""),
    ]
    .concat();
    apply_form(pool, &form, "bob").await.expect("save");
    let mut rows = editor_rows(pool).await.expect("rows");
    rows.sort();
    assert_eq!(
        rows,
        [edit("en", "greet", "Hello"), edit("fr", "greet", "Salut")]
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        emptied_cell_drops_that_locales_override,
        stale_blank_keeps_an_override_filled_meanwhile,
    ],
}
