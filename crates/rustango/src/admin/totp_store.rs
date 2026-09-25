//! Storage for admin TOTP (two-factor) devices.
//!
//! The RFC 6238 crypto lives in [`crate::totp`]. This module is the
//! storage the enrollment page and the login challenge share: one
//! device per admin user, keyed by `user_id`.
//!
//! The table is `managed = false` and created by [`ensure_table`], the
//! same pattern as the audit log, so it never joins the migration
//! graph. Without the `totp` feature there is no 2FA challenge.
//!
//! [`ensure_table`]: crate::admin::totp_store::ensure_table

use crate::sql::Pool;
use crate::totp::TotpSecret;
use crate::Model;

/// One admin TOTP device. `confirmed = false` means enrollment is
/// half done: the secret exists but no code has been verified yet.
/// Only a confirmed device gates login.
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
    #[rustango(default = "now()")]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Create the `rustango_admin_totp` table for the active backend. Safe
/// to call repeatedly.
///
/// The DDL is rendered from `AdminTotp::SCHEMA` by the migration
/// engine's dialect emitter, the same path `migrate` uses, so it
/// cannot drift. The model is `managed = false`, so this is its only
/// DDL path.
///
/// # Errors
/// Driver or SQL failures. Duplicate-object errors are ignored, so
/// repeat calls succeed.
pub async fn ensure_table(pool: &Pool) -> Result<(), sqlx::Error> {
    use crate::core::Model as _;
    let snapshot = crate::migrate::SchemaSnapshot::from_models_forced(&[AdminTotp::SCHEMA]);
    let changes =
        crate::migrate::detect_changes(&crate::migrate::SchemaSnapshot::default(), &snapshot);
    let batch =
        crate::migrate::render_changes_split_with_dialect(&changes, &snapshot, pool.dialect())
            .map_err(sqlx::Error::Protocol)?;
    crate::migrate::apply_idempotent(pool, &batch).await?;
    Ok(())
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

/// The user's **confirmed** TOTP secret, decoded. `None` when there is
/// no device, or when the device is still pending.
///
/// A read failure also gives `None`, which is why an authentication
/// gate must not use this — see [`confirmed_secret_checked`] (#1644).
pub async fn confirmed_secret(pool: &Pool, user_id: i64) -> Option<TotpSecret> {
    confirmed_secret_checked(pool, user_id).await.ok().flatten()
}

/// [`confirmed_secret`], but a read failure is an error rather than
/// "this user has no second factor".
///
/// The distinction is the whole point. `device()` maps any error to
/// `None`, the login gate read `None` as "no 2FA", and the password
/// alone then granted the session — so a dropped table or a pool
/// timeout silently downgraded every enrolled admin to one factor
/// (#1644). `rustango_admin_totp` is `managed = false`, so "the table
/// is not there" is a reachable state, not a hypothetical.
///
/// # Errors
/// The driver error, when the device row cannot be read at all.
pub async fn confirmed_secret_checked(
    pool: &Pool,
    user_id: i64,
) -> Result<Option<TotpSecret>, crate::sql::ExecError> {
    use crate::sql::FetcherPool as _;
    let rows = AdminTotp::objects()
        .filter("user_id", user_id)
        .fetch(pool)
        .await?;
    let Some(d) = rows.into_iter().next() else {
        return Ok(None);
    };
    if !d.confirmed {
        return Ok(None);
    }
    Ok(TotpSecret::from_base32(&d.secret_base32))
}

/// Start or restart enrollment: store a fresh unconfirmed secret for
/// `user_id`, replacing any device that is already there.
///
/// # Errors
/// Driver or SQL failures.
pub async fn start_enrollment(
    pool: &Pool,
    user_id: i64,
    secret: &TotpSecret,
) -> Result<(), crate::sql::ExecError> {
    // One device per user, so drop any earlier row first.
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

/// Mark the user's device confirmed, once a code has verified during
/// enrollment. Does nothing when there is no pending device.
///
/// # Errors
/// Driver or SQL failures.
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
