//! The framework's system chain on engine v2 (design items 20-22): JSON
//! files shipped in the crate, one app per model module (and scope), the
//! feature-gated columns in apps of their own. An app runs when every table
//! and column it adopts is in the compiled models.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

use super::format::Op;
use super::load::Loaded;
use super::state::{Backend, Target};
use super::LoadError;

/// `(app, file stem, text)` of every shipped system file.
const FILES: &[(&str, &str, &str)] = include!("system/files.rs");

/// Columns a feature adds to a framework table: `(table, column, app)`.
const GATED_COLUMNS: &[(&str, &str, &str)] = &[
    ("rustango_admin_users", "email", "rustango_admin_sso"),
    ("rustango_users", "email", "rustango_sso"),
];

/// Indexes the framework made outside its models before they were declared
/// (design item 24): `(index, app)`. Each ships as an `adopt_column` of its
/// first column, so it is verified if present and created if not.
const SIDE_INDEXES: &[(&str, &str)] = &[(
    "rustango_content_types_natural_key",
    "rustango_contenttypes_natural_key",
)];

/// Whether the compiled models hold `table` (and `column` of it).
fn compiled(table: &str, column: Option<&str>) -> bool {
    let Some(e) = crate::core::ModelEntry::for_table(table) else {
        return false;
    };
    column.is_none_or(|c| e.schema.fields.iter().any(|f| f.column == c))
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

/// The system files today's models give: `(app, stem, text)`. Each app's
/// `0001_initial` adopts its tables; FKs into another app depend on it.
///
/// # Errors
/// A framework model engine v2 cannot hold.
pub(crate) fn generate() -> Result<Vec<(String, String, String)>, LoadError> {
    let snap = crate::migrate::snapshot::SchemaSnapshot::from_registry();
    let backends = [Backend::Postgres, Backend::Mysql, Backend::Sqlite];
    // Table → (app, target), from the model's module and scope.
    let mut owner: BTreeMap<String, (String, Target)> = BTreeMap::new();
    for e in inventory::iter::<crate::core::ModelEntry> {
        let t = e.schema.table;
        if !t.starts_with(super::make::FRAMEWORK_PREFIX) || !e.schema.managed || e.schema.is_view {
            continue;
        }
        let target = match e.schema.scope {
            crate::core::ModelScope::Registry => Target::Registry,
            crate::core::ModelScope::Tenant => Target::Tenant,
        };
        let mut app = e.module_path.replace("::", "_");
        if target == Target::Registry {
            app.push_str("_registry");
        }
        owner.insert(t.to_owned(), (app, target));
    }
    let mut ops: BTreeMap<String, (Target, Vec<Value>, BTreeSet<String>)> = BTreeMap::new();
    for t in snap.tables.iter().filter(|t| owner.contains_key(&t.name)) {
        let (app, target) = owner[&t.name].clone();
        let mut state = super::make::table_of(t, &snap, &backends, "system")?;
        state.scope = target;
        let gated: Vec<&(&str, &str, &str)> =
            GATED_COLUMNS.iter().filter(|g| g.0 == t.name).collect();
        let mut fields = Vec::new();
        let mut deps: BTreeSet<String> = BTreeSet::new();
        for c in &state.columns {
            if let Some(g) = gated.iter().find(|g| g.1 == c.column) {
                let col = ops
                    .entry(g.2.to_owned())
                    .or_insert_with(|| (target, Vec::new(), BTreeSet::new()));
                // The column's own indexes go with it.
                let own: Vec<&super::state::IndexDef> = state
                    .indexes
                    .iter()
                    .filter(|i| i.columns == [c.column.clone()])
                    .collect();
                let mut op = json!({"op": "adopt_column", "v": 1, "table": t.name,
                    "column": super::make::field_json(c, None)});
                if !own.is_empty() {
                    op["indexes"] = serde_json::to_value(&own).expect("indexes");
                }
                col.1.push(op);
                col.2.insert(format!("{app}:0001_initial"));
                continue;
            }
            let fk = state
                .fks
                .iter()
                .find(|f| f.columns.first() == Some(&c.column));
            fields.push(super::make::field_json(c, fk));
            if let Some(f) = fk {
                if let Some((other, _)) = owner.get(&f.to).filter(|(o, _)| *o != app) {
                    deps.insert(format!("{other}:0001_initial"));
                }
            }
        }
        let mut table = json!({"name": t.name, "scope": target.as_str(), "fields": fields});
        if let Some(c) = &state.comment {
            table["comment"] = json!(c);
        }
        let mut op = json!({"op": "adopt_table", "v": 1, "table": table});
        let gated_cols: Vec<&str> = gated.iter().map(|g| g.1).collect();
        let mut indexes: Vec<&super::state::IndexDef> = Vec::new();
        for i in &state.indexes {
            if i.columns.len() == 1 && gated_cols.contains(&i.columns[0].as_str()) {
                continue;
            }
            let Some((_, side)) = SIDE_INDEXES.iter().find(|s| s.0 == i.name) else {
                indexes.push(i);
                continue;
            };
            let first = &i.columns[0];
            let c = state
                .columns
                .iter()
                .find(|c| c.column == *first)
                .expect("an index column is in its table");
            let entry = ops
                .entry((*side).to_owned())
                .or_insert_with(|| (target, Vec::new(), BTreeSet::new()));
            entry
                .1
                .push(json!({"op": "adopt_column", "v": 1, "table": t.name,
                "column": super::make::field_json(c, None), "indexes": [i]}));
            entry.2.insert(format!("{app}:0001_initial"));
        }
        if !indexes.is_empty() {
            op["indexes"] = serde_json::to_value(&indexes).expect("indexes");
        }
        if !state.checks.is_empty() {
            op["checks"] = serde_json::to_value(&state.checks).expect("checks");
        }
        let entry = ops
            .entry(app.clone())
            .or_insert_with(|| (target, Vec::new(), BTreeSet::new()));
        entry.1.push(op);
        entry.2.extend(deps);
    }
    Ok(ops
        .into_iter()
        .map(|(app, (target, mut list, deps))| {
            list = fk_order(list);
            let mut body = json!({"engine": 2, "format": 1, "id": "0001_initial", "app": app,
                "target": target.as_str(), "operations": list});
            if !deps.is_empty() {
                body["depends_on"] = json!(deps);
            }
            let text = format!("{}\n", serde_json::to_string_pretty(&body).expect("json"));
            (app, "0001_initial".to_owned(), text)
        })
        .collect())
}

/// The ops by name, each after the tables its FKs point at in this app.
fn fk_order(mut list: Vec<Value>) -> Vec<Value> {
    let name = |o: &Value| o["table"]["name"].as_str().unwrap_or_default().to_owned();
    list.sort_by_key(name);
    let mine: BTreeSet<String> = list.iter().map(name).collect();
    let targets = |o: &Value| -> BTreeSet<String> {
        o["table"]["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["fk"]["to"].as_str().map(str::to_owned))
            .filter(|t| mine.contains(t) && *t != name(o))
            .collect()
    };
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::with_capacity(list.len());
    while !list.is_empty() {
        let at = list
            .iter()
            .position(|o| targets(o).is_subset(&placed))
            .unwrap_or(0);
        let o = list.remove(at);
        placed.insert(name(&o));
        out.push(o);
    }
    out
}

#[cfg(test)]
#[path = "system_tests.rs"]
mod tests;
