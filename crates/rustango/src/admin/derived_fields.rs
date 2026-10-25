//! Columns the admin computes on every create and update, from the other
//! submitted values (and, on update, the stored row). Registered per table.

use std::future::Future;
use std::pin::Pin;

use crate::core::SqlValue;

/// What a [`DeriveFn`] returns. `Err` is shown on the form and nothing is written.
pub type DeriveFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Fills one derived column. `values` are the columns the form is writing;
/// `before` is the stored row on update. Async so a hook can hash off the runtime.
pub type DeriveFn = for<'a> fn(
    &'a mut Vec<(&'static str, SqlValue)>,
    Option<&'a serde_json::Value>,
) -> DeriveFuture<'a>;

/// One table's derived-column hook, registered through `inventory`.
pub struct AdminDerivedField {
    pub table: &'static str,
    pub derive: DeriveFn,
}

inventory::collect!(AdminDerivedField);

/// Run every hook registered for `table`.
pub(crate) async fn apply(
    table: &str,
    values: &mut Vec<(&'static str, SqlValue)>,
    before: Option<&serde_json::Value>,
) -> Result<(), String> {
    for entry in inventory::iter::<AdminDerivedField> {
        if entry.table == table {
            (entry.derive)(values, before).await?;
        }
    }
    Ok(())
}

/// The submitted text for `column`, else the stored one.
#[cfg_attr(not(feature = "sso"), allow(dead_code))]
pub(crate) fn text(
    values: &[(&'static str, SqlValue)],
    before: Option<&serde_json::Value>,
    column: &str,
) -> Option<String> {
    values
        .iter()
        .find(|(c, _)| *c == column)
        .and_then(|(_, v)| match v {
            SqlValue::String(s) => Some(s.clone()),
            _ => None,
        })
        .or_else(|| before?.get(column)?.as_str().map(str::to_owned))
}

/// The admin writes `active` itself; a deactivation there ends the account's
/// sessions and JWT refresh in the same UPDATE, at the logout cut-off.
pub(crate) fn revoke_on_deactivate<'a>(
    values: &'a mut Vec<(&'static str, SqlValue)>,
    before: Option<&'a serde_json::Value>,
) -> DeriveFuture<'a> {
    use crate::session::SESSIONS_REVOKED_AT;
    Box::pin(async move {
        let was_active = before
            .and_then(|r| r.get("active"))
            .is_some_and(|v| v.as_bool() == Some(true) || v.as_i64().is_some_and(|n| n != 0));
        let deactivates = values
            .iter()
            .any(|(c, v)| *c == "active" && matches!(v, SqlValue::Bool(false)));
        if !(was_active && deactivates) {
            return Ok(());
        }
        let prev = before
            .and_then(|r| r.get(SESSIONS_REVOKED_AT)?.as_str())
            .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
        let cutoff = crate::session::logout_cutoff(prev, 0);
        let cutoff = chrono::DateTime::from_timestamp(cutoff, 0).unwrap_or_else(chrono::Utc::now);
        values.retain(|(c, _)| *c != SESSIONS_REVOKED_AT);
        values.push((SESSIONS_REVOKED_AT, SqlValue::DateTime(cutoff)));
        Ok::<(), String>(())
    })
}

/// The submitted plaintext of a `password`-widget column, taken out of
/// `values`. The admin already dropped it when an edit left it empty.
pub(crate) fn take_secret(
    values: &mut Vec<(&'static str, SqlValue)>,
    column: &str,
) -> Option<String> {
    let i = values.iter().position(|(c, _)| *c == column)?;
    match values.remove(i).1 {
        SqlValue::String(s) => Some(s),
        _ => None,
    }
}
