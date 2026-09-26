#![cfg(feature = "sqlite")]
//! A schema-driven INSERT stamps `auto_now_add` / `auto_now` itself.
//!
//! The derive macro's own INSERT path was fixed for #1464, but the
//! ViewSet, the admin's create view and `ModelForm` do not use it.
//! They build an `InsertQuery` from [`ModelSchema`] and the client
//! payload — and a server-assigned timestamp is in no payload, so the
//! column was omitted and the database default fired.
//!
//! That is not a small difference on SQLite. A database created before
//! #1464 still defaults to `CURRENT_TIMESTAMP`, `ALTER TABLE` there
//! cannot replace a column default, and `YYYY-MM-DD HH:MM:SS` sorts
//! below the canonical `…T…` spelling. A cursor keyed on such a column
//! matches every row, so page two is page one — which is exactly what
//! the commerce soak reported, on a fleet built from the *fixed* tree,
//! after 7572 unit tests had passed.
//!
//! The fixture therefore builds its table the **pre-#1464** way. A
//! table built by today's DDL writes canonical rows whichever path
//! inserts them, so it would pass without the fix.

use chrono::{DateTime, Utc};
use rustango::core::Model as _;
use rustango::core::SqlValue;
use rustango::forms::{collect_insert_values, collect_values};
use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "schema_insert_stamp", display = "label")]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub label: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<DateTime<Utc>>,
}

fn form(label: &str) -> std::collections::HashMap<String, String> {
    let mut f = std::collections::HashMap::new();
    f.insert("label".to_owned(), label.to_owned());
    f
}

/// The unit of the fix: the insert collector supplies both timestamps,
/// the plain one supplies neither.
#[test]
fn the_insert_collector_supplies_what_the_payload_cannot() {
    let plain = collect_values(Note::SCHEMA, &form("a"), &[]).expect("collect");
    let cols: Vec<&str> = plain.iter().map(|(c, _)| *c).collect();
    assert_eq!(
        cols,
        vec!["label"],
        "the plain collector is for UPDATE and must not stamp \
         `auto_now_add` — that column is immutable after insert"
    );

    let insert = collect_insert_values(Note::SCHEMA, &form("a"), &[]).expect("collect");
    let cols: Vec<&str> = insert.iter().map(|(c, _)| *c).collect();
    assert!(
        cols.contains(&"created_at") && cols.contains(&"updated_at"),
        "a schema-driven INSERT must supply both server-assigned \
         timestamps rather than leave them to the column default. Got: \
         {cols:?}"
    );
    for (col, v) in &insert {
        if *col == "created_at" || *col == "updated_at" {
            assert!(
                matches!(v, SqlValue::DateTime(_)),
                "{col} must be bound as a DateTime, not text: {v:?}"
            );
        }
    }
}

/// And it lands in the database in the canonical shape, against a
/// table whose default is the one an upgraded database still carries.
#[tokio::test]
async fn a_schema_driven_insert_writes_canonical_text() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE schema_insert_stamp (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            label TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
        Vec::new(),
    )
    .await
    .expect("create with the legacy default");

    let collected =
        collect_insert_values(Note::SCHEMA, &form("through-the-viewset"), &[]).expect("collect");
    let (columns, values): (Vec<_>, Vec<_>) = collected.into_iter().unzip();
    let query =
        rustango::core::InsertQuery::new(Note::SCHEMA, columns, values).returning(vec!["id"]);
    rustango::sql::insert_returning_pool(&pool, &query)
        .await
        .expect("insert");

    let rows: Vec<(String, String)> = rustango::sql::raw_query_pool(
        "SELECT created_at, updated_at FROM schema_insert_stamp",
        Vec::new(),
        &pool,
    )
    .await
    .expect("read back");

    assert_eq!(rows.len(), 1);
    for (what, stored) in [("created_at", &rows[0].0), ("updated_at", &rows[0].1)] {
        let b = stored.as_bytes();
        assert!(
            b.len() == 32 && b[10] == b'T' && stored.ends_with("+00:00"),
            "{what} must be canonical. This table's default is the \
             pre-#1464 `CURRENT_TIMESTAMP`, so any other shape means the \
             writer omitted the column and the default fired. Stored: \
             {stored:?}"
        );
    }
}

/// The reason the shape matters, as the soak sees it: `ORDER BY
/// created_at DESC` must put the newest row first.
///
/// Two rows — one fresh through the insert collector, and one
/// canonical seed at midnight of that same day, which is what the
/// migrate sweep leaves behind. The fresh one must sort first.
///
/// It is the ordering that discriminates, not a cursor comparison. A
/// cursor at `now` returns both rows whatever the shapes, and a cursor
/// at the fresh row's own value returns the old one either way — both
/// would pass on broken code. Under the defect the fresh row is
/// `' '`-separated, sorts *below* every canonical value whatever
/// instant it holds, and `DESC` hands back the older row first. That
/// inversion is what makes cursor pagination serve page one forever.
///
/// **The seed shares the fresh row's date, deliberately.** A seed a day
/// back is decided at index 8 and the comparison never reaches the
/// `' '` / `'T'` separator at index 10 — so it sorts correctly even
/// with the defect present, and the guard passes on broken code. The
/// first draft of this test did exactly that; the revert drill caught
/// it. Reading the date back from the stored row keeps the two aligned
/// without a fixture that breaks at midnight.
#[tokio::test]
async fn the_newest_row_sorts_first() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE schema_insert_stamp (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            label TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
        Vec::new(),
    )
    .await
    .expect("create with the legacy default");

    let collected = collect_insert_values(Note::SCHEMA, &form("new"), &[]).expect("collect");
    let (columns, values): (Vec<_>, Vec<_>) = collected.into_iter().unzip();
    rustango::sql::insert_returning_pool(
        &pool,
        &rustango::core::InsertQuery::new(Note::SCHEMA, columns, values).returning(vec!["id"]),
    )
    .await
    .expect("insert");

    // Seed an older row on the *same calendar day* as the fresh one, so
    // the comparison is decided at the separator rather than inside the
    // date. Canonical spelling — what the migrate sweep leaves behind.
    let stored: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT created_at FROM schema_insert_stamp",
        Vec::new(),
        &pool,
    )
    .await
    .expect("read the fresh row");
    let same_day_midnight = format!("{}T00:00:00.000000+00:00", &stored[0].0[..10]);
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO schema_insert_stamp (label, created_at, updated_at) VALUES ('old', ?, ?)",
        vec![
            SqlValue::String(same_day_midnight.clone()),
            SqlValue::String(same_day_midnight),
        ],
    )
    .await
    .expect("seed the swept row");

    let page: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT label FROM schema_insert_stamp ORDER BY created_at DESC",
        Vec::new(),
        &pool,
    )
    .await
    .expect("page");

    let labels: Vec<String> = page.into_iter().map(|r| r.0).collect();
    assert_eq!(
        labels,
        vec!["new".to_owned(), "old".to_owned()],
        "the row written a moment ago must sort above midnight of the \
         same day. Getting `old` first means the fresh row is stored in \
         the legacy shape and sorts below every canonical value \
         whatever instant it holds (#1464)."
    );
}
