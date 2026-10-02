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
        let _ = vs.router_pool("/api/notes", pool);
    });
    out.contents()
}

#[tokio::test]
async fn open_writes_warn_unless_acknowledged() {
    let pool = Pool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
    let vs = || ViewSet::for_model(Note::SCHEMA);

    let open = mount_logs(vs(), pool.clone());
    assert!(open.contains("WARN"), "{open}");
    assert!(open.contains("rustango::viewset"), "{open}");
    assert!(open.contains("vs_open_note"), "{open}");

    // Only the one open write action is named.
    let one_open = mount_logs(
        vs().permissions(ViewSetPerms {
            create: vec!["vs_open_note.add".into()],
            update: vec!["vs_open_note.change".into()],
            ..ViewSetPerms::default()
        }),
        pool.clone(),
    );
    assert!(
        one_open.contains("destroy") && !one_open.contains("create"),
        "{one_open}"
    );

    for (what, quiet) in [
        ("read_only", vs().read_only()),
        ("allow_anonymous", vs().allow_anonymous()),
        (
            "permissions",
            vs().permissions(ViewSetPerms {
                create: vec!["vs_open_note.add".into()],
                update: vec!["vs_open_note.change".into()],
                destroy: vec!["vs_open_note.delete".into()],
                ..ViewSetPerms::default()
            }),
        ),
    ] {
        let logs = mount_logs(quiet, pool.clone());
        assert!(!logs.contains("WARN"), "{what}: {logs}");
    }
}
