//! Schema tenants log in as a role of their own, with rights on their schema
//! only. `search_path` alone kept nothing out: SQL naming another tenant's
//! schema ran as the shared registry login. A role switched to with `SET ROLE`
//! can be switched back from tenant SQL, so each tenant gets a LOGIN role.
//!
//! The role's password is an HMAC of its name under [`ROLE_SECRET_ENV`], so
//! every process derives the same one and nothing is stored.

use crate::session::{SessionSecret, SessionSecretError};
use crate::sql::sqlx::{self, postgres::PgConnectOptions, postgres::PgPool};
use crate::sql::Dialect as _;

use super::error::TenancyError;

/// Env var holding the base64 key (32+ bytes) tenant role passwords derive from.
/// Required on the prod tier; dev keeps one in `./var/.rustango_tenant_role.key`.
pub const ROLE_SECRET_ENV: &str = "RUSTANGO_TENANT_ROLE_SECRET";

const ROLE_KEY_PATH: &str = "./var/.rustango_tenant_role.key";
const PASSWORD_TAG: &[u8] = b"rustango.tenant-role.v1";

/// The secret for tenant role passwords: [`ROLE_SECRET_ENV`], else the dev key file.
pub(crate) fn load_secret() -> Result<SessionSecret, String> {
    const HINT: &str = "generate one with: openssl rand -base64 32";
    match std::env::var(ROLE_SECRET_ENV) {
        Ok(raw) => SessionSecret::from_b64(&raw).map_err(|e| {
            let why = match e {
                SessionSecretError::BadBase64 { cause } => format!("not valid base64 ({cause})"),
                SessionSecretError::TooShort { actual } => {
                    format!("{actual} bytes once decoded; it needs 32")
                }
                _ => "unusable".to_owned(),
            };
            format!("{ROLE_SECRET_ENV} is {why}; {HINT}")
        }),
        Err(_) if crate::env::is_prod_tier(&crate::env::tier_from_env()) => Err(format!(
            "{ROLE_SECRET_ENV} is not set: schema tenants log in as roles whose passwords \
             derive from it ({HINT}), or turn off TenantPoolsConfig::schema_tenant_roles"
        )),
        Err(_) => crate::session::load_or_create_key_file(
            std::path::Path::new(ROLE_KEY_PATH),
            "tenant role secret",
            ROLE_SECRET_ENV,
        )
        .map(SessionSecret::from_bytes)
        .map_err(|e| format!("could not keep the dev tenant role secret at {ROLE_KEY_PATH}: {e}")),
    }
}

/// The role of the tenant whose schema is `schema` in `database`. Role names
/// are global to the server, so the database is part of the name.
pub(crate) fn role_name(database: &str, schema: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{database}\0{schema}").as_bytes());
    format!("rgo_t_{}", &crate::hex::hex_encode(&digest)[..24])
}

/// A tenant's login. `Debug` never shows the password.
#[derive(Clone)]
pub(crate) struct TenantRole {
    pub(crate) name: String,
    password: String,
}

impl std::fmt::Debug for TenantRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantRole")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl TenantRole {
    pub(crate) fn derive(secret: &SessionSecret, database: &str, schema: &str) -> Self {
        use base64::Engine as _;
        let name = role_name(database, schema);
        let mac = crate::session::sign_tagged(secret, PASSWORD_TAG, name.as_bytes());
        let password = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac);
        Self { name, password }
    }

    /// `base` (the registry's options) logging in as this role.
    pub(crate) fn login(&self, base: PgConnectOptions) -> PgConnectOptions {
        base.username(&self.name).password(&self.password)
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

/// `true` for a refused login: a missing role, or a stale password.
pub(crate) fn is_login_refused(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if matches!(d.code().as_deref(), Some("28P01" | "28000")))
}

fn q(ident: &str) -> String {
    crate::sql::Postgres.quote_ident(ident)
}

/// Create or update `role` and grant it its schema: every table, sequence and
/// function there now and later, and the extension schemas on its path.
/// Run by the registry login, which needs `CREATEROLE`.
pub(crate) async fn ensure(
    registry: &PgPool,
    role: &TenantRole,
    schema: &str,
    extension_schemas: &[String],
) -> Result<(), TenancyError> {
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
    // A SCRAM verifier, not the password, so the server never sees or logs it.
    let verifier = scram_verifier(&role.password);
    let login = if exists {
        format!("ALTER ROLE {r} WITH LOGIN PASSWORD '{verifier}'")
    } else {
        format!("CREATE ROLE {r} WITH LOGIN NOINHERIT PASSWORD '{verifier}'")
    };
    run(&mut tx, &login, schema).await?;
    let mut stmts = vec![
        format!("GRANT USAGE ON SCHEMA {s} TO {r}"),
        format!("GRANT ALL ON ALL TABLES IN SCHEMA {s} TO {r}"),
        format!("GRANT ALL ON ALL SEQUENCES IN SCHEMA {s} TO {r}"),
        format!("GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA {s} TO {r}"),
        format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT ALL ON TABLES TO {r}"),
        format!("ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT ALL ON SEQUENCES TO {r}"),
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
    tx.commit().await?;
    Ok(())
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

/// Drop the role, first revoking what it holds on schemas that outlive it.
pub(crate) async fn drop_role(registry: &PgPool, name: &str) -> Result<(), TenancyError> {
    let held: Vec<String> = sqlx::query_scalar(
        "SELECT n.nspname::text FROM pg_catalog.pg_namespace n \
         CROSS JOIN LATERAL aclexplode(n.nspacl) a \
         JOIN pg_catalog.pg_roles r ON r.oid = a.grantee WHERE r.rolname = $1",
    )
    .bind(name)
    .fetch_all(registry)
    .await?;
    for schema in held {
        sqlx::query(&format!(
            "REVOKE ALL ON SCHEMA {} FROM {}",
            q(&schema),
            q(name)
        ))
        .execute(registry)
        .await?;
    }
    sqlx::query(&format!("DROP ROLE IF EXISTS {}", q(name)))
        .execute(registry)
        .await?;
    Ok(())
}

/// Refuse a registry login that cannot create the tenant roles.
pub(crate) async fn check_can_create_roles(registry: &PgPool) -> Result<(), TenancyError> {
    let ok: bool = sqlx::query_scalar(
        "SELECT rolcreaterole OR rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user",
    )
    .fetch_one(registry)
    .await?;
    if ok {
        return Ok(());
    }
    Err(TenancyError::Validation(
        "schema tenants log in as their own roles, and the registry login cannot create \
         them: run `ALTER ROLE <registry login> CREATEROLE`, or turn off \
         TenantPoolsConfig::schema_tenant_roles"
            .into(),
    ))
}

/// A `SCRAM-SHA-256` verifier for `password`, the form `pg_authid` stores.
fn scram_verifier(password: &str) -> String {
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use rand::RngCore as _;
    use sha2::{Digest, Sha256};
    const ITERATIONS: u32 = 4096;
    let mac = |key: &[u8], parts: &[&[u8]]| -> [u8; 32] {
        let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
        for p in parts {
            m.update(p);
        }
        m.finalize().into_bytes().into()
    };
    let mut salt = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    // PBKDF2-HMAC-SHA-256, one block.
    let mut u = mac(password.as_bytes(), &[&salt, &1u32.to_be_bytes()]);
    let mut salted = u;
    for _ in 1..ITERATIONS {
        u = mac(password.as_bytes(), &[&u]);
        salted.iter_mut().zip(u).for_each(|(a, b)| *a ^= b);
    }
    let stored = Sha256::digest(mac(&salted, &[b"Client Key"]));
    let server = mac(&salted, &[b"Server Key"]);
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    format!(
        "SCRAM-SHA-256${ITERATIONS}:{}${}:{}",
        b64(&salt),
        b64(&stored),
        b64(&server)
    )
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
        assert!(!format!("{a:?}").contains(&a.password));
        let other = TenantRole::derive(&SessionSecret::from_bytes(vec![8; 32]), "app", "acme");
        assert_eq!(a.name, other.name);
        assert_ne!(a.password, other.password);
    }

    #[test]
    fn verifier_has_the_stored_shape() {
        let v = scram_verifier("pw");
        assert!(v.starts_with("SCRAM-SHA-256$4096:"), "{v}");
        assert!(!v.contains('\''));
        assert_ne!(v, scram_verifier("pw"), "salt is random");
    }
}
