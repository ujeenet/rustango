// `Pool` is single-variant on a one-backend build and three-variant
// under `--all-features`, so each module's `let Pool::X(..) = .. else`
// is refutable in one configuration and not the other.
#![allow(irrefutable_let_patterns)]
//! Migration progress reporting (#1320) — execution-based, all three
//! dialects.
//!
//! The runner used to be silent for the length of a run. These assert
//! the event stream an observer now sees: the pending count up front,
//! one `Started` / `Finished` pair per migration in apply order, a
//! `Failed` naming the migration that died, and — the distinction that
//! matters most — `Faked` never masquerading as `Ran`.
//!
//! The apply loop lives on `crate::sql::Pool`, so it is one code path
//! for Postgres, MySQL and SQLite. That is exactly why it is worth
//! running the same scenarios against all three rather than trusting
//! SQLite: the migrate lock is a different mechanism on each
//! (`pg_advisory_lock`, `GET_LOCK`, none), and the events are emitted
//! from inside it.
//!
//! Each scenario body is shared; the cfg-gated modules at the bottom own
//! pool construction and call into `scenarios::check_*`. PG reads
//! `DATABASE_URL`, MySQL reads `MYSQL_TEST_URL`; both skip when unset.

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use rustango::migrate::{
        self, file, Migration, MigrationEvent, Operation, Outcome, SchemaChange, SchemaSnapshot,
        TableSnapshot,
    };
    use rustango::sql::Pool;

    pub const SYSTEM_LEDGER: &str = "__rustango_system_migrations__";
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn snapshot_with_tables(tables: &[&str]) -> SchemaSnapshot {
        let ts: Vec<TableSnapshot> = tables
            .iter()
            .map(|table| {
                serde_json::from_value(serde_json::json!({
                    "name": table,
                    "model": "T",
                    "fields": [
                        {"name": "id", "column": "id", "ty": "i64",
                         "nullable": false, "primary_key": true}
                    ]
                }))
                .unwrap()
            })
            .collect();
        SchemaSnapshot {
            tables: ts,
            ..Default::default()
        }
    }

    /// A migration that creates `table`, carrying the cumulative
    /// snapshot so a chain of them stays internally consistent.
    fn create_table_mig(name: &str, table: &str, all_tables: &[&str]) -> Migration {
        Migration {
            name: name.to_owned(),
            created_at: "2026-09-10T00:00:00Z".into(),
            prev: None,
            atomic: true,
            scope: migrate::MigrationScope::default(),
            replaces: Vec::new(),
            snapshot: snapshot_with_tables(all_tables),
            forward: vec![Operation::Schema(SchemaChange::CreateTable(table.into()))],
        }
    }

    fn write_dir(migs: &[Migration]) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut dir = std::env::temp_dir();
        dir.push(format!("rustango_progress_dir_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for m in migs {
            file::write(&dir.join(format!("{}.json", m.name)), m).unwrap();
        }
        dir
    }

    /// Collects events so a test can assert on the whole sequence.
    #[derive(Default)]
    pub struct Recorder(Mutex<Vec<MigrationEvent>>);

    impl Recorder {
        fn observer(&self) -> impl migrate::MigrationObserver + '_ {
            move |event: MigrationEvent| self.0.lock().unwrap().push(event)
        }

        fn events(&self) -> Vec<MigrationEvent> {
            self.0.lock().unwrap().clone()
        }

        /// A compact rendering, so a failure message shows the sequence
        /// rather than a wall of struct debug.
        fn trace(&self) -> Vec<String> {
            self.events()
                .iter()
                .map(|e| match e {
                    MigrationEvent::Planned { total } => format!("planned:{total}"),
                    MigrationEvent::Started { name, index, total } => {
                        format!("start:{name}:{index}/{total}")
                    }
                    MigrationEvent::Finished {
                        name,
                        index,
                        total,
                        outcome,
                        ..
                    } => {
                        let o = match outcome {
                            Outcome::Ran => "ran",
                            Outcome::RanPartial { .. } => "partial",
                            Outcome::Faked => "faked",
                        };
                        format!("done:{name}:{index}/{total}:{o}")
                    }
                    MigrationEvent::Failed {
                        name, index, total, ..
                    } => format!("fail:{name}:{index}/{total}"),
                })
                .collect()
        }
    }

    /// The happy path: the pending count up front, then a start/finish
    /// pair per migration, numbered and in apply order.
    pub async fn check_reports_every_migration_in_order(pool: &Pool, p: &str) {
        let dir = write_dir(&[
            create_table_mig("0001_a", &format!("{p}_a"), &[&format!("{p}_a")]),
            create_table_mig(
                "0002_b",
                &format!("{p}_b"),
                &[&format!("{p}_a"), &format!("{p}_b")],
            ),
            create_table_mig(
                "0003_c",
                &format!("{p}_c"),
                &[&format!("{p}_a"), &format!("{p}_b"), &format!("{p}_c")],
            ),
        ]);

        let rec = Recorder::default();
        let applied = migrate::migrate_pool_with_progress(pool, &dir, &rec.observer())
            .await
            .expect("migrate");

        assert_eq!(applied.len(), 3);
        assert_eq!(
            rec.trace(),
            vec![
                "planned:3",
                "start:0001_a:1/3",
                "done:0001_a:1/3:ran",
                "start:0002_b:2/3",
                "done:0002_b:2/3:ran",
                "start:0003_c:3/3",
                "done:0003_c:3/3:ran",
            ],
            "{:?}",
            rec.trace()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Planned` is emitted even when there is nothing to do, so a
    /// watcher can render "up to date" instead of waiting for events
    /// that will never come.
    pub async fn check_up_to_date_run_still_plans(pool: &Pool, p: &str) {
        let table = format!("{p}_none");
        let dir = write_dir(&[create_table_mig("0001_a", &table, &[&table])]);

        migrate::migrate_pool(pool, &dir).await.expect("first");

        let rec = Recorder::default();
        let applied = migrate::migrate_pool_with_progress(pool, &dir, &rec.observer())
            .await
            .expect("second");

        assert!(applied.is_empty(), "nothing should be pending");
        assert_eq!(rec.trace(), vec!["planned:0"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failure names the migration that died and stops there — the
    /// whole point, since the old behaviour reported only that
    /// *something* failed.
    pub async fn check_failure_names_the_migration(pool: &Pool, p: &str) {
        // 0002 recreates the table 0001 just made: the second CREATE
        // TABLE collides, and this is a plain (non-fake-initial) run.
        let table = format!("{p}_dup");
        let dir = write_dir(&[
            create_table_mig("0001_ok", &table, &[&table]),
            create_table_mig("0002_boom", &table, &[&table]),
        ]);

        let rec = Recorder::default();
        let err = migrate::migrate_pool_with_progress(pool, &dir, &rec.observer())
            .await
            .expect_err("second migration must collide");

        let trace = rec.trace();
        assert_eq!(
            trace,
            vec![
                "planned:2",
                "start:0001_ok:1/2",
                "done:0001_ok:1/2:ran",
                "start:0002_boom:2/2",
                "fail:0002_boom:2/2",
            ],
            "{trace:?}"
        );

        // The event carries the same failure the caller gets, so a
        // watcher does not have to correlate two sources.
        let Some(MigrationEvent::Failed { error, .. }) = rec.events().pop() else {
            panic!("last event should be Failed: {trace:?}");
        };
        assert_eq!(error, err.to_string());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The table [`check_faked_is_distinguishable`] migrates. The
    /// caller creates it first, because raw `CREATE TABLE` is the one
    /// thing that cannot be shared across dialects.
    pub fn faked_table(p: &str) -> String {
        format!("{p}_pre")
    }

    /// A faked migration must not report as `Ran`. An operator reading
    /// "applied 0003" and one reading "faked 0003" should reach
    /// different conclusions about what is in their database.
    ///
    /// Precondition: the caller has already created [`faked_table`],
    /// standing in for the pre-migration era where the table was built
    /// by the retired lazy `ensure_table` DDL.
    pub async fn check_faked_is_distinguishable(pool: &Pool, p: &str) {
        let table = faked_table(p);
        let dir = write_dir(&[create_table_mig("0001_create", &table, &[&table])]);

        let rec = Recorder::default();
        migrate::migrate_pool_with_ledger_fake_initial_with_progress(
            pool,
            &dir,
            SYSTEM_LEDGER,
            &rec.observer(),
        )
        .await
        .expect("fake-initial migrate");

        assert_eq!(
            rec.trace(),
            vec![
                "planned:1",
                "start:0001_create:1/1",
                "done:0001_create:1/1:faked",
            ],
            "a migration recorded without running its DDL must not report as `ran`: {:?}",
            rec.trace()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The silent path is the one every pre-existing caller takes, and
    /// it must apply exactly what the observed path applies.
    ///
    /// SQLite-only, hence the `allow`: this needs two *independent*
    /// databases, which two temp files give for free. On PG and MySQL
    /// one server means one shared `__rustango_migrations__`, so the
    /// second run would find nothing pending and prove nothing.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub async fn check_observing_changes_nothing(silent: &Pool, watched: &Pool, p: &str) {
        let dir = write_dir(&[
            create_table_mig("0001_a", &format!("{p}_s1"), &[&format!("{p}_s1")]),
            create_table_mig(
                "0002_b",
                &format!("{p}_s2"),
                &[&format!("{p}_s1"), &format!("{p}_s2")],
            ),
        ]);

        let quiet = migrate::migrate_pool(silent, &dir).await.expect("silent");
        let rec = Recorder::default();
        let loud = migrate::migrate_pool_with_progress(watched, &dir, &rec.observer())
            .await
            .expect("watched");

        let names = |ms: &[Migration]| ms.iter().map(|m| m.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&quiet), names(&loud));
        assert_eq!(names(&loud), vec!["0001_a", "0002_b"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(feature = "sqlite")]
mod sqlite_live {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use rustango::sql::{sqlx, Pool};

    use super::scenarios;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A temp *file*, not `:memory:` — every pool connection has to see
    /// the same database, and the migrate path takes more than one.
    async fn fresh_pool() -> (Pool, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("rustango_progress_{}_{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite:{}?mode=rwc", path.display());
        let sq = sqlx::SqlitePool::connect(&url).await.expect("sqlite pool");
        (Pool::Sqlite(sq), path)
    }

    fn cleanup(pool: Pool, path: &PathBuf) {
        drop(pool);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn reports_every_migration_in_order() {
        let (pool, path) = fresh_pool().await;
        scenarios::check_reports_every_migration_in_order(&pool, "sqp1").await;
        cleanup(pool, &path);
    }

    #[tokio::test]
    async fn up_to_date_run_still_plans() {
        let (pool, path) = fresh_pool().await;
        scenarios::check_up_to_date_run_still_plans(&pool, "sqp2").await;
        cleanup(pool, &path);
    }

    #[tokio::test]
    async fn failure_names_the_migration() {
        let (pool, path) = fresh_pool().await;
        scenarios::check_failure_names_the_migration(&pool, "sqp3").await;
        cleanup(pool, &path);
    }

    #[tokio::test]
    async fn faked_is_distinguishable_from_ran() {
        let (pool, path) = fresh_pool().await;
        let table = scenarios::faked_table("sqp4");
        let Pool::Sqlite(sq) = &pool else {
            unreachable!("fresh_pool builds a sqlite pool")
        };
        sqlx::query(&format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY)"))
            .execute(sq)
            .await
            .expect("pre-create");
        scenarios::check_faked_is_distinguishable(&pool, "sqp4").await;
        cleanup(pool, &path);
    }

    #[tokio::test]
    async fn observing_changes_nothing() {
        let (silent, p1) = fresh_pool().await;
        let (watched, p2) = fresh_pool().await;
        scenarios::check_observing_changes_nothing(&silent, &watched, "sqp5").await;
        cleanup(silent, &p1);
        cleanup(watched, &p2);
    }
}

#[cfg(feature = "postgres")]
mod pg_live {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::OnceLock;

    use rustango::sql::{sqlx, Pool};
    use tokio::sync::Mutex;

    use super::scenarios;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// The migrate path takes a session-scoped `pg_advisory_lock` on a
    /// fixed key, so two of these running at once would serialise on it
    /// anyway — and the ledger is shared. Run them one at a time.
    fn live_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    /// `None` when `DATABASE_URL` is unset — the suite skips rather
    /// than fails, matching every other `*_pg_live` test here.
    async fn fresh_pool(prefix: &str) -> Option<Pool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pg = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .ok()?;
        // Each scenario gets its own ledger-free slate: drop anything a
        // previous run left, including the ledger, so `pending` is the
        // whole chain again.
        for t in ["a", "b", "c", "none", "dup", "pre", "s1", "s2"] {
            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {prefix}_{t} CASCADE"))
                .execute(&pg)
                .await;
        }
        for ledger in ["__rustango_migrations__", scenarios::SYSTEM_LEDGER] {
            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {ledger} CASCADE"))
                .execute(&pg)
                .await;
        }
        Some(Pool::Postgres(pg))
    }

    fn prefix(n: u32) -> String {
        format!("pgp{n}_{}", COUNTER.fetch_add(1, Ordering::SeqCst))
    }

    #[tokio::test]
    async fn reports_every_migration_in_order() {
        let _g = live_lock().lock().await;
        let p = prefix(1);
        let Some(pool) = fresh_pool(&p).await else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        scenarios::check_reports_every_migration_in_order(&pool, &p).await;
    }

    #[tokio::test]
    async fn up_to_date_run_still_plans() {
        let _g = live_lock().lock().await;
        let p = prefix(2);
        let Some(pool) = fresh_pool(&p).await else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        scenarios::check_up_to_date_run_still_plans(&pool, &p).await;
    }

    #[tokio::test]
    async fn failure_names_the_migration() {
        let _g = live_lock().lock().await;
        let p = prefix(3);
        let Some(pool) = fresh_pool(&p).await else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        scenarios::check_failure_names_the_migration(&pool, &p).await;
    }

    #[tokio::test]
    async fn faked_is_distinguishable_from_ran() {
        let _g = live_lock().lock().await;
        let p = prefix(4);
        let Some(pool) = fresh_pool(&p).await else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        let table = scenarios::faked_table(&p);
        let Pool::Postgres(pg) = &pool else {
            unreachable!("fresh_pool builds a postgres pool")
        };
        sqlx::query(&format!("CREATE TABLE {table} (id BIGSERIAL PRIMARY KEY)"))
            .execute(pg)
            .await
            .expect("pre-create");
        scenarios::check_faked_is_distinguishable(&pool, &p).await;
    }
}

#[cfg(feature = "mysql")]
mod mysql_live {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::OnceLock;

    use rustango::sql::{sqlx, Pool};
    use tokio::sync::Mutex;

    use super::scenarios;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// MySQL's migrate lock is a named `GET_LOCK`, so concurrent runs
    /// would serialise on it; the ledger is shared besides.
    fn live_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    /// `None` when `MYSQL_TEST_URL` is unset — skip, don't fail.
    async fn fresh_pool(prefix: &str) -> Option<Pool> {
        let url = std::env::var("MYSQL_TEST_URL").ok()?;
        let my = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .ok()?;
        for t in ["a", "b", "c", "none", "dup", "pre", "s1", "s2"] {
            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{prefix}_{t}`"))
                .execute(&my)
                .await;
        }
        for ledger in ["__rustango_migrations__", scenarios::SYSTEM_LEDGER] {
            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{ledger}`"))
                .execute(&my)
                .await;
        }
        Some(Pool::Mysql(my))
    }

    fn prefix(n: u32) -> String {
        format!("myp{n}_{}", COUNTER.fetch_add(1, Ordering::SeqCst))
    }

    #[tokio::test]
    async fn reports_every_migration_in_order() {
        let _g = live_lock().lock().await;
        let p = prefix(1);
        let Some(pool) = fresh_pool(&p).await else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        scenarios::check_reports_every_migration_in_order(&pool, &p).await;
    }

    #[tokio::test]
    async fn up_to_date_run_still_plans() {
        let _g = live_lock().lock().await;
        let p = prefix(2);
        let Some(pool) = fresh_pool(&p).await else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        scenarios::check_up_to_date_run_still_plans(&pool, &p).await;
    }

    #[tokio::test]
    async fn failure_names_the_migration() {
        let _g = live_lock().lock().await;
        let p = prefix(3);
        let Some(pool) = fresh_pool(&p).await else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        scenarios::check_failure_names_the_migration(&pool, &p).await;
    }

    #[tokio::test]
    async fn faked_is_distinguishable_from_ran() {
        let _g = live_lock().lock().await;
        let p = prefix(4);
        let Some(pool) = fresh_pool(&p).await else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        let table = scenarios::faked_table(&p);
        let Pool::Mysql(my) = &pool else {
            unreachable!("fresh_pool builds a mysql pool")
        };
        sqlx::query(&format!(
            "CREATE TABLE `{table}` (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY)"
        ))
        .execute(my)
        .await
        .expect("pre-create");
        scenarios::check_faked_is_distinguishable(&pool, &p).await;
    }
}
