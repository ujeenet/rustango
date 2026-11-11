//! Schema tenants log in as a role of their own, with rights on their schema
//! only. `search_path` alone kept nothing out: SQL naming another tenant's
//! schema ran as the shared registry login. A role switched to with `SET ROLE`
//! can be switched back from tenant SQL, so each tenant gets a LOGIN role.
//!
//! The role's password is an HMAC of its name under [`ROLE_SECRET_ENV`], so
//! every process derives the same one and nothing is stored.
//!
//! A tenant role creates nothing: no trigger, function, table or temp object
//! the registry login could later run or resolve.

use crate::session::{SessionSecret, SessionSecretError};
use crate::sql::server::{Login, LoginName, LoginPassword};
use crate::sql::sqlx::{self, postgres::PgConnectOptions, postgres::PgPool};
use crate::sql::Dialect as _;

use super::error::TenancyError;

/// Env var holding the base64 key (32+ bytes) tenant role passwords derive from.
/// Required on the prod tier; dev keeps one in `./var/.rustango_tenant_role.key`.
pub const ROLE_SECRET_ENV: &str = "RUSTANGO_TENANT_ROLE_SECRET";

/// The secret [`ROLE_SECRET_ENV`] held before a rotation. Pools still log in
/// with it until `ensure-tenant-roles` moves the roles to the new one.
pub const ROLE_SECRET_PREVIOUS_ENV: &str = "RUSTANGO_TENANT_ROLE_SECRET_PREVIOUS";

const ROLE_KEY_PATH: &str = "./var/.rustango_tenant_role.key";
const PASSWORD_TAG: &[u8] = b"rustango.tenant-role.v1";
const HINT: &str = "generate one with: openssl rand -base64 32";

/// The role passwords' secret, and the one before a rotation.
#[derive(Clone)]
pub(crate) struct RoleSecrets {
    pub(crate) current: SessionSecret,
    pub(crate) previous: Option<SessionSecret>,
}

fn parse_secret(var: &str, raw: &str) -> Result<SessionSecret, String> {
    SessionSecret::from_b64(raw).map_err(|e| {
        let why = match e {
            SessionSecretError::BadBase64 { cause } => format!("not valid base64 ({cause})"),
            SessionSecretError::TooShort { actual } => {
                format!("{actual} bytes once decoded; it needs 32")
            }
            _ => "unusable".to_owned(),
        };
        format!("{var} is {why}; {HINT}")
    })
}

/// [`ROLE_SECRET_ENV`], else the dev key file; plus [`ROLE_SECRET_PREVIOUS_ENV`].
pub(crate) fn load_secret() -> Result<RoleSecrets, String> {
    let current = match std::env::var(ROLE_SECRET_ENV) {
        Ok(raw) => parse_secret(ROLE_SECRET_ENV, &raw)?,
        Err(_) if crate::env::is_prod_tier(&crate::env::tier_from_env()) => {
            return Err(format!(
                "{ROLE_SECRET_ENV} is not set: schema tenants log in as roles whose passwords \
                 derive from it ({HINT}), or turn off TenantPoolsConfig::schema_tenant_roles"
            ))
        }
        Err(_) => crate::session::load_or_create_key_file(
            std::path::Path::new(ROLE_KEY_PATH),
            "tenant role secret",
            ROLE_SECRET_ENV,
        )
        .map(SessionSecret::from_bytes)
        .map_err(|e| {
            format!("could not keep the dev tenant role secret at {ROLE_KEY_PATH}: {e}")
        })?,
    };
    let previous = match std::env::var(ROLE_SECRET_PREVIOUS_ENV) {
        Ok(raw) => Some(parse_secret(ROLE_SECRET_PREVIOUS_ENV, &raw)?),
        Err(_) => None,
    };
    Ok(RoleSecrets { current, previous })
}

/// The role of the tenant whose schema is `schema` in `database`. Role names
/// are global to the server, so the database is part of the name.
pub(crate) fn role_name(database: &str, schema: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{database}\0{schema}").as_bytes());
    format!("rgo_t_{}", &crate::hex::hex_encode(&digest)[..24])
}

/// A tenant's login. `Debug` never shows the password.
#[derive(Clone, Debug)]
pub(crate) struct TenantRole {
    pub(crate) name: String,
    login: Login,
    password: LoginPassword,
}

impl TenantRole {
    pub(crate) fn derive(secret: &SessionSecret, database: &str, schema: &str) -> Self {
        use base64::Engine as _;
        let name = role_name(database, schema);
        let mac = crate::session::sign_tagged(secret, PASSWORD_TAG, name.as_bytes());
        let password = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac);
        Self {
            login: Login::new(LoginName::new(&name).expect("`rgo_t_<hex>` is a valid login")),
            password: LoginPassword::new(password).expect("43 base64 characters"),
            name,
        }
    }

    /// `base` (the registry's options) logging in as this role.
    pub(crate) fn login(&self, base: PgConnectOptions) -> PgConnectOptions {
        base.username(&self.name).password(self.password.expose())
    }
}

/// The role exists and may use `schema`.
pub(crate) async fn can_use(
    registry: &PgPool,
    role: &TenantRole,
    schema: &str,
) -> Result<bool, TenancyError> {
    Ok(sqlx::query_scalar(
        "SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $1) \
         THEN has_schema_privilege($1, $2, 'USAGE') ELSE false END",
    )
    .bind(&role.name)
    .bind(schema)
    .fetch_one(registry)
    .await?)
}

/// `true` for a wrong password, as after a secret rotation. Not `28000`:
/// that is `pg_hba.conf`, which no password fixes.
pub(crate) fn is_bad_password(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("28P01"))
}

/// The tenant role's login error, with the fix for a `pg_hba.conf` refusal.
pub(crate) fn login_error(role: &TenantRole, e: sqlx::Error) -> TenancyError {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("28000") => {
            TenancyError::Validation(format!(
                "tenant role `{}` may not log in: {} — let the `rgo_t_*` roles through \
                 pg_hba.conf like the registry login",
                role.name,
                d.message()
            ))
        }
        _ if is_bad_password(&e) => TenancyError::Validation(format!(
            "tenant role `{}` refused its password: run `ensure-tenant-roles` after \
             changing {ROLE_SECRET_ENV}, and keep the old value in \
             {ROLE_SECRET_PREVIOUS_ENV} until it has run",
            role.name
        )),
        _ => e.into(),
    }
}

fn q(ident: &str) -> String {
    crate::sql::Postgres.quote_ident(ident)
}

/// Create `role`, or reset its password to the current secret, and grant it
/// rows in its schema (now and later) and the extension schemas on its path.
/// Run by the registry login, which needs `CREATEROLE`.
pub(crate) async fn ensure(
    registry: &PgPool,
    role: &TenantRole,
    schema: &str,
    extension_schemas: &[String],
) -> Result<(), TenancyError> {
    let pg = crate::sql::Postgres;
    let (r, s) = (q(&role.name), q(schema));
    let mut tx = registry.begin().await?;
    // One process at a time per role, so two first logins don't race.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("rustango.tenant-role:{}", role.name))
        .execute(&mut *tx)
        .await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $1)")
            .bind(&role.name)
            .fetch_one(&mut *tx)
            .await?;
    // The emitters send a SCRAM verifier, so the server never sees the password.
    let login = if exists {
        vec![pg
            .alter_login_password_sql(&role.login, &role.password, false)
            .expect("Postgres alters logins")]
    } else {
        vec![
            pg.create_login_sql(&role.login, &role.password)
                .expect("Postgres creates logins"),
            format!("ALTER ROLE {r} NOINHERIT"),
        ]
    };
    // First: the USAGE checks below need the role.
    for sql in &login {
        run(&mut tx, sql, schema).await?;
    }
    // Rows only: TRIGGER, TRUNCATE or REFERENCES would let tenant SQL act
    // when the registry login writes these tables.
    let mut stmts = vec![
        format!("GRANT USAGE ON SCHEMA {s} TO {r}"),
        format!("GRANT {TABLE_RIGHTS} ON ALL TABLES IN SCHEMA {s} TO {r}"),
        format!("GRANT {SEQUENCE_RIGHTS} ON ALL SEQUENCES IN SCHEMA {s} TO {r}"),
        format!("GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA {s} TO {r}"),
        format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT {TABLE_RIGHTS} ON TABLES TO {r}"),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT {SEQUENCE_RIGHTS} ON SEQUENCES TO {r}"
        ),
        format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT EXECUTE ON FUNCTIONS TO {r}"),
        format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT USAGE ON TYPES TO {r}"),
    ];
    for ext in extension_schemas {
        let usable: bool = sqlx::query_scalar("SELECT has_schema_privilege($1, $2, 'USAGE')")
            .bind(&role.name)
            .bind(ext)
            .fetch_one(&mut *tx)
            .await?;
        if !usable {
            stmts.push(format!("GRANT USAGE ON SCHEMA {} TO {r}", q(ext)));
        }
    }
    for sql in &stmts {
        run(&mut tx, sql, schema).await?;
    }
    revoke_public_writes(&mut tx).await?;
    refuse_writable(&mut *tx, &role.name).await?;
    tx.commit().await?;
    Ok(())
}

const TABLE_RIGHTS: &str = "SELECT, INSERT, UPDATE, DELETE";
const SEQUENCE_RIGHTS: &str = "USAGE, SELECT, UPDATE";

/// Take TEMP and schema CREATE from PUBLIC, where the registry login may.
/// Elsewhere Postgres only warns; [`refuse_writable`] then names the fix.
pub(crate) async fn revoke_public_writes(
    conn: &mut sqlx::PgConnection,
) -> Result<(), TenancyError> {
    let (database, temp): (String, bool) = sqlx::query_as(
        "SELECT current_database()::text, \
         has_database_privilege('public', current_database(), 'TEMP')",
    )
    .fetch_one(&mut *conn)
    .await?;
    if temp {
        let sql = format!("REVOKE TEMPORARY ON DATABASE {} FROM PUBLIC", q(&database));
        sqlx::query(&sql).execute(&mut *conn).await?;
    }
    for schema in creatable_schemas(&mut *conn, "public").await? {
        let sql = format!("REVOKE CREATE ON SCHEMA {} FROM PUBLIC", q(&schema));
        sqlx::query(&sql).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Schemas `grantee` (a role, or `public` for PUBLIC) may create objects in.
async fn creatable_schemas(
    conn: &mut sqlx::PgConnection,
    grantee: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT nspname::text FROM pg_catalog.pg_namespace \
         WHERE has_schema_privilege($1, oid, 'CREATE') ORDER BY 1",
    )
    .bind(grantee)
    .fetch_all(conn)
    .await
}

/// Refuse when `grantee` (a role, or `public`) can create temp objects or
/// objects in any schema: tenant SQL could plant code the registry login runs.
pub(crate) async fn refuse_writable(
    conn: &mut sqlx::PgConnection,
    grantee: &str,
) -> Result<(), TenancyError> {
    let (database, temp): (String, bool) = sqlx::query_as(
        "SELECT current_database()::text, has_database_privilege($1, current_database(), 'TEMP')",
    )
    .bind(grantee)
    .fetch_one(&mut *conn)
    .await?;
    let schemas = creatable_schemas(conn, grantee).await?;
    if !temp && schemas.is_empty() {
        return Ok(());
    }
    let mut fix = Vec::new();
    if temp {
        fix.push(format!(
            "REVOKE TEMPORARY ON DATABASE {} FROM PUBLIC",
            q(&database)
        ));
    }
    for s in &schemas {
        fix.push(format!("REVOKE CREATE ON SCHEMA {} FROM PUBLIC", q(s)));
    }
    let who = if grantee == "public" {
        "every role".to_owned()
    } else {
        format!("tenant role `{grantee}`")
    };
    Err(TenancyError::Validation(format!(
        "{who} can create objects in this database, so tenant SQL could plant code \
         the registry login runs; as the owner run: {}; then `ensure-tenant-roles`",
        fix.join("; ")
    )))
}

/// One setup statement. Never echoed: the login one carries the verifier.
async fn run(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql: &str,
    schema: &str,
) -> Result<(), TenancyError> {
    match sqlx::query(sql).execute(&mut **tx).await {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("42501") => {
            Err(TenancyError::Validation(format!(
                "could not set up the login role of schema `{schema}`: {} — the registry \
                 login needs CREATEROLE and must own the tenant schema",
                d.message()
            )))
        }
        Err(e) => Err(e.into()),
    }
}

/// Drop the role, first revoking what it holds in schemas that outlive it,
/// such as the copy a storage move leaves behind.
pub(crate) async fn drop_role(registry: &PgPool, name: &str) -> Result<(), TenancyError> {
    let held: Vec<String> = sqlx::query_scalar(
        "SELECT n.nspname::text FROM pg_catalog.pg_namespace n \
         CROSS JOIN LATERAL aclexplode(n.nspacl) a \
         JOIN pg_catalog.pg_roles r ON r.oid = a.grantee WHERE r.rolname = $1",
    )
    .bind(name)
    .fetch_all(registry)
    .await?;
    let r = q(name);
    for schema in held {
        let s = q(&schema);
        for sql in [
            format!("REVOKE ALL ON ALL TABLES IN SCHEMA {s} FROM {r}"),
            format!("REVOKE ALL ON ALL SEQUENCES IN SCHEMA {s} FROM {r}"),
            format!("REVOKE ALL ON ALL FUNCTIONS IN SCHEMA {s} FROM {r}"),
            format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} REVOKE ALL ON TABLES FROM {r}"),
            format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} REVOKE ALL ON SEQUENCES FROM {r}"),
            format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} REVOKE ALL ON FUNCTIONS FROM {r}"),
            format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} REVOKE ALL ON TYPES FROM {r}"),
            format!("REVOKE ALL ON SCHEMA {s} FROM {r}"),
        ] {
            sqlx::query(&sql).execute(registry).await?;
        }
    }
    sqlx::query(&format!("DROP ROLE IF EXISTS {}", q(name)))
        .execute(registry)
        .await?;
    Ok(())
}

/// Refuse a registry login that cannot create the tenant roles.
pub(crate) async fn check_can_create_roles(registry: &PgPool) -> Result<(), TenancyError> {
    let sql = crate::sql::Postgres
        .login_privileges_sql()
        .expect("Postgres reads login privileges");
    let (superuser, _createdb, createrole): (bool, bool, bool) =
        sqlx::query_as(sql).fetch_one(registry).await?;
    if superuser || createrole {
        return Ok(());
    }
    Err(TenancyError::Validation(
        "schema tenants log in as their own roles, and the registry login cannot create \
         them: run `ALTER ROLE <registry login> CREATEROLE`, or turn off \
         TenantPoolsConfig::schema_tenant_roles"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_differ_by_database_and_never_show_the_password() {
        let secret = SessionSecret::from_bytes(vec![7; 32]);
        let a = TenantRole::derive(&secret, "app", "acme");
        assert_eq!(a.name, TenantRole::derive(&secret, "app", "acme").name);
        assert_ne!(a.name, role_name("other", "acme"));
        assert_ne!(a.name, "acme");
        assert!(a.name.len() <= 63);
        assert!(!format!("{a:?}").contains(a.password.expose()));
        let other = TenantRole::derive(&SessionSecret::from_bytes(vec![8; 32]), "app", "acme");
        assert_eq!(a.name, other.name);
        assert_ne!(a.password, other.password);
    }

    #[derive(Debug)]
    struct Refused(&'static str);
    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("refused")
        }
    }
    impl std::error::Error for Refused {}
    impl sqlx::error::DatabaseError for Refused {
        fn message(&self) -> &str {
            "refused"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.0.into())
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    /// Only a wrong password is one a new password fixes; `pg_hba.conf` is not.
    #[test]
    fn only_28p01_is_a_bad_password() {
        let err = |code| sqlx::Error::Database(Box::new(Refused(code)));
        assert!(is_bad_password(&err("28P01")));
        assert!(!is_bad_password(&err("28000")));
        let role = TenantRole::derive(&SessionSecret::from_bytes(vec![7; 32]), "app", "acme");
        assert!(login_error(&role, err("28000"))
            .to_string()
            .contains("pg_hba.conf"));
    }
}
