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

/// Keys differing only in case or accents are two overrides, as on PG/SQLite.
async fn keys_compare_case_sensitively(pool: &Pool) {
    use rustango::i18n::db::{all_pool, upsert_pool};
    for key in ["Key", "key", "kéy"] {
        upsert_pool(pool, "en", key, key, "alice")
            .await
            .unwrap_or_else(|e| panic!("{key}: {e}"));
    }
    let mut keys: Vec<String> = all_pool(pool)
        .await
        .expect("rows")
        .into_iter()
        .map(|t| t.key)
        .collect();
    keys.sort();
    assert_eq!(keys, ["Key", "key", "kéy"]);
}

/// MySQL: a table an older release made under a `_ci` collation is moved.
async fn an_older_case_insensitive_table_is_upgraded(pool: &Pool) {
    if pool.dialect().name() != "mysql" {
        return;
    }
    let old = "ALTER TABLE `rustango_translations` MODIFY COLUMN `key` VARCHAR(200) \
               CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NOT NULL";
    rustango::sql::raw_execute_pool(pool, old, Vec::new())
        .await
        .expect("old collation");
    ensure_table_pool(pool).await.expect("upgrade");
    keys_compare_case_sensitively(pool).await;
}

/// The `key` column's collation, MySQL only.
async fn key_collation(pool: &Pool) -> String {
    let sql = "SELECT CAST(COLLATION_NAME AS CHAR) FROM information_schema.COLUMNS \
               WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'rustango_translations' \
               AND COLUMN_NAME = 'key'";
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(sql, Vec::new(), pool)
        .await
        .expect("collation");
    rows.into_iter().next().expect("key column").0
}

/// MySQL: a user without ALTER keeps the old `_ci` table and is not refused;
/// the pool does not retry, a new one does.
async fn a_refused_upgrade_only_warns(pool: &Pool) {
    use rustango::sql::raw_execute_pool as exec;
    if pool.dialect().name() != "mysql" {
        return;
    }
    let old = "ALTER TABLE `rustango_translations` MODIFY COLUMN `key` VARCHAR(200) \
               CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NOT NULL";
    exec(pool, old, Vec::new()).await.expect("old collation");
    let db: Vec<(String,)> = rustango::sql::raw_query_pool("SELECT DATABASE()", Vec::new(), pool)
        .await
        .expect("db name");
    let db = &db[0].0;
    for sql in [
        "DROP USER IF EXISTS 'rg_noalter'@'%'".to_owned(),
        "CREATE USER 'rg_noalter'@'%' IDENTIFIED BY 'noalter'".to_owned(),
        format!("GRANT SELECT, INSERT, UPDATE, DELETE, CREATE ON `{db}`.* TO 'rg_noalter'@'%'"),
    ] {
        exec(pool, &sql, Vec::new()).await.expect(&sql);
    }
    let url = std::env::var("MYSQL_TEST_URL").expect("mysql url");
    let (scheme, rest) = url.split_once("://").expect("scheme");
    let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let url = format!("{scheme}://rg_noalter:noalter@{host}");
    let limited = Pool::connect(&url).await.expect("limited pool");

    ensure_table_pool(&limited)
        .await
        .expect("refused ALTER only warns");
    assert!(key_collation(pool).await.ends_with("_ci"));

    // A table-level grant applies at once; the refused pool still skips.
    let grant = format!("GRANT ALTER ON `{db}`.`rustango_translations` TO 'rg_noalter'@'%'");
    exec(pool, &grant, Vec::new()).await.expect("grant alter");
    ensure_table_pool(&limited).await.expect("second ensure");
    assert!(key_collation(pool).await.ends_with("_ci"), "retried");

    let fresh = Pool::connect(&url).await.expect("fresh pool");
    ensure_table_pool(&fresh).await.expect("fresh ensure");
    assert!(key_collation(pool).await.ends_with("_cs"));
    exec(pool, "DROP USER 'rg_noalter'@'%'", Vec::new())
        .await
        .expect("drop user");
}

/// A catalog key longer than the column is skipped: the rest still seed,
/// and an editor save with it writes its other rows.
async fn a_long_key_does_not_fail_the_rest(pool: &Pool) {
    use rustango::i18n::db::{all_pool, seed_from_translator_pool};
    use rustango::i18n::{Locale, Translator};
    let long = "x".repeat(366);
    let t = Translator::new(Locale::new("en"));
    let catalog = [("short", "court"), (long.as_str(), "long fr")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    t.insert_locale(Locale::new("fr"), catalog);
    seed_from_translator_pool(pool, &t).await.expect("seed");
    let keys: Vec<String> = all_pool(pool)
        .await
        .expect("rows")
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(keys, ["short"]);
    let form: Vec<_> = [
        cell("fr", "short", "bref", "court"),
        cell("fr", &long, "autre", "long fr"),
    ]
    .concat();
    apply_form(pool, &form, "bob").await.expect("save");
    assert_eq!(
        editor_rows(pool).await.expect("rows"),
        [edit("fr", "short", "bref")]
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        emptied_cell_drops_that_locales_override,
        stale_blank_keeps_an_override_filled_meanwhile,
        keys_compare_case_sensitively,
        an_older_case_insensitive_table_is_upgraded,
        a_refused_upgrade_only_warns,
        a_long_key_does_not_fail_the_rest,
    ],
}
