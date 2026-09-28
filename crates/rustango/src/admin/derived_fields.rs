//! Columns the admin computes on every create and update, from the other
//! submitted values (and, on update, the stored row). Registered per table.

use crate::core::SqlValue;

/// Fills one derived column. `values` are the columns the form is writing;
/// `before` is the stored row on update.
pub type DeriveFn = fn(&mut Vec<(&'static str, SqlValue)>, Option<&serde_json::Value>);

/// One table's derived-column hook, registered through `inventory`.
pub struct AdminDerivedField {
    pub table: &'static str,
    pub derive: DeriveFn,
}

inventory::collect!(AdminDerivedField);

/// Run every hook registered for `table`.
pub(crate) fn apply(
    table: &str,
    values: &mut Vec<(&'static str, SqlValue)>,
    before: Option<&serde_json::Value>,
) {
    for entry in inventory::iter::<AdminDerivedField> {
        if entry.table == table {
            (entry.derive)(values, before);
        }
    }
}

/// The submitted text for `column`, else the stored one.
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
