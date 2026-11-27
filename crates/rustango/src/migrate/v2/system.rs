//! The framework's system chain on engine v2 (design items 20-22): JSON
//! files shipped in the crate, one app per model module (and scope), the
//! feature-gated columns in apps of their own. An app runs when every table
//! and column it adopts is in the compiled models.

use std::collections::BTreeMap;
use std::path::Path;

use super::format::Op;
use super::load::Loaded;
use super::LoadError;

/// `(app, file stem, text)` of every shipped system file.
const FILES: &[(&str, &str, &str)] = include!("system/files.rs");

/// Only managed models count: an unmanaged one may map the same table with
/// fewer columns.
pub(crate) fn compiled(table: &str, column: Option<&str>) -> bool {
    inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .filter(|e| e.schema.table == table && e.schema.managed && !e.schema.is_view)
        .any(|e| column.is_none_or(|c| e.schema.fields.iter().any(|f| f.column == c)))
}

/// The shipped system files this build runs: an app whose every adopted
/// table and column is compiled.
///
/// # Errors
/// A shipped file that does not load (a broken build).
pub(crate) fn files() -> Result<Vec<Loaded>, LoadError> {
    let mut out = Vec::new();
    let mut apps: BTreeMap<&str, Vec<Loaded>> = BTreeMap::new();
    for (app, stem, text) in FILES {
        let path = Path::new("system").join(app).join(format!("{stem}.json"));
        apps.entry(app)
            .or_default()
            .push(super::load::load_file(&path, text)?);
    }
    for (_, files) in apps {
        let runs = files.iter().all(|l| {
            l.file.operations.iter().all(|e| match &e.op {
                Op::AdoptTable { table, .. } => compiled(&table.name, None),
                Op::AdoptColumn { table, column, .. } => {
                    compiled(table, Some(&column.column.column))
                }
                _ => true,
            })
        });
        if runs {
            out.extend(files);
        }
    }
    Ok(out)
}

/// Writing the files from the models: only the every-feature build checks them.
#[cfg(all(
    test,
    feature = "tenancy",
    feature = "admin",
    feature = "sso",
    feature = "admin-sso",
    feature = "media",
    feature = "passkey",
    feature = "mcp",
    feature = "totp"
))]
#[path = "system_gen.rs"]
pub(crate) mod gen;

#[cfg(test)]
#[path = "system_tests.rs"]
mod tests;
