//! Passkey / WebAuthn support — issue #392.
//!
//! This is the **foundation slice**: credential storage. It defines the
//! `rustango_webauthn_credentials` table + a small store API that the
//! registration / authentication ceremonies (the next slice) build on.
//!
//! ## Design
//!
//! - **All-RustCrypto, no C dep.** The ceremony layer (CBOR attestation
//!   parsing + ES256 assertion verification) will use pure-Rust
//!   `ciborium` + `p256`, *not* `webauthn-rs` (which pulls `openssl`).
//!   This keeps rustango's dependency story C-free, matching `argon2` /
//!   `sha2` / `hmac`.
//! - **Operator-managed table** (`#[rustango(managed = false)]`),
//!   bootstrapped idempotently via [`ensure_table`] (the `audit` /
//!   `totp_store` pattern), so it never enters the migration graph.
//! - A user may register **several** passkeys (laptop, phone, security
//!   key), so the PK is a surrogate `id`; lookups are by `user_id` (list)
//!   or `credential_id` (the unique handle the authenticator returns at
//!   assertion time).
//!
//! Gated behind the `passkey` feature; builds without it are unchanged.

pub mod ceremony;
pub mod error;
pub mod session;
pub mod verify;

pub use ceremony::{
    authentication_options_json, generate_challenge, registration_options_json,
    verify_authentication, verify_registration, RegistrationOutcome,
};
pub use error::PasskeyError;
pub use session::{open_challenge, seal_challenge};

use crate::sql::{Auto, Pool};
use crate::Model;

/// One registered WebAuthn credential (passkey). `public_key` holds the
/// COSE-encoded public key bytes captured at registration; `sign_count`
/// is the authenticator's signature counter, bumped on each assertion to
/// detect cloned authenticators.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_webauthn_credentials", managed = false)]
#[allow(dead_code)]
pub struct WebauthnCredential {
    /// Surrogate primary key (a user may have many credentials).
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// The owning user's id.
    pub user_id: i64,
    /// Base64url (no padding) of the raw credential id the authenticator
    /// returns — the unique handle used to look the credential up at
    /// assertion time.
    #[rustango(max_length = 255)]
    pub credential_id: String,
    /// COSE-encoded public key bytes captured at registration.
    pub public_key: Vec<u8>,
    /// Authenticator signature counter (replay / clone detection).
    pub sign_count: i64,
    /// Optional user-facing label ("MacBook Touch ID", "YubiKey 5").
    #[rustango(max_length = 64, default = "")]
    pub label: String,
    /// When the credential was registered.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const CREATE_TABLE_PG: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_webauthn_credentials" (
    "id"            BIGSERIAL PRIMARY KEY,
    "user_id"       BIGINT NOT NULL,
    "credential_id" VARCHAR(255) NOT NULL,
    "public_key"    BYTEA NOT NULL,
    "sign_count"    BIGINT NOT NULL DEFAULT 0,
    "label"         VARCHAR(64) NOT NULL DEFAULT '',
    "created_at"    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT "rustango_webauthn_credentials_cred_uq" UNIQUE ("credential_id")
);
CREATE INDEX IF NOT EXISTS "rustango_webauthn_credentials_user_idx"
    ON "rustango_webauthn_credentials" ("user_id");
"#;

const CREATE_TABLE_MYSQL: &str = r"
CREATE TABLE IF NOT EXISTS `rustango_webauthn_credentials` (
    `id`            BIGINT AUTO_INCREMENT PRIMARY KEY,
    `user_id`       BIGINT NOT NULL,
    `credential_id` VARCHAR(255) NOT NULL,
    `public_key`    LONGBLOB NOT NULL,
    `sign_count`    BIGINT NOT NULL DEFAULT 0,
    `label`         VARCHAR(64) NOT NULL DEFAULT '',
    `created_at`    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    CONSTRAINT `rustango_webauthn_credentials_cred_uq` UNIQUE (`credential_id`),
    INDEX `rustango_webauthn_credentials_user_idx` (`user_id`)
);
";

const CREATE_TABLE_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_webauthn_credentials" (
    "id"            INTEGER PRIMARY KEY AUTOINCREMENT,
    "user_id"       INTEGER NOT NULL,
    "credential_id" TEXT NOT NULL,
    "public_key"    BLOB NOT NULL,
    "sign_count"    INTEGER NOT NULL DEFAULT 0,
    "label"         TEXT NOT NULL DEFAULT '',
    "created_at"    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "rustango_webauthn_credentials_cred_uq" UNIQUE ("credential_id")
);
CREATE INDEX IF NOT EXISTS "rustango_webauthn_credentials_user_idx"
    ON "rustango_webauthn_credentials" ("user_id");
"#;

/// Idempotently create the `rustango_webauthn_credentials` table for the
/// active backend.
///
/// # Errors
/// Driver / SQL failures from `CREATE TABLE IF NOT EXISTS` (other than
/// the duplicate-object errors `run_ddl_idempotent` swallows).
pub async fn ensure_table(pool: &Pool) -> Result<(), sqlx::Error> {
    let ddl = match pool.dialect().name() {
        "mysql" => CREATE_TABLE_MYSQL,
        "sqlite" => CREATE_TABLE_SQLITE,
        _ => CREATE_TABLE_PG,
    };
    crate::sql::run_ddl_idempotent(pool, ddl).await
}

/// Every credential registered to `user_id` — the allow-list the
/// authentication ceremony offers the authenticator.
///
/// # Errors
/// As the ORM fetch path ([`crate::sql::ExecError`]).
pub async fn for_user(
    pool: &Pool,
    user_id: i64,
) -> Result<Vec<WebauthnCredential>, crate::sql::ExecError> {
    use crate::sql::FetcherPool as _;
    WebauthnCredential::objects()
        .filter("user_id", user_id)
        .fetch(pool)
        .await
}

/// Look a credential up by its (unique) base64url credential id — the
/// handle the authenticator returns at assertion time.
///
/// # Errors
/// As the ORM fetch path ([`crate::sql::ExecError`]).
pub async fn by_credential_id(
    pool: &Pool,
    credential_id: &str,
) -> Result<Option<WebauthnCredential>, crate::sql::ExecError> {
    use crate::sql::FetcherPool as _;
    Ok(WebauthnCredential::objects()
        .filter("credential_id", credential_id.to_owned())
        .fetch(pool)
        .await?
        .into_iter()
        .next())
}

/// Persist a freshly-registered credential. `public_key` is the
/// COSE-encoded key bytes; `sign_count` is the authenticator's initial
/// counter (often 0).
///
/// # Errors
/// As the ORM insert path ([`crate::sql::ExecError`]).
pub async fn register(
    pool: &Pool,
    user_id: i64,
    credential_id: &str,
    public_key: Vec<u8>,
    sign_count: i64,
    label: &str,
) -> Result<(), crate::sql::ExecError> {
    let mut row = WebauthnCredential {
        id: Auto::Unset,
        user_id,
        credential_id: credential_id.to_owned(),
        public_key,
        sign_count,
        label: label.to_owned(),
        created_at: chrono::Utc::now(),
    };
    row.insert_pool(pool).await?;
    Ok(())
}

/// Advance the stored signature counter for a credential after a
/// successful assertion (clone detection: the new count must exceed the
/// stored one). No-op if the credential id is unknown.
///
/// # Errors
/// As the ORM update path ([`crate::sql::ExecError`]).
pub async fn update_sign_count(
    pool: &Pool,
    credential_id: &str,
    new_count: i64,
) -> Result<(), crate::sql::ExecError> {
    use crate::sql::UpdaterPool as _;
    WebauthnCredential::objects()
        .filter("credential_id", credential_id.to_owned())
        .update()
        .set("sign_count", new_count)
        .execute_pool(pool)
        .await?;
    Ok(())
}
