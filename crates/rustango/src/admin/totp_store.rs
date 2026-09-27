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
    /// Time step of the last accepted code. A code is accepted only for
    /// a later step, so each code works once.
    pub last_used_step: Option<i64>,
}

/// Authenticator-app defaults: 30s step, 6 digits, ±1 step of clock skew.
const STEP_SECS: u64 = 30;
const DIGITS: u32 = 6;
const WINDOW: i64 = 1;

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
    crate::migrate::apply_idempotent(pool, &snapshot).await?;
    add_new_columns(pool).await
}

/// Columns newer than the table, added to one created by an older
/// release. Never creates the table: an empty new one would read as
/// "no second factor" (#1644).
async fn add_new_columns(pool: &Pool) -> Result<(), sqlx::Error> {
    use crate::core::Model as _;
    let snapshot = crate::migrate::SchemaSnapshot::from_models_forced(&[AdminTotp::SCHEMA]);
    crate::migrate::add_columns_idempotent(
        pool,
        &snapshot,
        &[(AdminTotp::SCHEMA.table, "last_used_step")],
    )
    .await
}

/// Accept `code` for `user_id` at most once. Its time step must be later
/// than the last accepted one, checked and recorded in one UPDATE.
///
/// # Errors
/// Driver or SQL failures.
pub async fn redeem_code(
    pool: &Pool,
    user_id: i64,
    secret: &TotpSecret,
    code: &str,
) -> Result<bool, crate::sql::ExecError> {
    use crate::query::Q;
    use crate::sql::UpdaterPool as _;
    let Some(step) = crate::totp::matched_step(secret, code, STEP_SECS, DIGITS, WINDOW) else {
        return Ok(false);
    };
    let step = i64::try_from(step).unwrap_or(i64::MAX);
    let update = || {
        AdminTotp::objects()
            .filter("user_id", user_id)
            .where_(Q::is_null("last_used_step") | Q::lt("last_used_step", step))
            .update()
            .set("last_used_step", step)
            .execute_pool(pool)
    };
    let updated = match update().await {
        Ok(n) => n,
        // A table from before `last_used_step`: add it and try again.
        Err(first) => match add_new_columns(pool).await {
            Ok(()) => update().await?,
            Err(_) => return Err(first),
        },
    };
    Ok(updated == 1)
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
    // Only the secret, so a table from before `last_used_step` still reads.
    let secrets: Vec<String> = AdminTotp::objects()
        .filter("user_id", user_id)
        .filter("confirmed", true)
        .values_list_flat("secret_base32")
        .fetch(pool)
        .await?;
    Ok(secrets.first().and_then(|s| TotpSecret::from_base32(s)))
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
        last_used_step: None,
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
