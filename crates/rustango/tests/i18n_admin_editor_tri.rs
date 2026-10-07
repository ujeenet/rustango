//! The translations editor on all three backends: emptying one locale's
//! cell removes that override only (#2091).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin",
    feature = "testkit"
))]

use rustango::i18n::admin::{apply_edits, editor_rows};
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

/// #2091: an emptied cell was skipped, so the override could not be removed.
async fn emptied_cell_drops_that_locales_override(pool: &Pool) {
    let seed = [edit("en", "greet", "Hello"), edit("fr", "greet", "Salut")];
    apply_edits(pool, &seed, "alice").await.expect("seed");
    // The grid posts en untouched, fr emptied, and an untouched blank de gap.
    let save = [
        edit("en", "greet", "Hello"),
        edit("fr", "greet", ""),
        edit("de", "greet", ""),
    ];
    assert_eq!(apply_edits(pool, &save, "bob").await.expect("save"), 1);
    assert_eq!(
        editor_rows(pool).await.expect("rows"),
        [edit("en", "greet", "Hello")]
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [emptied_cell_drops_that_locales_override],
}
