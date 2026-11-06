//! A framework router serves whichever tenant backend is mounted.
//!
//! The default `Tenant` is `Tenant<DefaultTenantDb>`, Postgres whenever
//! `postgres` is compiled in, so a router whose handler takes it answers
//! 500 to a SQLite or MySQL tenant in a multi-backend build. Handlers take
//! `TenantScope` (any backend) or a `TenantArg` the router picks; the
//! `_for::<DB>` variants name their backend. A real exception goes in
//! `ALLOWED` with its reason.

use std::path::{Path, PathBuf};

/// `(file, line text, why)` allowed to name the default backend.
const ALLOWED: &[(&str, &str, &str)] = &[];

/// Code that pins a handler or router to the default backend.
const PINNED: &[&str] = &[
    "t: Tenant,",
    "t: Tenant)",
    "_for::<DefaultTenantDb>",
    "Tenant<DefaultTenantDb>",
    "TenantContext<DefaultTenantDb>",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_framework_router_is_pinned_to_the_default_backend() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.len() > 100, "walked {} files", files.len());
    let mut found = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&src).unwrap().display().to_string();
        let text = std::fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || !PINNED.iter().any(|p| code.contains(p)) {
                continue;
            }
            let allowed = ALLOWED
                .iter()
                .any(|(f, l, _)| *f == rel && code.contains(l));
            if !allowed {
                found.push(format!("src/{rel}:{}: {code}", n + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "these pin a handler to the default tenant backend; take `TenantScope` \
         (or a `TenantArg` the router picks) so SQLite and MySQL tenants work in a \
         build that also has postgres, or add them to ALLOWED with a reason:\n{}",
        found.join("\n")
    );
}
