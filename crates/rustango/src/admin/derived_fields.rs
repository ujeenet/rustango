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
