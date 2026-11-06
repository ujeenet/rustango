//! Every framework model declares who may serve its table, so a new
//! secret-holding table cannot default to open by being forgotten.
//! Build with `--all-features` to see every model.

use rustango::core::ModelEntry;

#[test]
fn every_framework_model_declares_its_table_access() {
    let framework: Vec<&ModelEntry> = rustango::core::inventory::iter::<ModelEntry>
        .into_iter()
        .filter(|e| e.module_path == "rustango" || e.module_path.starts_with("rustango::"))
        .collect();
    assert!(
        framework
            .iter()
            .any(|e| e.schema.table == "rustango_audit_log"),
        "vacuous: no framework model registered"
    );
    let undeclared: Vec<String> = framework
        .iter()
        .filter(|e| e.schema.access.is_none())
        .map(|e| format!("{} ({})", e.schema.table, e.module_path))
        .collect();
    assert!(
        undeclared.is_empty(),
        "add #[rustango(access = \"open\" | \"admin_only\" | \"never\")] to: {undeclared:?}"
    );
}
