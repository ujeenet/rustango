#![cfg(feature = "sqlite")]
//! Live SQLite test for the i18n DB-override layer — issue #532 Slice 1.
//!
//! Covers the full storage substrate: `ensure_table_pool` is idempotent,
//! `upsert_pool` inserts then updates a `(locale, key)`, seeding from a
//! `Translator`'s file catalogs is idempotent (`ON CONFLICT DO NOTHING`),
//! and `refresh_overrides_pool` makes `Translator::translate` prefer the
//! DB value over the file value while leaving non-overridden keys on the
//! file catalog.
//!
//! `max_connections(1)` pins DDL + writes + reads to one in-memory DB.

use std::collections::HashMap;

use rustango::i18n::db::{
    all_pool, ensure_table_pool, refresh_overrides_pool, seed_from_translator_pool, upsert_pool,
};
use rustango::i18n::{Locale, Translator};
use rustango::sql::{sqlx, Pool};

async fn empty_pool() -> Pool {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite");
    p.into()
}

async fn ready_pool() -> Pool {
    let pool = empty_pool().await;
    ensure_table_pool(&pool).await.expect("ensure_table");
    pool
}

fn catalog(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[tokio::test]
async fn ensure_table_is_idempotent() {
    let pool = empty_pool().await;
    ensure_table_pool(&pool).await.expect("first create");
    ensure_table_pool(&pool)
        .await
        .expect("second create is a no-op");
    // Table is usable.
    assert_eq!(all_pool(&pool).await.unwrap().len(), 0);
}

#[tokio::test]
async fn upsert_inserts_then_updates_in_place() {
    let pool = ready_pool().await;

    upsert_pool(&pool, "fr", "greeting", "Bonjour", "alice")
        .await
        .unwrap();
    upsert_pool(&pool, "es", "greeting", "Hola", "bob")
        .await
        .unwrap();
    assert_eq!(all_pool(&pool).await.unwrap().len(), 2);

    // Same (locale, key) again → UPDATE, not a duplicate row.
    upsert_pool(&pool, "fr", "greeting", "Salut", "carol")
        .await
        .unwrap();
    let rows = all_pool(&pool).await.unwrap();
    assert_eq!(rows.len(), 2, "(locale, key) is unique — upsert updates");
    let fr = rows
        .iter()
        .find(|t| t.locale == "fr" && t.key == "greeting")
        .unwrap();
    assert_eq!(fr.value, "Salut");
    assert_eq!(fr.updated_by, "carol");
}

#[tokio::test]
async fn refresh_makes_translate_prefer_db_over_file() {
    let pool = ready_pool().await;

    // File catalog: fr has two keys.
    let t = Translator::new(Locale::new("en"));
    t.insert_locale(
        Locale::new("fr"),
        catalog(&[("greeting", "Bonjour (file)"), ("bye", "Au revoir")]),
    );
    assert_eq!(t.translate("fr", "greeting", &[]), "Bonjour (file)");

    // Operator edits `fr.greeting` in the DB, then we refresh the layer.
    upsert_pool(&pool, "fr", "greeting", "Salut (db)", "op")
        .await
        .unwrap();
    let loaded = refresh_overrides_pool(&t, &pool).await.unwrap();
    assert_eq!(loaded, 1);

    // DB override wins for the edited key…
    assert_eq!(t.translate("fr", "greeting", &[]), "Salut (db)");
    // …but a non-overridden key still resolves from the file catalog.
    assert_eq!(t.translate("fr", "bye", &[]), "Au revoir");
    // Base-language resolution also sees the override (fr-FR → fr).
    assert_eq!(t.translate("fr-FR", "greeting", &[]), "Salut (db)");
}

#[tokio::test]
async fn seed_from_translator_is_idempotent() {
    let pool = ready_pool().await;

    let t = Translator::new(Locale::new("en"));
    t.insert_locale(Locale::new("en"), catalog(&[("a", "A"), ("b", "B")]));
    t.insert_locale(Locale::new("fr"), catalog(&[("a", "Ah")]));

    let n1 = seed_from_translator_pool(&pool, &t).await.unwrap();
    assert_eq!(n1, 3, "2 en + 1 fr entries processed");
    assert_eq!(all_pool(&pool).await.unwrap().len(), 3);

    // Re-seed: every key already present → ON CONFLICT DO NOTHING.
    let n2 = seed_from_translator_pool(&pool, &t).await.unwrap();
    assert_eq!(n2, 3, "still processes all entries");
    assert_eq!(
        all_pool(&pool).await.unwrap().len(),
        3,
        "no duplicate rows — files stay the read-only seed"
    );

    // A pre-existing DB edit is NOT clobbered by a re-seed.
    upsert_pool(&pool, "fr", "a", "Salut-override", "op")
        .await
        .unwrap();
    seed_from_translator_pool(&pool, &t).await.unwrap();
    refresh_overrides_pool(&t, &pool).await.unwrap();
    assert_eq!(t.translate("fr", "a", &[]), "Salut-override");
}
