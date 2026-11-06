//! A ViewSet whose write actions need no permission refuses to mount,
//! unless it is read-only or says `.allow_anonymous()`.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "runtime"))]

use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::viewset::ViewSet;
#[cfg(feature = "tenancy")]
use rustango::viewset::ViewSetPerms;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_open_note")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

/// The panic message of mounting `vs`, or `None` when it mounted.
fn refusal(vs: ViewSet, pool: Pool) -> Option<String> {
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = vs.router("/api/notes", pool);
    }))
    .err()?;
    Some(err.downcast_ref::<String>().cloned().unwrap_or_default())
}

#[tokio::test]
async fn open_writes_refuse_unless_acknowledged() {
    let pool = Pool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
    let vs = || ViewSet::for_model(Note::SCHEMA, &["id", "title"]);

    let open = refusal(vs(), pool.clone()).expect("open writes must not mount");
    assert!(open.contains("ViewSet `vs_open_note`"), "{open}");
    assert!(
        open.contains("create") && open.contains("update") && open.contains("destroy"),
        "{open}"
    );
    // The fix it names exists in this build; the no-tenancy text is a
    // unit test in `viewset`, which the `sqlite,admin` lib run covers.
    assert!(
        open.contains("allow_anonymous") && open.contains("read_only"),
        "{open}"
    );
    #[cfg(feature = "tenancy")]
    assert!(open.contains("permissions_for_model"), "{open}");

    for (what, decided) in [
        ("read_only", vs().read_only()),
        ("allow_anonymous", vs().allow_anonymous()),
    ] {
        assert_eq!(refusal(decided, pool.clone()), None, "{what}");
    }
}

#[cfg(feature = "tenancy")]
fn write_perms(destroy: bool) -> ViewSetPerms {
    ViewSetPerms {
        create: vec!["vs_open_note.add".into()],
        update: vec!["vs_open_note.change".into()],
        destroy: if destroy {
            vec!["vs_open_note.delete".into()]
        } else {
            Vec::new()
        },
        ..ViewSetPerms::default()
    }
}

/// With tenancy, codenames close the write actions they name.
#[cfg(feature = "tenancy")]
#[tokio::test]
async fn codenames_close_open_writes() {
    let pool = Pool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
    let vs = || ViewSet::for_model(Note::SCHEMA, &["id", "title"]);
    let one_open = refusal(vs().permissions(write_perms(false)), pool.clone())
        .expect("an open destroy must not mount");
    assert!(
        one_open.contains("destroy") && !one_open.contains("create"),
        "{one_open}"
    );
    assert_eq!(refusal(vs().permissions(write_perms(true)), pool), None);
}
