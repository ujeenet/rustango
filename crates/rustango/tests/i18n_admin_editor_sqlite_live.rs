#![cfg(all(feature = "sqlite", feature = "admin"))]
//! Live SQLite tests for the i18n admin editor — issue #532 Slices 2 + 3.
//!
//! Slice 2: the save → store → re-render round-trip end-to-end against
//! the Slice 1 override layer — `apply_edits` upserts what the editor
//! POSTs, `editor_rows` reads it back, `pivot` builds the grid, and
//! `render_editor` produces inputs that round-trip the values.
//!
//! Slice 3: `apply_deletes` removes a key across all locales, and
//! `export_json` renders the live catalog locale-keyed.
//!
//! `max_connections(1)` pins DDL + writes + reads to one in-memory DB.

use rustango::i18n::admin::{
    apply_deletes, apply_edits, editor_rows, export_json, pivot, render_editor,
};
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

#[tokio::test]
async fn delete_key_and_export_round_trip() {
    let p = pool().await;
    let edits = vec![
        ("en".to_owned(), "greeting".to_owned(), "Hello".to_owned()),
        ("fr".to_owned(), "greeting".to_owned(), "Bonjour".to_owned()),
        ("en".to_owned(), "bye".to_owned(), "Bye".to_owned()),
    ];
    apply_edits(&p, &edits, "alice").await.unwrap();

    // Export renders the live catalog locale-keyed (deterministic JSON).
    let json = export_json(&editor_rows(&p).await.unwrap());
    assert!(json.contains("\"greeting\""), "{json}");
    assert!(json.contains("Bonjour"), "{json}");
    assert!(json.contains("Bye"), "{json}");

    // Delete the "greeting" key → both locales' overrides removed.
    let removed = apply_deletes(&p, &["greeting".to_owned()]).await.unwrap();
    assert_eq!(removed, 2, "greeting had en + fr overrides");
    let rows = editor_rows(&p).await.unwrap();
    assert!(
        rows.iter().all(|(_, k, _)| k != "greeting"),
        "greeting gone: {rows:?}"
    );
    assert_eq!(rows.len(), 1, "only en.bye remains");

    // Deleting an unknown key is a no-op (0 rows).
    assert_eq!(apply_deletes(&p, &["nope".to_owned()]).await.unwrap(), 0);
}
