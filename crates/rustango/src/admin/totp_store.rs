//! Persistence for admin TOTP (two-factor) devices — issue #367.
//!
//! The RFC 6238 crypto lives in [`crate::totp`]; this module is the
//! admin-side storage + lookup that the enrollment page and the login
//! challenge share. One device per admin user, keyed by `user_id`.
//!
//! The table is **operator-managed** (`#[rustango(managed = false)]`) and
//! bootstrapped idempotently via [`ensure_table`] (the same pattern as
//! the audit log), so it never enters the migration graph. Gated behind
//! the `totp` feature; admin builds without `totp` are unchanged (no
//! 2FA challenge).

use crate::sql::Pool;
use crate::totp::TotpSecret;
use crate::Model;

/// One admin TOTP device. `confirmed = false` is a half-finished
/// enrollment (secret generated + QR shown, code not yet verified);
/// only a `confirmed` device gates login.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_admin_totp", managed = false)]
#[allow(dead_code)]
pub struct AdminTotp {
    /// Admin user id this device belongs to (one per user).
    #[rustango(primary_key)]
    pub user_id: i64,
    /// Base32-encoded shared secret (RFC 4648, no padding).
    #[rustango(max_length = 64)]
    pub secret_base32: String,
    /// `true` once the user has verified a code against the secret.
    #[rustango(default = "false")]
    pub confirmed: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const CREATE_TABLE_PG: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_admin_totp" (
    "user_id"       BIGINT PRIMARY KEY,
    "secret_base32" VARCHAR(64) NOT NULL,
    "confirmed"     BOOLEAN NOT NULL DEFAULT false,
    "created_at"    TIMESTAMPTZ NOT NULL DEFAULT now()
);
"#;

const CREATE_TABLE_MYSQL: &str = r#"
CREATE TABLE IF NOT EXISTS `rustango_admin_totp` (
    `user_id`       BIGINT PRIMARY KEY,
    `secret_base32` VARCHAR(64) NOT NULL,
    `confirmed`     TINYINT(1) NOT NULL DEFAULT 0,
    `created_at`    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
);
"#;

const CREATE_TABLE_SQLITE: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_admin_totp" (
    "user_id"       INTEGER PRIMARY KEY,
    "secret_base32" TEXT NOT NULL,
    "confirmed"     INTEGER NOT NULL DEFAULT 0,
    "created_at"    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
"#;

/// Idempotently create the `rustango_admin_totp` table for the active
/// backend.
///
/// # Errors
/// Driver / SQL failures from `CREATE TABLE IF NOT EXISTS`.
pub async fn ensure_table(pool: &Pool) -> Result<(), sqlx::Error> {
    let ddl = match pool.dialect().name() {
        "mysql" => CREATE_TABLE_MYSQL,
        "sqlite" => CREATE_TABLE_SQLITE,
        _ => CREATE_TABLE_PG,
    };
    crate::sql::run_ddl_idempotent(pool, ddl).await
}

/// Fetch the device row for `user_id`, if any.
pub async fn device(pool: &Pool, user_id: i64) -> Option<AdminTotp> {
    use crate::sql::FetcherPool as _;
    AdminTotp::objects()
        .filter("user_id", user_id)
        .fetch(pool)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next())
}

/// The user's **confirmed** TOTP secret, decoded — `None` when the user
/// has no device or an unconfirmed (pending) one. This is the gate the
/// login challenge checks.
pub async fn confirmed_secret(pool: &Pool, user_id: i64) -> Option<TotpSecret> {
    let d = device(pool, user_id).await?;
    if !d.confirmed {
        return None;
    }
    TotpSecret::from_base32(&d.secret_base32)
}

/// Start (or restart) enrollment: store a fresh, unconfirmed secret for
/// `user_id`, replacing any existing device. Returns the stored secret.
///
/// # Errors
/// Driver / SQL failures.
pub async fn start_enrollment(
    pool: &Pool,
    user_id: i64,
    secret: &TotpSecret,
) -> Result<(), crate::sql::ExecError> {
    // One device per user — clear any prior (pending or confirmed) row.
    let del = AdminTotp::objects()
        .filter("user_id", user_id)
        .compile_delete()?;
    crate::sql::delete_pool(pool, &del).await?;
    let row = AdminTotp {
        user_id,
        secret_base32: secret.to_base32(),
        confirmed: false,
        created_at: chrono::Utc::now(),
    };
    row.insert_pool(pool).await?;
    Ok(())
}

/// Mark the user's device confirmed (called once a code verifies during
/// enrollment). No-op if there's no pending device.
///
/// # Errors
/// Driver / SQL failures.
pub async fn confirm(pool: &Pool, user_id: i64) -> Result<(), crate::sql::ExecError> {
    use crate::sql::UpdaterPool as _;
    AdminTotp::objects()
        .filter("user_id", user_id)
        .update()
        .set("confirmed", true)
        .execute_pool(pool)
        .await?;
    Ok(())
}
