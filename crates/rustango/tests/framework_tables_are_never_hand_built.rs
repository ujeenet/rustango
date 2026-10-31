//! A test never hand-builds a framework table.
//!
//! The live suites share one PostgreSQL and one MySQL database. A test
//! that builds `rustango_users` from its own DDL or its own model leaves
//! a table the next suite's `migrate_framework` keeps (`IF NOT EXISTS`),
//! and that suite then fails on a column the copy lacks. Build framework
//! tables with `testkit`; a test that needs another shape uses a scratch
//! database and is listed below with its reason. Files that never reach a
//! server database are not checked.

use std::collections::BTreeSet;
use std::path::Path;

/// `(file, table, why)` for tests allowed to build a framework table themselves.
const ALLOWED: &[(&str, &str, &str)] = &[
    ("test_db_tri.rs", "rustango_users", "SQLite only"),
    (
        "queryset_rel_m2m_gfk_live.rs",
        "rustango_content_types",
        "SQLite in-memory pool",
    ),
    (
        "sso_link_tri.rs",
        "rustango_sso_providers",
        "restores the framework table in the same scenario",
    ),
    (
        "sso_policy_tri.rs",
        "rustango_sso_providers",
        "restores the framework table in the same scenario",
    ),
    (
        "sso_policy_tri.rs",
        "rustango_sso_links",
        "restores the framework table in the same scenario",
    ),
    (
        "migrate_tenant_storage_live.rs",
        "rustango_users",
        "scratch databases",
    ),
    (
        "manage_live.rs",
        "rustango_users",
        "a copy in a tenant run, dropped by the same test",
    ),
];

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `table = "rustango_*"` the framework's own models declare.
fn framework_tables() -> BTreeSet<String> {
    let mut files = Vec::new();
    rust_files(&crate_dir().join("src"), &mut files);
    let mut tables = BTreeSet::new();
    for file in files {
        let src = std::fs::read_to_string(&file).expect("read src");
        for name in declared_tables(&src) {
            tables.insert(name);
        }
    }
    tables
}

/// Tables named by a model's `table = "..."`, not a `let table = "..."`.
fn declared_tables(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in code_lines(src) {
        let mut rest = line;
        while let Some(at) = rest.find("table = \"") {
            let is_let = rest[..at].ends_with("let ");
            rest = &rest[at + "table = \"".len()..];
            if is_let {
                continue;
            }
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].to_owned());
            }
        }
    }
    out
}

/// Tables named right after `CREATE TABLE`, past `IF NOT EXISTS`, a schema
/// and quotes.
fn created_tables(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in code_lines(src) {
        let mut rest = line;
        while let Some(at) = rest.find("CREATE TABLE ") {
            rest = &rest[at + "CREATE TABLE ".len()..];
            let name = rest.trim_start_matches("IF NOT EXISTS ");
            let name: String = name
                .chars()
                .take_while(|c| c.is_alphanumeric() || "_.`\"\\{}".contains(*c))
                .collect();
            let name = name.rsplit('.').next().unwrap_or_default();
            out.push(name.trim_matches(|c| "`\"\\".contains(c)).to_owned());
        }
    }
    out
}

/// Can this file reach the shared PostgreSQL or MySQL database?
fn reaches_a_server(src: &str) -> bool {
    [
        "DATABASE_URL",
        "MYSQL_TEST_URL",
        "tri_dialect_test",
        "matrix::Backend",
    ]
    .iter()
    .any(|k| src.contains(k))
}

fn code_lines(src: &str) -> impl Iterator<Item = &str> {
    src.lines().filter(|l| !l.trim_start().starts_with("//"))
}

#[test]
fn no_test_builds_a_framework_table_by_hand() {
    let framework = framework_tables();
    assert!(
        framework.contains("rustango_users") && framework.len() > 20,
        "the scan of src/ found too few framework tables: {framework:?}"
    );
    let mut files = Vec::new();
    rust_files(&crate_dir().join("tests"), &mut files);
    let mut offenders = Vec::new();
    for file in files {
        let src = std::fs::read_to_string(&file).expect("read test");
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        // This file names the keys and spellings it scans for.
        if !reaches_a_server(&src) || name == "framework_tables_are_never_hand_built.rs" {
            continue;
        }
        let built = declared_tables(&src)
            .into_iter()
            .chain(created_tables(&src));
        for table in built.filter(|t| framework.contains(t)) {
            if !ALLOWED.iter().any(|(f, t, _)| *f == name && *t == table) {
                offenders.push(format!("{name}: {table}"));
            }
        }
    }
    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "build framework tables with `testkit` (or a scratch database \
         listed in ALLOWED): {offenders:?}"
    );
}

#[test]
fn the_scan_sees_each_spelling() {
    let src = "#[rustango(table = \"rustango_users\")]\n\
               let table = \"rustango_roles\";\n\
               \"CREATE TABLE IF NOT EXISTS `rustango_users` (id INT)\"\n\
               r#\"CREATE TABLE public.\"rustango_orgs\" AS SELECT 1\"#\n\
               // CREATE TABLE rustango_roles (id INT)\n";
    assert_eq!(declared_tables(src), ["rustango_users"]);
    assert_eq!(created_tables(src), ["rustango_users", "rustango_orgs"]);
}
