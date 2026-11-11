#![cfg(all(feature = "postgres", feature = "tenancy"))]
//! Schema tenants log in as their own roles: tenant SQL that names another
//! tenant's schema is refused, even after it resets the session role.
//!
//! Skipped silently when `DATABASE_URL` is unset.

#[path = "support/scratch_db.rs"]
mod scratch_db;

use rustango::core::Column as _;
use rustango::session::SessionSecret;
use rustango::sql::sqlx::{self, PgPool};
use rustango::tenancy::{Org, TenantPools, TenantPoolsConfig};
use scratch_db::ScratchDb;

struct Env {
    db: ScratchDb,
    raw: PgPool,
    dir: tempfile::TempDir,
}

impl Env {
    async fn boot(prefix: &str) -> Option<Self> {
        let admin = std::env::var("DATABASE_URL").ok()?;
        let db = ScratchDb::create(&admin, prefix).await;
        let raw = PgPool::connect(db.url()).await.expect("connect");
        Some(Self {
            db,
            raw,
            dir: tempfile::tempdir().expect("migrations dir"),
        })
    }

    fn pools(&self, roles: bool) -> TenantPools<sqlx::Postgres> {
        TenantPools::new(self.raw.clone())
            .config(TenantPoolsConfig {
                schema_tenant_roles: roles,
                ..TenantPoolsConfig::default()
            })
            .tenant_role_secret(SessionSecret::from_bytes(vec![42; 32]))
    }

    async fn run(
        &self,
        pools: &TenantPools<sqlx::Postgres>,
        args: &[&str],
    ) -> Result<String, String> {
        let mut buf: Vec<u8> = Vec::new();
        let args = rustango::testkit::routed_args(args);
        rustango::tenancy::manage::run_with_writer(
            pools,
            self.db.url(),
            self.dir.path(),
            args,
            &mut buf,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    async fn create_schema_tenant(&self, pools: &TenantPools<sqlx::Postgres>, slug: &str) -> Org {
        self.run(
            pools,
            &[
                "create-tenant",
                slug,
                "--mode",
                "schema",
                "--schema-name",
                slug,
            ],
        )
        .await
        .expect("create-tenant");
        Org::objects()
            .where_(Org::slug.eq(slug))
            .fetch_on(&self.raw)
            .await
            .unwrap()
            .remove(0)
    }

    async fn registry_user(&self) -> String {
        sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&self.raw)
            .await
            .unwrap()
    }

    async fn role_exists(&self, name: &str) -> bool {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(name)
            .fetch_one(&self.raw)
            .await
            .unwrap()
    }
}

async fn current_user(c: &mut sqlx::PgConnection) -> String {
    sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(c)
        .await
        .unwrap()
}

/// The SQLSTATE of `sql` on `c`; `None` when it ran.
async fn refusal(c: &mut sqlx::PgConnection, sql: &str) -> Option<String> {
    match sqlx::query(sql).execute(c).await {
        Ok(_) => None,
        Err(sqlx::Error::Database(d)) => d.code().map(|c| c.into_owned()),
        Err(e) => panic!("{sql}: {e}"),
    }
}

/// An injected role reset is still the tenant: another tenant's schema and
/// the registry stay closed on both the queries pool and held connections.
#[tokio::test]
async fn tenant_sql_cannot_read_another_tenants_schema() {
    let Some(env) = Env::boot("rgo_roles_iso").await else {
        return;
    };
    let pools = env.pools(true);
    env.run(&pools, &["migrate-registry"])
        .await
        .expect("registry");
    let alpha = env.create_schema_tenant(&pools, "alpha").await;
    let beta = env.create_schema_tenant(&pools, "beta").await;
    // Made after provisioning: default privileges give it to beta's role.
    sqlx::query("CREATE TABLE beta.notes (x int)")
        .execute(&env.raw)
        .await
        .unwrap();
    sqlx::query("INSERT INTO beta.notes VALUES (42)")
        .execute(&env.raw)
        .await
        .unwrap();
    let registry = env.registry_user().await;

    let scoped = pools.scoped_pool(&alpha).await.expect("alpha pool");
    let mut c = scoped.acquire().await.unwrap();
    let role = current_user(&mut c).await;
    assert!(role.starts_with("rgo_t_"), "alpha runs as {role}");
    assert_eq!(
        refusal(&mut c, "SELECT count(*) FROM rustango_users").await,
        None
    );
    assert_eq!(
        refusal(&mut c, "SELECT set_config('role', 'none', false)").await,
        None
    );
    assert_eq!(
        current_user(&mut c).await,
        role,
        "the reset left the tenant role"
    );
    assert_eq!(
        refusal(&mut c, "SELECT * FROM beta.notes").await.as_deref(),
        Some("42501")
    );
    assert_eq!(
        refusal(&mut c, "SELECT * FROM public.rustango_orgs")
            .await
            .as_deref(),
        Some("42501")
    );
    let set = format!("SET ROLE \"{registry}\"");
    assert_eq!(refusal(&mut c, &set).await.as_deref(), Some("42501"));
    drop(c);

    let mut held = pools.acquire(&alpha).await.expect("held");
    assert_eq!(current_user(&mut held).await, role);
    assert_eq!(
        refusal(&mut held, "SELECT * FROM beta.notes")
            .await
            .as_deref(),
        Some("42501")
    );
    drop(held);

    // Uncached, a held connection is not borrowed from the registry.
    let uncached = TenantPools::new(env.raw.clone())
        .config(TenantPoolsConfig {
            max_cached_scoped_pools: 0,
            ..TenantPoolsConfig::default()
        })
        .tenant_role_secret(SessionSecret::from_bytes(vec![42; 32]));
    let mut c = uncached.acquire(&alpha).await.expect("uncached");
    assert_eq!(current_user(&mut c).await, role);
    drop(c);

    let beta_pool = pools.scoped_pool(&beta).await.expect("beta pool");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM notes")
        .fetch_one(&beta_pool)
        .await
        .expect("beta reads its own table");
    assert_eq!(n, 1);

    for slug in ["alpha", "beta"] {
        env.run(&pools, &["purge-tenant", slug, "--confirm", slug])
            .await
            .expect("purge");
    }
    assert!(!env.role_exists(&role).await, "purge left alpha's role");
}

/// A tenant from before roles gets one on its first login or from the
/// backfill verb; with roles off it keeps the registry login.
#[tokio::test]
async fn tenants_from_before_roles_get_them() {
    let Some(env) = Env::boot("rgo_roles_backfill").await else {
        return;
    };
    let off = env.pools(false);
    env.run(&off, &["migrate-registry"])
        .await
        .expect("registry");
    let gamma = env.create_schema_tenant(&off, "gamma").await;
    let delta = env.create_schema_tenant(&off, "delta").await;
    let registry = env.registry_user().await;
    let mut c = off
        .scoped_pool(&gamma)
        .await
        .unwrap()
        .acquire()
        .await
        .unwrap();
    assert_eq!(
        current_user(&mut c).await,
        registry,
        "roles off share the login"
    );
    drop(c);

    let on = env.pools(true);
    let out = env
        .run(&on, &["ensure-tenant-roles"])
        .await
        .expect("backfill");
    assert!(
        out.contains("gamma") && out.contains("2 schema tenant"),
        "{out}"
    );
    let mut c = on
        .scoped_pool(&gamma)
        .await
        .unwrap()
        .acquire()
        .await
        .unwrap();
    let role = current_user(&mut c).await;
    assert!(role.starts_with("rgo_t_"), "{role}");
    assert_eq!(
        refusal(&mut c, "SELECT count(*) FROM rustango_users").await,
        None
    );
    drop(c);

    // No backfill: the first login sets the role up.
    let fresh = env.pools(true);
    let delta_role = {
        let mut c = fresh
            .scoped_pool(&delta)
            .await
            .expect("first login")
            .acquire()
            .await
            .unwrap();
        current_user(&mut c).await
    };
    assert!(env.role_exists(&delta_role).await);
    // A schema recreated by hand loses its grants; the next pool restores them.
    for sql in [
        "DROP SCHEMA delta CASCADE",
        "CREATE SCHEMA delta",
        "CREATE TABLE delta.t (x int)",
    ] {
        sqlx::query(sql).execute(&env.raw).await.unwrap();
    }
    let again = env.pools(true);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM t")
        .fetch_one(&again.scoped_pool(&delta).await.expect("recreated schema"))
        .await
        .expect("grants on the recreated schema");
    assert_eq!(n, 0);
    // A rotated secret is picked up the same way.
    let rotated = TenantPools::new(env.raw.clone())
        .tenant_role_secret(SessionSecret::from_bytes(vec![7; 32]));
    rotated.scoped_pool(&delta).await.expect("rotated secret");

    for slug in ["gamma", "delta"] {
        env.run(&on, &["purge-tenant", slug, "--confirm", slug])
            .await
            .expect("purge");
    }
    assert!(!env.role_exists(&role).await && !env.role_exists(&delta_role).await);
}

/// With schema tenants, a registry login that cannot create roles is refused
/// at boot, with the fix in the message.
#[tokio::test]
async fn a_registry_login_without_createrole_is_refused() {
    let Some(env) = Env::boot("rgo_roles_nocr").await else {
        return;
    };
    let off = env.pools(false);
    env.run(&off, &["migrate-registry"])
        .await
        .expect("registry");
    env.create_schema_tenant(&off, "eps").await;
    let login = format!("rgo_nocr_{}", std::process::id());
    for sql in [
        format!("DROP ROLE IF EXISTS {login}"),
        format!("CREATE ROLE {login} LOGIN PASSWORD 'pw' NOCREATEROLE"),
        format!("GRANT SELECT ON ALL TABLES IN SCHEMA public TO {login}"),
    ] {
        sqlx::query(&sql).execute(&env.raw).await.unwrap();
    }
    let opts: sqlx::postgres::PgConnectOptions = env.db.url().parse().unwrap();
    let weak = PgPool::connect_with(opts.username(&login).password("pw"))
        .await
        .unwrap();
    let pools =
        TenantPools::new(weak.clone()).tenant_role_secret(SessionSecret::from_bytes(vec![42; 32]));
    let err = env
        .run(&pools, &["list-tenants"])
        .await
        .expect_err("no CREATEROLE");
    assert!(err.contains("CREATEROLE"), "{err}");
    let off_weak = TenantPools::new(weak.clone()).config(TenantPoolsConfig {
        schema_tenant_roles: false,
        ..TenantPoolsConfig::default()
    });
    env.run(&off_weak, &["list-tenants"])
        .await
        .expect("roles off");
    weak.close().await;
    for sql in [
        format!("DROP OWNED BY {login}"),
        format!("DROP ROLE {login}"),
    ] {
        sqlx::query(&sql).execute(&env.raw).await.unwrap();
    }
    env.run(&off, &["purge-tenant", "eps", "--confirm", "eps"])
        .await
        .expect("purge");
}
