//! A ViewSet whose write actions need no permission warns at mount time,
//! unless it is read-only or says `.allow_anonymous()` (#1857).

#![cfg(all(
    feature = "sqlite",
    feature = "admin",
    feature = "runtime",
    feature = "testkit"
))]

use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::testkit::CaptureWriter;
use rustango::viewset::{ViewSet, ViewSetPerms};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_open_note")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

/// What mounting `vs` logs.
fn mount_logs(vs: ViewSet, pool: Pool) -> String {
    let out = CaptureWriter::default();
    let sub = tracing_subscriber::fmt()
        .with_writer(out.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(sub, || {
        let _ = vs.router("/api/notes", pool);
    });
    out.contents()
}

#[tokio::test]
async fn open_writes_warn_unless_acknowledged() {
    let pool = Pool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
    let vs = || ViewSet::for_model(Note::SCHEMA, &["id", "title"]);

    let open = mount_logs(vs(), pool.clone());
    assert!(open.contains("WARN"), "{open}");
    assert!(open.contains("rustango::viewset"), "{open}");
    assert!(open.contains("vs_open_note"), "{open}");
    // The fix it names exists in this build; the no-tenancy text is a
    // unit test in `viewset`, which the `sqlite,admin` lib run covers.
    assert!(
        open.contains("allow_anonymous") && open.contains("read_only"),
        "{open}"
    );
    #[cfg(feature = "tenancy")]
    assert!(open.contains("permissions_for_model"), "{open}");

    for (what, quiet) in [
        ("read_only", vs().read_only()),
        ("allow_anonymous", vs().allow_anonymous()),
    ] {
        let logs = mount_logs(quiet, pool.clone());
        assert!(!logs.contains("WARN"), "{what}: {logs}");
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
    let one_open = mount_logs(vs().permissions(write_perms(false)), pool.clone());
    assert!(
        one_open.contains("destroy") && !one_open.contains("create"),
        "{one_open}"
    );
    let logs = mount_logs(vs().permissions(write_perms(true)), pool);
    assert!(!logs.contains("WARN"), "{logs}");
}
