//! Enum fields stored as text (`TextEnum`) on every backend: round trip,
//! filters, updates, flat reads, and exact errors for unknown stored values.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{Column as _, Model as _, TextEnum, UnknownVariant};
use rustango::sql::{
    sqlx, Auto, CounterPool as _, DialectKind, ExecError, FetcherPool as _, Pool, UpdaterPool as _,
};
use rustango::{tri_dialect_test, Model};

#[derive(rustango::TextEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[text_enum(rename_all = "snake_case")]
pub enum Mode {
    #[text_enum(alias = "old_draft")]
    Draft,
    #[text_enum(rename = "pub", label = "Published")]
    Published,
    Archived,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "te_doc", app = "text_enum_tri")]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(text_enum, max_length = 16, default = "'draft'")]
    pub mode: Mode,
    #[rustango(text_enum, max_length = 16)]
    pub note: Option<Mode>,
}

/// The same values in an unbounded text column (TEXT / LONGTEXT).
#[derive(Model, Debug, Clone)]
#[rustango(table = "te_plain", app = "text_enum_tri")]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub body: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Doc>(pool).await;
    rustango::testkit::matrix::fresh_table::<Plain>(pool).await;
}

async fn doc(pool: &Pool, mode: Mode, note: Option<Mode>) -> i64 {
    let mut d = Doc {
        id: Auto::default(),
        mode,
        note,
    };
    d.insert(pool).await.expect("insert doc");
    *d.id.get().expect("pk")
}

async fn raw(pool: &Pool, sql: &str) {
    rustango::sql::raw_execute_pool(pool, sql, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The column the decode error names and its typed source.
fn decode_error(e: ExecError) -> (String, UnknownVariant) {
    match e {
        ExecError::Driver(sqlx::Error::ColumnDecode { index, source }) => {
            let uv = source
                .downcast_ref::<UnknownVariant>()
                .unwrap_or_else(|| panic!("source is not UnknownVariant: {source}"))
                .clone();
            (index, uv)
        }
        other => panic!("expected ColumnDecode, got {other:?}"),
    }
}

async fn round_trip(pool: &Pool) {
    let a = doc(pool, Mode::Published, None).await;
    let b = doc(pool, Mode::Archived, Some(Mode::Draft)).await;
    let rows: Vec<Doc> = Doc::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("fetch");
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].mode, rows[0].note), (Mode::Published, None));
    assert_eq!(
        (rows[1].mode, rows[1].note),
        (Mode::Archived, Some(Mode::Draft))
    );
    // Stored as the canonical name, renamed variant included.
    let stored: Vec<String> = Doc::objects()
        .order_by(&[("id", false)])
        .pluck("mode", pool)
        .await
        .expect("raw text");
    assert_eq!(stored, ["pub", "archived"]);
    assert!(a < b);
}

async fn filters_bind_text(pool: &Pool) {
    doc(pool, Mode::Published, None).await;
    doc(pool, Mode::Archived, Some(Mode::Published)).await;
    doc(pool, Mode::Draft, None).await;
    let published = Doc::objects()
        .where_(Doc::mode.eq(Mode::Published))
        .fetch(pool)
        .await
        .expect("eq");
    assert_eq!(published.len(), 1, "{}", pool.dialect().name());
    let two = Doc::objects()
        .where_(Doc::mode.is_in([Mode::Published, Mode::Draft]))
        .count(pool)
        .await
        .expect("in");
    assert_eq!(two, 2);
    let noted = Doc::objects()
        .where_(Doc::note.eq(Some(Mode::Published)))
        .count(pool)
        .await
        .expect("nullable eq");
    assert_eq!(noted, 1);
}

async fn updates_write_the_stored_name(pool: &Pool) {
    let id = doc(pool, Mode::Draft, None).await;
    Doc::objects()
        .where_(Doc::id.eq(id))
        .update()
        .set_typed(Doc::mode.set(Mode::Published))
        .set_typed(Doc::note.set(Some(Mode::Archived)))
        .execute_pool(pool)
        .await
        .expect("update");
    let got: Vec<Doc> = Doc::objects().fetch(pool).await.expect("fetch");
    assert_eq!(
        (got[0].mode, got[0].note),
        (Mode::Published, Some(Mode::Archived))
    );
}

async fn flat_reads_decode_the_enum(pool: &Pool) {
    doc(pool, Mode::Archived, None).await;
    let modes: Vec<Mode> = Doc::objects().pluck("mode", pool).await.expect("pluck");
    assert_eq!(modes, [Mode::Archived]);
    let notes: Vec<Option<Mode>> = Doc::objects().pluck("note", pool).await.expect("pluck");
    assert_eq!(notes, [None]);
}

/// VARCHAR above; an unbounded text column decodes the same way.
async fn text_column_decodes(pool: &Pool) {
    let mut p = Plain {
        id: Auto::default(),
        body: "pub".into(),
    };
    p.insert(pool).await.expect("insert");
    let modes: Vec<Mode> = Plain::objects().pluck("body", pool).await.expect("pluck");
    assert_eq!(modes, [Mode::Published]);
}

/// Unknown, wrong-case and padded values are refused, never guessed.
async fn unknown_values_are_decode_errors(pool: &Pool) {
    doc(pool, Mode::Draft, None).await;
    for (stored, shown) in [("shard", "shard"), ("Draft", "Draft"), ("draft ", "draft ")] {
        raw(pool, &format!("UPDATE te_doc SET mode = '{stored}'")).await;
        let err = Doc::objects().fetch(pool).await.expect_err(stored);
        let (column, uv) = decode_error(err);
        assert!(column.contains("mode"), "{column}");
        assert_eq!(uv.value, shown);
        assert_eq!(
            uv.to_string(),
            format!("unknown value `{shown}` for Mode; expected one of `draft`, `pub`, `archived`")
        );
    }
    if pool.dialect().kind() == DialectKind::Sqlite {
        // A TEXT-affinity column turns the integer into the text `5`.
        raw(pool, "UPDATE te_doc SET mode = 5").await;
        let (column, uv) = decode_error(Doc::objects().fetch(pool).await.expect_err("int"));
        assert!(column.contains("mode"), "{column}");
        assert_eq!(uv.value, "5");
    }
}

/// An empty string is an error, not `None`.
async fn empty_string_is_not_null(pool: &Pool) {
    doc(pool, Mode::Draft, Some(Mode::Draft)).await;
    raw(pool, "UPDATE te_doc SET note = ''").await;
    let (column, uv) = decode_error(Doc::objects().fetch(pool).await.expect_err("empty"));
    assert!(column.contains("note"), "{column}");
    assert_eq!(uv.value, "");
}

/// A long bad value is cut at 64 bytes on a char boundary, control
/// characters escaped.
async fn long_bad_values_are_cut(pool: &Pool) {
    let body = format!("{}é{}", "a".repeat(63), "z".repeat(40));
    let mut p = Plain {
        id: Auto::default(),
        body,
    };
    p.insert(pool).await.expect("insert");
    let err = Plain::objects()
        .pluck::<Mode>("body", pool)
        .await
        .expect_err("long");
    let (_, uv) = decode_error(err);
    assert_eq!(uv.value, format!("{}…", "a".repeat(63)));
    raw(pool, "DELETE FROM te_plain").await;
    let mut p = Plain {
        id: Auto::default(),
        body: "a\tb".into(),
    };
    p.insert(pool).await.expect("insert");
    let (_, uv) = decode_error(
        Plain::objects()
            .pluck::<Mode>("body", pool)
            .await
            .expect_err("tab"),
    );
    assert_eq!(uv.value, "a\\tb");
}

/// An alias reads as its variant; a save writes the stored name back.
async fn aliases_read_and_resave_canonical(pool: &Pool) {
    doc(pool, Mode::Archived, None).await;
    raw(pool, "UPDATE te_doc SET mode = 'old_draft'").await;
    let mut rows: Vec<Doc> = Doc::objects().fetch(pool).await.expect("alias reads");
    assert_eq!(rows[0].mode, Mode::Draft);
    rows[0].save(pool).await.expect("save");
    let stored: Vec<String> = Doc::objects().pluck("mode", pool).await.expect("raw");
    assert_eq!(stored, ["draft"]);
}

/// MySQL's default collation matches `Draft` in a filter; the read still
/// refuses it, so the row never decodes as the wrong case.
async fn mysql_collation_matches_but_decode_refuses(pool: &Pool) {
    doc(pool, Mode::Draft, None).await;
    raw(pool, "UPDATE te_doc SET mode = 'Draft'").await;
    let matched = Doc::objects()
        .where_(Doc::mode.eq(Mode::Draft))
        .count(pool)
        .await
        .expect("count");
    let fetched = Doc::objects()
        .where_(Doc::mode.eq(Mode::Draft))
        .fetch(pool)
        .await;
    if pool.dialect().kind() == DialectKind::Mysql {
        assert_eq!(matched, 1);
        decode_error(fetched.expect_err("case-insensitive match"));
    } else {
        assert_eq!(matched, 0);
        assert!(fetched.expect("no match").is_empty());
    }
}

/// The schema carries the enum's values as choices; aliases are not offered.
async fn schema_choices_come_from_variants(_pool: &Pool) {
    let f = Doc::SCHEMA.field("mode").expect("mode");
    assert_eq!(
        f.choices,
        Some(
            &[
                ("draft", "Draft"),
                ("pub", "Published"),
                ("archived", "Archived")
            ][..]
        )
    );
    assert_eq!(f.max_length, Some(16));
    assert_eq!(f.default, Some("'draft'"));
    assert_eq!(
        Mode::VARIANTS
            .iter()
            .map(|(_, s, _)| *s)
            .collect::<Vec<_>>(),
        ["draft", "pub", "archived"]
    );
    assert!(Doc::SCHEMA.field("note").expect("note").nullable);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        round_trip,
        filters_bind_text,
        updates_write_the_stored_name,
        flat_reads_decode_the_enum,
        text_column_decodes,
        unknown_values_are_decode_errors,
        empty_string_is_not_null,
        long_bad_values_are_cut,
        aliases_read_and_resave_canonical,
        mysql_collation_matches_but_decode_refuses,
        schema_choices_come_from_variants,
    ],
}
