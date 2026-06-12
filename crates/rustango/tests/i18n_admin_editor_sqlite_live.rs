#![cfg(all(feature = "sqlite", feature = "admin"))]
//! Live SQLite test for the i18n admin editor — issue #532 Slice 2.
//!
//! Exercises the save → store → re-render round-trip end-to-end against
//! the merged Slice 1 override layer: `apply_edits` upserts what the
//! editor POSTs, `editor_rows` reads it back, `pivot` builds the grid,
//! and `render_editor` produces inputs that round-trip the values.
//!
//! `max_connections(1)` pins DDL + writes + reads to one in-memory DB.

use rustango::i18n::admin::{apply_edits, editor_rows, pivot, render_editor};
use rustango::i18n::db::ensure_table_pool;
use rustango::sql::{sqlx, Pool};

async fn pool() -> Pool {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite");
    let pool: Pool = p.into();
    ensure_table_pool(&pool).await.expect("ensure_table");
    pool
}

#[tokio::test]
async fn editor_save_store_render_round_trip() {
    let p = pool().await;

    // The editor POSTs these (locale, key, value) edits.
    let edits = vec![
        ("en".to_owned(), "greeting".to_owned(), "Hello".to_owned()),
        ("fr".to_owned(), "greeting".to_owned(), "Bonjour".to_owned()),
        ("en".to_owned(), "bye".to_owned(), "Bye".to_owned()),
    ];
    let n = apply_edits(&p, &edits, "alice").await.unwrap();
    assert_eq!(n, 3);

    // Read back → pivot → grid reflects the saved values + the fr.bye gap.
    let (locales, rows) = pivot(&editor_rows(&p).await.unwrap());
    assert_eq!(locales, vec!["en", "fr"]);
    let bye = rows.iter().find(|r| r.key == "bye").unwrap();
    assert_eq!(
        bye.values,
        vec![Some("Bye".to_owned()), None],
        "fr.bye is a gap"
    );

    // The rendered editor has an input pre-filled with the saved fr value.
    let html = render_editor(&locales, &rows, "editor", |s| s.to_owned());
    assert!(
        html.contains(r#"name="tr:fr:greeting" value="Bonjour""#),
        "{html}"
    );

    // A second save UPDATES in place (upsert), and fills the fr.bye gap.
    let edits2 = vec![
        ("fr".to_owned(), "greeting".to_owned(), "Salut".to_owned()),
        ("fr".to_owned(), "bye".to_owned(), "Au revoir".to_owned()),
    ];
    apply_edits(&p, &edits2, "bob").await.unwrap();

    let (locales, rows) = pivot(&editor_rows(&p).await.unwrap());
    let greeting = rows.iter().find(|r| r.key == "greeting").unwrap();
    // en unchanged, fr updated — no duplicate rows.
    assert_eq!(
        greeting.values,
        vec![Some("Hello".to_owned()), Some("Salut".to_owned())]
    );
    let bye = rows.iter().find(|r| r.key == "bye").unwrap();
    assert_eq!(
        bye.values,
        vec![Some("Bye".to_owned()), Some("Au revoir".to_owned())]
    );
    // Coverage now complete for both locales (4 rows, 2 keys × 2 locales).
    assert_eq!(editor_rows(&p).await.unwrap().len(), 4);
}
