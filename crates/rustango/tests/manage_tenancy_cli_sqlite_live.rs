#![cfg(all(feature = "sqlite", feature = "tenancy"))]
//! Tenancy CLI verbs honour their arguments (#1909, #1910).
//!
//! Each case once ran the real action with the flag dropped: a
//! `--dry-run` that migrated, a `migrate <target>` that put tenant tables
//! in the registry, a flag taken as a username.

use rustango::migrate::{file, DataOp, Migration, MigrationScope, Operation, SchemaChange};
use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;

struct Booted {
    pools: TenantPools<sqlx::Sqlite>,
    raw: sqlx::SqlitePool,
    url: String,
    _tmp: tempfile::TempDir,
    migrations: tempfile::TempDir,
}

fn migration(name: &str, table: &str, scope: MigrationScope) -> Migration {
    Migration {
        name: name.to_owned(),
        created_at: "2026-09-30T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope,
        replaces: Vec::new(),
        merges: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({
            "tables": [{
                "name": table, "model": "T",
                "fields": [{"name": "id", "column": "id", "ty": "i64",
                            "nullable": false, "primary_key": true}]
            }]
        }))
        .unwrap(),
        forward: vec![Operation::Schema(SchemaChange::CreateTable(table.into()))],
    }
}

/// A registry with nothing applied, over a dir holding one registry-scoped
/// and one tenant-scoped migration.
async fn boot() -> Booted {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let raw = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = TenantPools::<sqlx::Sqlite>::new(raw.clone());
    let migrations = tempfile::tempdir().expect("migrations dir");
    for m in [
        migration("0001_reg", "reg_t", MigrationScope::Registry),
        migration("0002_ten", "ten_t", MigrationScope::Tenant),
    ] {
        file::write(&migrations.path().join(format!("{}.json", m.name)), &m).unwrap();
    }
    Booted {
        pools,
        raw,
        url,
        _tmp: tmp,
        migrations,
    }
}

impl Booted {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut buf: Vec<u8> = Vec::new();
        rustango::tenancy::manage::run_with_writer(
            &self.pools,
            &self.url,
            self.migrations.path(),
            args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            &mut buf,
        )
        .await
        .map(|()| String::from_utf8_lossy(&buf).into_owned())
        .map_err(|e| e.to_string())
    }

    async fn has_table(&self, name: &str) -> bool {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(name)
        .fetch_one(&self.raw)
        .await
        .expect("sqlite_master");
        n > 0
    }
}

#[tokio::test]
async fn migrate_registry_dry_run_and_help_apply_nothing() {
    let b = boot().await;
    let out = b
        .run(&["migrate-registry", "--dry-run"])
        .await
        .expect("dry-run");
    assert!(!b.has_table("reg_t").await, "--dry-run migrated: {out}");
    assert!(
        out.contains("0001_reg"),
        "should preview the pending one: {out}"
    );

    assert!(b.run(&["migrate-registry", "--help"]).await.is_err());
    assert!(b.run(&["migrate-registry", "--bogus"]).await.is_err());
    assert!(!b.has_table("reg_t").await, "--help / --bogus migrated");
}

#[tokio::test]
async fn migrate_tenants_refuses_flags_it_does_not_have() {
    let b = boot().await;
    b.run(&["migrate-registry"]).await.expect("registry");
    for flag in ["--dry-run", "--help", "--bogus"] {
        let err = b.run(&["migrate-tenants", flag]).await.expect_err(flag);
        assert!(err.contains("migrate-tenants"), "{flag}: {err}");
    }
}

#[tokio::test]
async fn migrate_target_and_dry_run_stay_registry_scoped() {
    let b = boot().await;
    let preview = b.run(&["migrate", "--dry-run"]).await.expect("dry-run");
    assert!(
        !preview.contains("0002_ten"),
        "tenant migration listed as pending on the registry: {preview}"
    );
    assert!(preview.contains("-- 0001_reg ("), "{preview}");

    let _ = b.run(&["migrate", "0002_ten"]).await;
    assert!(
        !b.has_table("ten_t").await,
        "`migrate 0002_ten` created a tenant table in the registry"
    );

    b.run(&["migrate", "0001_reg"])
        .await
        .expect("registry target");
    assert!(b.has_table("reg_t").await);
}

/// The registry dry-run previews a pending registry-scope system step.
#[tokio::test]
async fn migrate_dry_run_previews_the_registry_system_chain() {
    let b = boot().await;
    b.run(&["migrate-registry"]).await.expect("registry");
    let step = file::list_dir(&b.migrations.path().join("system").join("migrations"))
        .expect("system chain")
        .into_iter()
        .find(|m| m.scope == MigrationScope::Registry)
        .expect("a registry system step");
    sqlx::query("DELETE FROM __rustango_system_migrations__ WHERE name = ?")
        .bind(&step.name)
        .execute(&b.raw)
        .await
        .expect("forget the step");
    let preview = b.run(&["migrate", "--dry-run"]).await.expect("dry-run");
    assert!(
        preview.contains(&format!("-- {} (system,", step.name)),
        "{preview}"
    );
}

impl Booted {
    /// A migrated database-mode tenant, so user and permission verbs run.
    async fn tenant(&self, slug: &str) {
        self.run(&["migrate-registry"]).await.expect("registry");
        let db = self._tmp.path().join(format!("{slug}.db"));
        self.run(&[
            "create-tenant",
            slug,
            "--mode",
            "database",
            "--backend",
            "sqlite",
            "--database-url",
            &format!("sqlite://{}?mode=rwc", db.display()),
        ])
        .await
        .expect("create the tenant");
    }
}

#[tokio::test]
async fn create_user_never_takes_a_flag_as_the_username() {
    let b = boot().await;
    b.tenant("acme").await;
    let res = b
        .run(&[
            "create-user",
            "acme",
            "--superuser",
            "--skip-validation",
            "--password",
            "pw",
        ])
        .await;
    assert!(res.is_err(), "no username given, but: {res:?}");

    // Flags before and after the positionals mean the same thing.
    let out = b
        .run(&[
            "create-superuser",
            "--skip-validation",
            "--password",
            "pw",
            "acme",
            "bob",
        ])
        .await
        .expect("create-superuser");
    assert!(
        out.contains("`bob`") && out.contains("superuser=true"),
        "{out}"
    );
}

/// A trailing `--password` once took the injected `--superuser` as the password.
#[tokio::test]
async fn a_valued_flag_never_takes_the_next_flag_as_its_value() {
    let b = boot().await;
    b.tenant("acme").await;
    let err = b
        .run(&["create-superuser", "acme", "bob", "--password"])
        .await
        .expect_err("no password given");
    assert!(err.contains("needs a value"), "{err}");
    let gone = b.run(&["set-superuser", "acme", "bob", "--off"]).await;
    assert!(gone.is_err(), "a user was created anyway: {gone:?}");
}

#[tokio::test]
async fn grant_perm_refuses_a_misspelt_role_flag() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ])
    .await
    .expect("user");
    for verb in ["grant-perm", "revoke-perm"] {
        let res = b.run(&[verb, "acme", "bob", "post.change", "--rol"]).await;
        assert!(res.is_err(), "{verb} ignored `--rol`: {res:?}");
    }
}

#[tokio::test]
async fn set_host_enabled_reads_the_enabled_value_as_a_value() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&["add-host", "acme", "shop.example.com"])
        .await
        .expect("add");
    let out = b
        .run(&[
            "set-host-enabled",
            "--enabled",
            "false",
            "acme",
            "shop.example.com",
        ])
        .await
        .expect("--enabled false");
    assert!(out.contains("parked"), "{out}");
}

/// A failed tenant fails the verb, so a deploy stops on it (#1844).
#[tokio::test]
async fn a_failed_tenant_fails_the_tenant_verbs() {
    let b = boot().await;
    b.tenant("acme").await;
    let mut bad = migration("0003_bad", "ten_t", MigrationScope::Tenant);
    bad.forward = vec![Operation::Data(DataOp {
        sql: "INSERT INTO no_such_table VALUES (1)".into(),
        reverse_sql: None,
        reversible: false,
    })];
    file::write(&b.migrations.path().join("0003_bad.json"), &bad).unwrap();
    for args in [
        &["migrate-tenants"][..],
        &["migrate"],
        &["migrate", "--fake", "9999_none", "--all-tenants"],
    ] {
        let err = b.run(args).await.expect_err(&args.join(" "));
        assert!(err.contains("1 of 1 tenant(s) failed"), "{args:?}: {err}");
    }
}

/// Tenant-scoped files are marked from the tenant ledgers, not the
/// registry's, where they never land.
#[tokio::test]
async fn showmigrations_reads_the_tenant_ledgers() {
    let b = boot().await;
    b.tenant("acme").await;
    let out = b.run(&["showmigrations"]).await.expect("showmigrations");
    assert!(out.contains("[X] 0001_reg"), "{out}");
    assert!(out.contains("[X] 0002_ten (1/1 tenants)"), "{out}");
}

/// Framework tenant steps are marked from the tenants' system ledgers.
#[tokio::test]
async fn showmigrations_lists_tenant_system_steps() {
    let b = boot().await;
    b.tenant("acme").await;
    let step = file::list_dir(&b.migrations.path().join("system").join("migrations"))
        .expect("system chain")
        .into_iter()
        .find(|m| m.scope == MigrationScope::Tenant)
        .expect("a tenant system step");
    let out = b.run(&["showmigrations"]).await.expect("showmigrations");
    assert!(
        out.contains(&format!("[X] {} (1/1 tenants)", step.name)),
        "{out}"
    );

    let url = format!(
        "sqlite://{}?mode=rwc",
        b._tmp.path().join("acme.db").display()
    );
    let tenant = sqlx::SqlitePool::connect(&url).await.expect("tenant db");
    sqlx::query("DELETE FROM __rustango_system_migrations__ WHERE name = ?")
        .bind(&step.name)
        .execute(&tenant)
        .await
        .expect("forget the step");
    tenant.close().await;
    let out = b.run(&["showmigrations"]).await.expect("showmigrations");
    assert!(
        out.contains(&format!("[ ] {} (0/1 tenants)", step.name)),
        "{out}"
    );
}

/// With no tenant, nothing tenant-scoped reads as pending.
#[tokio::test]
async fn showmigrations_without_tenants_owes_nothing() {
    let b = boot().await;
    b.run(&["migrate-registry"]).await.expect("registry");
    let out = b.run(&["showmigrations"]).await.expect("showmigrations");
    assert!(out.contains("[-] 0002_ten (no tenants)"), "{out}");
    assert!(!out.contains("[ ]"), "{out}");
}

/// An unreachable tenant fails `prewarm-pools` and `audit-cleanup`.
#[tokio::test]
async fn an_unreachable_tenant_fails_prewarm_and_audit_cleanup() {
    use rustango::sql::Auto;
    let b = boot().await;
    b.run(&["migrate-registry"]).await.expect("registry");
    // The parent dir does not exist, so SQLite cannot open the file.
    let gone = b._tmp.path().join("no-such-dir").join("gone.db");
    let mut org = rustango::tenancy::Org {
        id: Auto::default(),
        slug: "gone".into(),
        display_name: "gone".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(format!("sqlite://{}?mode=rwc", gone.display())),
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.insert_pool(&b.pools.registry_pool())
        .await
        .expect("seed org");
    for args in [&["prewarm-pools"][..], &["audit-cleanup", "--days", "30"]] {
        let err = b.run(args).await.expect_err(&args.join(" "));
        assert!(err.contains("1 of 1 tenant(s) failed"), "{args:?}: {err}");
    }
}

/// Trailing arguments and stray flags once ran the bare verb (#1952).
#[tokio::test]
async fn role_and_operator_verbs_refuse_extra_arguments() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ])
    .await
    .expect("user");
    b.run(&["create-role", "acme", "editor"])
        .await
        .expect("role");
    for args in [
        &["assign-role", "acme", "bob", "editor", "junk"][..],
        &["revoke-role", "acme", "bob", "editor", "junk"],
        &["list-roles", "acme", "junk"],
        &["list-operators", "-active"],
        &["prewarm-pools", "-x"],
    ] {
        let res = b.run(args).await;
        assert!(res.is_err(), "{args:?} ran: {res:?}");
    }

    for name in ["alice", "carol"] {
        b.run(&[
            "create-operator",
            name,
            "--skip-validation",
            "--password",
            "pw",
        ])
        .await
        .expect("operator");
    }
    let res = b
        .run(&["set-operator-active", "alice", "carol", "--off"])
        .await;
    assert!(res.is_err(), "a second username was ignored: {res:?}");
    let list = b.run(&["list-operators"]).await.expect("list");
    assert!(list.contains("2 active"), "{list}");
}

/// Flags may sit anywhere, and `--on --off` is refused (#1952).
#[tokio::test]
async fn user_verbs_take_flags_anywhere() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ])
    .await
    .expect("user");
    let res = b
        .run(&["set-superuser", "acme", "bob", "--on", "--off"])
        .await;
    assert!(res.is_err(), "contradicting flags ran: {res:?}");
    let out = b
        .run(&["set-superuser", "--off", "acme", "bob"])
        .await
        .expect("leading flag");
    assert!(out.contains("is_superuser=false"), "{out}");
    b.run(&[
        "reset-password",
        "--skip-validation",
        "--password",
        "pw2",
        "acme",
        "bob",
    ])
    .await
    .expect("leading flag");
}

/// set-superuser and reset-password write through the ORM (#1952).
#[tokio::test]
async fn set_superuser_and_reset_password_write_the_row() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ])
    .await
    .expect("user");
    let url = format!("sqlite://{}", b._tmp.path().join("acme.db").display());
    let tenant = sqlx::SqlitePool::connect(&url).await.expect("tenant db");
    let is_super = || {
        sqlx::query_scalar::<_, bool>(
            "SELECT is_superuser FROM rustango_users WHERE username = 'bob'",
        )
        .fetch_one(&tenant)
    };
    assert!(is_super().await.unwrap(), "the first user is promoted");
    b.run(&["set-superuser", "acme", "bob", "--off"])
        .await
        .expect("off");
    assert!(!is_super().await.unwrap());
    assert!(b.run(&["set-superuser", "acme", "nobody"]).await.is_err());

    b.run(&[
        "reset-password",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw2",
    ])
    .await
    .expect("reset");
    b.run(&[
        "change-password",
        "acme",
        "bob",
        "--current",
        "pw2",
        "--skip-validation",
        "--password",
        "pw3",
    ])
    .await
    .expect("the reset password works");
}

/// The remaining user and key verbs take flags before positionals too (#2225).
#[tokio::test]
async fn password_and_key_verbs_take_flags_anywhere() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&[
        "create-operator",
        "--skip-validation",
        "--password",
        "pw",
        "alice",
    ])
    .await
    .expect("create-operator");
    b.run(&[
        "reset-operator-password",
        "--skip-validation",
        "--password",
        "pw2",
        "alice",
    ])
    .await
    .expect("reset-operator-password");
    b.run(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ])
    .await
    .expect("user");
    b.run(&[
        "change-password",
        "--current",
        "pw",
        "--skip-validation",
        "--password",
        "pw2",
        "acme",
        "bob",
    ])
    .await
    .expect("change-password");
    b.run(&["create-api-key", "--label", "ci", "acme", "bob"])
        .await
        .expect("create-api-key");
    assert!(b
        .run(&["create-api-key", "acme", "bob", "junk"])
        .await
        .is_err());
}

/// `--expires-days` out of range is a validation error, not a panic (#2395).
#[tokio::test]
async fn api_key_expiry_out_of_range_is_refused() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ])
    .await
    .expect("user");
    for days in ["999999999999", "0", "-3"] {
        let err = b
            .run(&["create-api-key", "acme", "bob", "--expires-days", days])
            .await
            .expect_err(days);
        assert!(err.contains("positive number of days"), "{days}: {err}");
    }
    b.run(&["create-api-key", "acme", "bob", "--expires-days", "30"])
        .await
        .expect("a sane expiry");
}

/// `flush` never wipes the registry; `--tenant` clears that tenant only (#2284).
#[tokio::test]
async fn flush_never_touches_the_registry() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ])
    .await
    .expect("user");

    let plain = b.run(&["flush", "--yes"]).await;
    let listed = b.run(&["list-tenants"]).await.expect("list");
    assert!(
        listed.contains("acme"),
        "flush wiped the registry: {listed}"
    );
    let err = plain.expect_err("plain flush");
    assert!(err.contains("--tenant"), "{err}");
    let out = b
        .run(&["flush", "--tenant", "acme", "--yes", "--model", "Org"])
        .await
        .expect("registry model filtered out");
    assert!(out.contains("no tables match"), "{out}");

    b.run(&["flush", "--tenant", "acme", "--yes", "--model", "User"])
        .await
        .expect("tenant flush");
    assert!(
        b.run(&["set-superuser", "acme", "bob", "--off"])
            .await
            .is_err(),
        "the tenant's user survived the flush"
    );
    let listed = b.run(&["list-tenants"]).await.expect("list");
    assert!(listed.contains("acme"), "{listed}");
}

/// Dry run, unknown slug, and a whole-tenant flush with no filter (#2284).
#[tokio::test]
async fn flush_tenant_dry_run_unknown_slug_and_no_filter() {
    let b = boot().await;
    b.tenant("acme").await;
    b.run(&[
        "create-user",
        "acme",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ])
    .await
    .expect("user");

    let out = b
        .run(&["flush", "--tenant", "acme"])
        .await
        .expect("dry run");
    assert!(out.contains("would clear"), "{out}");
    b.run(&["set-superuser", "acme", "bob", "--off"])
        .await
        .expect("a dry run deleted the user");

    let err = b
        .run(&["flush", "--tenant", "nope", "--yes"])
        .await
        .expect_err("unknown slug");
    assert!(err.contains("not found"), "{err}");

    let out = b
        .run(&["flush", "--tenant", "acme", "--yes"])
        .await
        .expect("whole-tenant flush");
    assert!(out.contains("cleared"), "{out}");
    assert!(
        b.run(&["set-superuser", "acme", "bob", "--off"])
            .await
            .is_err(),
        "the tenant's user survived the flush"
    );
    let listed = b.run(&["list-tenants"]).await.expect("list");
    assert!(listed.contains("acme"), "{listed}");
}

/// `db:restore --clean` would drop the registry with `public` (#2283).
#[tokio::test]
async fn restore_clean_is_refused_under_tenancy() {
    let b = boot().await;
    // A missing file: even unrefused, nothing reaches psql.
    let missing = b._tmp.path().join("nope.sql");
    let err = b
        .run(&["db:restore", "--clean", "--yes", missing.to_str().unwrap()])
        .await
        .expect_err("--clean under tenancy");
    assert!(err.contains("registry"), "{err}");
}
