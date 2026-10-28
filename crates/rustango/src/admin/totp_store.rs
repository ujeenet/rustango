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
#[derive(Model, Clone)]
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
    /// A re-enrollment's new secret. The confirmed one above keeps
    /// gating login until a code for this one promotes it (#1756).
    #[rustango(max_length = 64)]
    pub pending_secret_base32: Option<String>,
}

/// The secrets are redacted (#1875).
impl std::fmt::Debug for AdminTotp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminTotp")
            .field("user_id", &self.user_id)
            .field("secret_base32", &"<redacted>")
            .field("confirmed", &self.confirmed)
            .field("created_at", &self.created_at)
            .field("last_used_step", &self.last_used_step)
            .field(
                "pending_secret_base32",
                &self.pending_secret_base32.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl AdminTotp {
    /// The secret an enrollment in progress would confirm: the pending
    /// re-enroll secret, else the device's own secret while unconfirmed.
    pub fn pending_secret(&self) -> Option<TotpSecret> {
        match &self.pending_secret_base32 {
            Some(s) => TotpSecret::from_base32(s),
            None if !self.confirmed => TotpSecret::from_base32(&self.secret_base32),
            None => None,
        }
    }
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
    crate::migrate::apply_idempotent(pool, &bootstrap_snapshot()).await?;
    add_new_columns(pool).await
}

/// The table as [`ensure_table`] creates it.
pub(crate) fn bootstrap_snapshot() -> crate::migrate::SchemaSnapshot {
    use crate::core::Model as _;
    crate::migrate::SchemaSnapshot::from_models_forced(&[AdminTotp::SCHEMA])
}

/// Columns newer than the table, added to one created by an older
/// release. Never creates the table: an empty new one would read as
/// "no second factor" (#1644).
async fn add_new_columns(pool: &Pool) -> Result<(), sqlx::Error> {
    use crate::core::Model as _;
    crate::migrate::add_columns_idempotent(
        pool,
        &bootstrap_snapshot(),
        &[
            (AdminTotp::SCHEMA.table, "last_used_step"),
            (AdminTotp::SCHEMA.table, "pending_secret_base32"),
        ],
    )
    .await
}

/// Accept `code` for `user_id` at most once. Its time step must be later
/// than the last accepted one, checked and recorded in one UPDATE.
/// Success also drops an unfinished re-enroll, so a stale pending key
/// does not outlive the next sign-in (#1756).
///
/// # Errors
/// Driver or SQL failures.
pub async fn redeem_code(
    pool: &Pool,
    user_id: i64,
    secret: &TotpSecret,
    code: &str,
) -> Result<bool, crate::sql::ExecError> {
    redeem(pool, user_id, secret, code, false).await
}

/// Finish enrollment with `code` for the [`AdminTotp::pending_secret`]:
/// confirms or promotes it and records the code's step in one UPDATE,
/// so a failed write leaves the code unused.
///
/// # Errors
/// Driver or SQL failures.
pub async fn confirm_with_code(
    pool: &Pool,
    user_id: i64,
    secret: &TotpSecret,
    code: &str,
) -> Result<bool, crate::sql::ExecError> {
    redeem(pool, user_id, secret, code, true).await
}

async fn redeem(
    pool: &Pool,
    user_id: i64,
    secret: &TotpSecret,
    code: &str,
    confirm: bool,
) -> Result<bool, crate::sql::ExecError> {
    redeem_in::<AdminTotp>(pool, user_id, &Stored::plain(secret), code, confirm).await
}

/// A TOTP device table: one row per account, keyed by its id. The admin's
/// stores base32; the operator console's seals the secret.
pub(crate) trait DeviceTable: crate::core::Model + Send + 'static {
    /// The account-id column, the primary key.
    const OWNER: &'static str;
    /// The confirmed (or, unconfirmed, the enrolling) secret.
    const SECRET: &'static str;
    /// A re-enrollment's new secret, beside a confirmed one.
    const PENDING: &'static str;
    /// The stored text for `secret`.
    fn seal(owner: i64, secret: &TotpSecret) -> Result<String, crate::sql::ExecError>;
    /// `None` when `stored` does not decode for `owner`.
    fn open(owner: i64, stored: &str) -> Option<TotpSecret>;
    /// Bring an older table up to date, after a write failed on it.
    fn upgrade(pool: &Pool) -> impl std::future::Future<Output = Result<(), sqlx::Error>> + Send;
}

impl DeviceTable for AdminTotp {
    const OWNER: &'static str = "user_id";
    const SECRET: &'static str = "secret_base32";
    const PENDING: &'static str = "pending_secret_base32";
    fn seal(_: i64, secret: &TotpSecret) -> Result<String, crate::sql::ExecError> {
        Ok(secret.to_base32())
    }
    fn open(_: i64, stored: &str) -> Option<TotpSecret> {
        TotpSecret::from_base32(stored)
    }
    fn upgrade(pool: &Pool) -> impl std::future::Future<Output = Result<(), sqlx::Error>> + Send {
        add_new_columns(pool)
    }
}

/// A secret as read from its row. Writes pin to the stored text, since a
/// sealed value never seals to the same text twice.
pub(crate) struct Stored {
    pub(crate) secret: TotpSecret,
    text: String,
}

impl Stored {
    /// A base32 secret, whose stored text is its encoding.
    fn plain(secret: &TotpSecret) -> Self {
        Self {
            secret: secret.clone(),
            text: secret.to_base32(),
        }
    }

    fn read<M: DeviceTable>(owner: i64, text: String) -> Option<Self> {
        M::open(owner, &text).map(|secret| Self { secret, text })
    }
}

/// [`redeem_code`] (`confirm = false`) or [`confirm_with_code`] on any device table.
pub(crate) async fn redeem_in<M: DeviceTable>(
    pool: &Pool,
    owner: i64,
    secret: &Stored,
    code: &str,
    confirm: bool,
) -> Result<bool, crate::sql::ExecError> {
    let Some(step) = crate::totp::matched_step(&secret.secret, code, STEP_SECS, DIGITS, WINDOW)
    else {
        return Ok(false);
    };
    let step = i64::try_from(step).unwrap_or(i64::MAX);
    let updated = match write_step::<M>(pool, owner, secret, step, confirm).await {
        Ok(n) => n,
        // A table from before the newer columns: add them and try again.
        Err(first) => match M::upgrade(pool).await {
            Ok(()) => write_step::<M>(pool, owner, secret, step, confirm).await?,
            Err(_) => return Err(first),
        },
    };
    Ok(updated == 1)
}

async fn write_step<M: DeviceTable>(
    pool: &Pool,
    owner: i64,
    secret: &Stored,
    step: i64,
    confirm: bool,
) -> Result<u64, crate::sql::ExecError> {
    use crate::query::{QuerySet, Q};
    use crate::sql::UpdaterPool as _;
    if confirm {
        // A re-enroll: swap the pending secret in, in one write.
        let promoted = QuerySet::<M>::new()
            .filter(M::OWNER, owner)
            .filter(M::PENDING, secret.text.clone())
            .update()
            .set(M::SECRET, secret.text.clone())
            .set(M::PENDING, None::<String>)
            .set("confirmed", true)
            .set("last_used_step", step)
            .execute_pool(pool)
            .await?;
        if promoted == 1 {
            return Ok(promoted);
        }
    }
    // Pinned to the secret the code was checked against, so a
    // re-enrollment in between cannot be confirmed by the old one.
    let q = QuerySet::<M>::new()
        .filter(M::OWNER, owner)
        .filter(M::SECRET, secret.text.clone());
    let q = if confirm {
        q.filter("confirmed", false)
    } else {
        q
    };
    let q = q
        .where_(Q::is_null("last_used_step") | Q::lt("last_used_step", step))
        .update()
        .set("last_used_step", step);
    let q = if confirm {
        q.set("confirmed", true)
    } else {
        q.set(M::PENDING, None::<String>)
    };
    q.execute_pool(pool).await
}

/// The confirmed secret of `owner`, `None` without one. A read failure, or a
/// confirmed row that will not decode, is an error: it still gates.
pub(crate) async fn confirmed_in<M: DeviceTable>(
    pool: &Pool,
    owner: i64,
) -> Result<Option<Stored>, crate::sql::ExecError> {
    // Only the secret, so a table from before `last_used_step` still reads.
    let secrets: Vec<String> = crate::query::QuerySet::<M>::new()
        .filter(M::OWNER, owner)
        .filter("confirmed", true)
        .values_list_flat(M::SECRET)
        .fetch(pool)
        .await?;
    secrets
        .into_iter()
        .next()
        .map(|text| {
            Stored::read::<M>(owner, text).ok_or_else(|| {
                crate::sql::ExecError::Driver(crate::sql::sqlx::Error::Decode(
                    format!("{}.{} does not decode", M::SCHEMA.table, M::SECRET).into(),
                ))
            })
        })
        .transpose()
}

/// The secret an enrollment in progress would confirm: a re-enroll's
/// pending one, else an unconfirmed device's own.
#[cfg(feature = "tenancy")]
pub(crate) async fn pending_in<M: DeviceTable>(
    pool: &Pool,
    owner: i64,
) -> Result<Option<Stored>, crate::sql::ExecError> {
    use crate::query::{QuerySet, Q};
    let staged: Vec<String> = QuerySet::<M>::new()
        .filter(M::OWNER, owner)
        .filter("confirmed", true)
        .where_(Q::is_not_null(M::PENDING))
        .values_list_flat(M::PENDING)
        .fetch(pool)
        .await?;
    let text = match staged.into_iter().next() {
        Some(t) => Some(t),
        None => QuerySet::<M>::new()
            .filter(M::OWNER, owner)
            .filter("confirmed", false)
            .values_list_flat(M::SECRET)
            .fetch(pool)
            .await?
            .into_iter()
            .next(),
    };
    Ok(text.and_then(|t| Stored::read::<M>(owner, t)))
}

/// [`start_enrollment`] on any device table.
pub(crate) async fn start_in<M: DeviceTable>(
    pool: &Pool,
    owner: i64,
    secret: &TotpSecret,
) -> Result<(), crate::sql::ExecError> {
    use crate::core::SqlValue;
    use crate::query::QuerySet;
    use crate::sql::UpdaterPool as _;
    let stored = M::seal(owner, secret)?;
    let staged = QuerySet::<M>::new()
        .filter(M::OWNER, owner)
        .filter("confirmed", true)
        .update()
        .set(M::PENDING, stored.clone())
        .execute_pool(pool)
        .await?;
    if staged == 1 {
        return Ok(());
    }
    // No confirmed device: replace a pending one. Never deletes a
    // confirmed row, so a race cannot drop the factor either.
    let del = QuerySet::<M>::new()
        .filter(M::OWNER, owner)
        .filter("confirmed", false)
        .compile_delete()?;
    crate::sql::delete_pool(pool, &del).await?;
    let insert = crate::core::InsertQuery::new(
        M::SCHEMA,
        vec![M::OWNER, M::SECRET, "confirmed", "created_at"],
        vec![
            SqlValue::I64(owner),
            SqlValue::String(stored),
            SqlValue::Bool(false),
            SqlValue::DateTime(chrono::Utc::now()),
        ],
    );
    crate::sql::insert_pool(pool, &insert).await
}

/// Drop the confirmed device whose secret is `secret`; `false` when it changed meanwhile.
#[cfg(feature = "tenancy")]
pub(crate) async fn remove_in<M: DeviceTable>(
    pool: &Pool,
    owner: i64,
    secret: &Stored,
) -> Result<bool, crate::sql::ExecError> {
    let del = crate::query::QuerySet::<M>::new()
        .filter(M::OWNER, owner)
        .filter(M::SECRET, secret.text.clone())
        .filter("confirmed", true)
        .compile_delete()?;
    Ok(crate::sql::delete_pool(pool, &del).await? == 1)
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
#[deprecated(
    since = "0.59.17",
    note = "a read error looks like no device; use confirmed_secret_checked"
)]
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
    Ok(confirmed_in::<AdminTotp>(pool, user_id)
        .await?
        .map(|s| s.secret))
}

/// What [`start_reenrollment`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reenroll {
    /// The current code was valid; the new secret is staged.
    Verified,
    /// The current code was refused; nothing changed.
    Refused,
    /// No confirmed device, so no code was checked; the secret is staged.
    NoDevice,
}

/// Re-enroll with `secret`, but only for a fresh `current_code` of the
/// confirmed device, so a stolen session cannot swap the factor (#1776).
///
/// # Errors
/// Driver or SQL failures.
pub async fn start_reenrollment(
    pool: &Pool,
    user_id: i64,
    current_code: &str,
    secret: &TotpSecret,
) -> Result<Reenroll, crate::sql::ExecError> {
    let outcome = match confirmed_secret_checked(pool, user_id).await? {
        Some(current) if !redeem_code(pool, user_id, &current, current_code).await? => {
            return Ok(Reenroll::Refused);
        }
        Some(_) => Reenroll::Verified,
        None => Reenroll::NoDevice,
    };
    start_enrollment(pool, user_id, secret).await?;
    Ok(outcome)
}

/// Start or restart enrollment with `secret`. A confirmed device keeps
/// its secret and gates login; `secret` waits beside it until
/// [`confirm_with_code`] promotes it (#1756).
///
/// # Errors
/// Driver or SQL failures.
pub async fn start_enrollment(
    pool: &Pool,
    user_id: i64,
    secret: &TotpSecret,
) -> Result<(), crate::sql::ExecError> {
    start_in::<AdminTotp>(pool, user_id, secret).await
}

/// Confirm the user's enrollment without a code, promoting a pending
/// re-enroll secret if there is one. Prefer [`confirm_with_code`].
///
/// # Errors
/// Driver or SQL failures.
pub async fn confirm(pool: &Pool, user_id: i64) -> Result<(), crate::sql::ExecError> {
    use crate::sql::UpdaterPool as _;
    let q = AdminTotp::objects().filter("user_id", user_id);
    let pending = device(pool, user_id)
        .await
        .and_then(|d| d.pending_secret_base32);
    // Pinned to the pending value read, so a newer re-enroll is not promoted.
    let q = match pending {
        Some(p) => q
            .filter("pending_secret_base32", p.clone())
            .update()
            .set("secret_base32", p)
            .set("pending_secret_base32", None::<String>),
        None => q.update(),
    };
    q.set("confirmed", true).execute_pool(pool).await?;
    Ok(())
}
