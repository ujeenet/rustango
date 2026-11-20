//! Code branches on `Dialect::kind()` or a `Dialect` method, never on the
//! `name()` string: a typo there compiles, and a new backend falls through
//! a `_` arm unnoticed. `name()` stays for logs and messages.

use std::path::{Path, PathBuf};

const NAMES: &[&str] = &["\"postgres\"", "\"mysql\"", "\"sqlite\"", "\"mariadb\""];

fn workspace_crates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !path.ends_with("target") {
                source_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The reason `line` branches on a dialect name string, if it does.
fn name_branch(line: &str) -> Option<&'static str> {
    let code = line.split("//").next().unwrap_or("");
    let calls = [".name()", ".backend_name()"];
    for call in calls {
        let mut from = 0;
        while let Some(at) = code[from..].find(call) {
            let end = from + at + call.len();
            let after = code[end..].trim_start();
            let receiver = &code[..from + at];
            // What precedes the receiver expression, e.g. `"mysql" ==`.
            let before = receiver
                .trim_end_matches(|c: char| c.is_alphanumeric() || "_.()&".contains(c))
                .trim_end();
            let cmp = |s: &str| s.starts_with("==") || s.starts_with("!=");
            if cmp(after) && NAMES.iter().any(|n| after[2..].trim_start().starts_with(n)) {
                return Some("compares a dialect name to a literal");
            }
            if (before.ends_with("==") || before.ends_with("!="))
                && NAMES
                    .iter()
                    .any(|n| before[..before.len() - 2].trim_end().ends_with(n))
            {
                return Some("compares a dialect name to a literal");
            }
            let dialectish = receiver.ends_with("dialect()")
                || receiver.ends_with("dialect")
                || receiver.ends_with(" d")
                || receiver.ends_with("b.d")
                || call == ".backend_name()";
            if dialectish && code.trim_start().contains("match ") && after.starts_with('{') {
                return Some("matches on a dialect name");
            }
            if dialectish
                && receiver.contains("matches!(")
                && after.starts_with(',')
                && NAMES.iter().any(|n| after[1..].trim_start().starts_with(n))
            {
                return Some("matches on a dialect name");
            }
            from = end;
        }
    }
    None
}

#[test]
fn no_branch_on_a_dialect_name_string() {
    let mut files = Vec::new();
    source_files(&workspace_crates(), &mut files);
    assert!(files.len() > 100, "found only {} files", files.len());
    let mut hits = Vec::new();
    for path in &files {
        if path.ends_with(file!().rsplit('/').next().unwrap()) {
            continue;
        }
        let src = std::fs::read_to_string(path).unwrap();
        for (i, line) in src.lines().enumerate() {
            if let Some(why) = name_branch(line) {
                hits.push(format!(
                    "{}:{}: {why}: {}",
                    path.display(),
                    i + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "branch on `Dialect::kind()` or a `Dialect` method instead:\n{}",
        hits.join("\n")
    );
}

#[test]
fn the_guard_sees_each_shape() {
    for line in [
        r#"if b.d.name() == "sqlite" {"#,
        r#"let pg = pool.dialect().name() != "postgres";"#,
        r#"if "mysql" == d.name() {"#,
        r#"match pool.dialect().name() {"#,
        r#"let sql = match dialect.name() {"#,
        r#"if pool.backend_name() == "sqlite" {"#,
        r#"let ok = matches!(dialect.name(), "postgres" | "sqlite");"#,
    ] {
        assert!(name_branch(line).is_some(), "missed: {line}");
    }
    for line in [
        r#"if b.d.kind() == DialectKind::Sqlite {"#,
        r#"dialect: b.d.name(),"#,
        r#"panic!("on {}", pool.dialect().name());"#,
        r#"// was `d.name() == "sqlite"`"#,
        r#"match field.name() {"#,
    ] {
        assert!(name_branch(line).is_none(), "false hit: {line}");
    }
}
