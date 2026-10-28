//! Operator TOTP devices, in the registry.
//!
//! The same store as the admin's ([`crate::admin::totp_store`]), on its own
//! table: operators and admin users are separate identities whose ids
//! overlap. The secret is sealed under `RUSTANGO_SECRET_KEY`, bound to the
//! operator id. `managed = false`: `migrate` (or the console's first
//! sign-in) creates it, so it never joins the migration graph.

use crate::admin::totp_store::{self as store, DeviceTable, Stored};
use crate::sql::{ExecError, Pool};
use crate::totp::TotpSecret;
use crate::Model;

/// One operator's TOTP device. `confirmed = false` is an enrollment not
/// yet finished; only a confirmed device gates sign-in.
#[derive(Model, Clone)]
#[rustango(table = "rustango_operator_totp", managed = false, scope = "registry")]
#[allow(dead_code)]
pub struct OperatorTotp {
    #[rustango(primary_key)]
    pub operator_id: i64,
    /// Sealed (`enc:v1:`); private so nothing stores it plain.
    #[rustango(max_length = 255)]
    secret: String,
    #[rustango(default = "false")]
    pub confirmed: bool,
    #[rustango(default = "now()")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Time step of the last accepted code; each code works once.
    pub last_used_step: Option<i64>,
    /// A re-enrollment's sealed new secret.
    #[rustango(max_length = 255)]
    pending_secret: Option<String>,
}

impl std::fmt::Debug for OperatorTotp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorTotp")
            .field("operator_id", &self.operator_id)
            .field("confirmed", &self.confirmed)
            .field("created_at", &self.created_at)
            .field("last_used_step", &self.last_used_step)
            .finish_non_exhaustive()
    }
}

const SEALED_V1: &str = "enc:v1:";

/// Binds a sealed secret to its operator, so a row copied to another
/// operator does not open.
fn aad(operator_id: i64) -> Vec<u8> {
    format!("rustango:operator-totp:v1:{operator_id}").into_bytes()
}

impl DeviceTable for OperatorTotp {
    const OWNER: &'static str = "operator_id";
    const SECRET: &'static str = "secret";
    const PENDING: &'static str = "pending_secret";

    fn seal(owner: i64, secret: &TotpSecret) -> Result<String, ExecError> {
        crate::aead::encrypt(&secret.0, &aad(owner))
            .map(|b64| format!("{SEALED_V1}{b64}"))
            .map_err(|_| {
                ExecError::Driver(sqlx::Error::Configuration(
                    "RUSTANGO_SECRET_KEY is not set; cannot seal a TOTP secret".into(),
                ))
            })
    }

    fn open(owner: i64, stored: &str) -> Option<TotpSecret> {
        let b64 = stored.strip_prefix(SEALED_V1)?;
        let bytes = crate::aead::decrypt(b64, &aad(owner)).ok()?;
        (bytes.len() >= crate::totp::MIN_SECRET_BYTES).then_some(TotpSecret(bytes))
    }

    fn upgrade(pool: &Pool) -> impl std::future::Future<Output = Result<(), sqlx::Error>> + Send {
        ensure_table(pool)
    }
}

/// `stored` opened (the previous key too) and sealed again under the
/// current key, for `reseal-tenant-secrets`.
pub(crate) fn reseal(operator_id: i64, stored: &str) -> Result<String, String> {
    let b64 = stored.strip_prefix(SEALED_V1).ok_or("not a sealed value")?;
    let plain = crate::aead::decrypt(b64, &aad(operator_id)).map_err(|e| format!("{e:?}"))?;
    crate::aead::encrypt(&plain, &aad(operator_id))
        .map(|b64| format!("{SEALED_V1}{b64}"))
        .map_err(|e| format!("cannot seal: {e:?}"))
}

/// Create `rustango_operator_totp` for the registry's backend. Idempotent.
///
/// # Errors
/// Driver or SQL failures.
pub async fn ensure_table(pool: &Pool) -> Result<(), sqlx::Error> {
    crate::migrate::apply_idempotent(pool, &bootstrap_snapshot()).await
}

/// The table as [`ensure_table`] creates it.
pub(crate) fn bootstrap_snapshot() -> crate::migrate::SchemaSnapshot {
    use crate::core::Model as _;
    crate::migrate::SchemaSnapshot::from_models_forced(&[OperatorTotp::SCHEMA])
}

/// The operator's confirmed secret. A read failure is an error, never
/// "no second factor".
pub(crate) async fn confirmed(pool: &Pool, operator_id: i64) -> Result<Option<Stored>, ExecError> {
    store::confirmed_in::<OperatorTotp>(pool, operator_id).await
}

/// The secret an enrollment in progress would confirm.
pub(crate) async fn pending(pool: &Pool, operator_id: i64) -> Result<Option<Stored>, ExecError> {
    store::pending_in::<OperatorTotp>(pool, operator_id).await
}

/// Start (or restart) an enrollment; a confirmed device keeps gating
/// until this one is confirmed.
pub(crate) async fn start(
    pool: &Pool,
    operator_id: i64,
    secret: &TotpSecret,
) -> Result<(), ExecError> {
    store::start_in::<OperatorTotp>(pool, operator_id, secret).await
}

/// Accept `code` once against `secret`.
pub(crate) async fn redeem(
    pool: &Pool,
    operator_id: i64,
    secret: &Stored,
    code: &str,
) -> Result<bool, ExecError> {
    store::redeem_in::<OperatorTotp>(pool, operator_id, secret, code, false).await
}

/// Finish the enrollment of `secret` with `code`.
pub(crate) async fn confirm(
    pool: &Pool,
    operator_id: i64,
    secret: &Stored,
    code: &str,
) -> Result<bool, ExecError> {
    store::redeem_in::<OperatorTotp>(pool, operator_id, secret, code, true).await
}

/// Drop the device whose confirmed secret is `secret`.
pub(crate) async fn remove(
    pool: &Pool,
    operator_id: i64,
    secret: &Stored,
) -> Result<bool, ExecError> {
    store::remove_in::<OperatorTotp>(pool, operator_id, secret).await
}
