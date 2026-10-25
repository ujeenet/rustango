//! The test this crate never had (#1207).
//!
//! `templates.rs` claimed "CI snapshot-tests the generated output by running
//! `cargo check` on each template". Nothing did — the unit tests are
//! `.contains()` assertions on template *strings*, and no test ever wrote a
//! project to disk. That is how `--template api` came to generate a project
//! that failed with 75 compiler errors, and how two templates came to emit
//! warnings, without anything going red.
//!
//! These tests generate each template into a tempdir and compile it. They use
//! `--rustango-path` so they validate the **working tree** rather than the last
//! published release — checking against crates.io would have said "fine" while
//! the tree was broken, and vice versa.
//!
//! Marked `#[ignore]` because each case compiles rustango (minutes, not
//! milliseconds) and `cargo test` should stay fast for everyone else. CI runs
//! them explicitly:
//!
//! ```sh
//! cargo test -p cargo-rustango -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path to `crates/rustango` in this checkout.
fn rustango_crate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("rustango")
}

/// These tests need the sibling `crates/rustango` source, so they only mean
/// anything inside the repo. The published `cargo-rustango` tarball carries
/// this file but not its sibling, so skip rather than fail confusingly for
/// anyone who runs the suite from a downloaded crate.
fn in_repo() -> bool {
    rustango_crate().join("Cargo.toml").is_file()
}

/// A unique scratch directory. No `tempfile` dep — this crate deliberately has
/// zero dependencies, and the scaffolder must keep working without them.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustango-scaffold-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Generate `template` into a fresh directory and return the project root.
///
/// `flags` are extra scaffolder arguments — `--backend`, `--features`.
fn generate(template: &str, flags: &[&str], tag: &str) -> (PathBuf, PathBuf) {
    let work = scratch(tag);
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
        .current_dir(&work)
        .args(["rustango", "new", "probe", "--template", template])
        .args(flags)
        .arg("--rustango-path")
        .arg(rustango_crate())
        .output()
        .expect("run the scaffolder");
    assert!(
        out.status.success(),
        "scaffolding {template} {flags:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let root = work.join("probe");
    (work, root)
}

fn scaffold_target() -> PathBuf {
    std::env::temp_dir().join("rustango-scaffold-target")
}

/// `cargo check` the generated project, returning (compiled, warnings, log).
///
/// Warnings are counted **only for the generated crate**, never for rustango.
/// The obvious implementation — `RUSTFLAGS=-D warnings` — is wrong: RUSTFLAGS
/// applies to every crate in the graph, so the framework's own warnings would
/// fail the test and mask the thing being measured. `--message-format=json`
/// carries the emitting package on each diagnostic, so filter on that.
///
/// The bar matters because a project you have just generated is the one moment
/// when there is nothing of yours to blame: anything the compiler says about it
/// is the generator's (#1210).
fn check(root: &Path, extra: &[&str]) -> (bool, usize, String) {
    let out = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["check", "--message-format=json-diagnostic-rendered-ansi"])
        .args(extra)
        // One shared target dir across every case, so rustango compiles once
        // per feature set rather than once per test.
        .env("CARGO_TARGET_DIR", scaffold_target())
        .env("CARGO_INCREMENTAL", "0")
        // Pick deps the running toolchain supports; the repo pins 1.88 (#2310).
        .env("CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS", "fallback")
        .output()
        .expect("run cargo check");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut warnings = Vec::new();
    for line in stdout.lines() {
        // Crude but dependency-free JSON probing — this crate has no deps by
        // design. A compiler-message line for the probe package looks like:
        //   {"reason":"compiler-message","package_id":"…probe…","message":{…}}
        if !line.contains(r#""reason":"compiler-message""#) {
            continue;
        }
        if !line.contains("probe") {
            continue; // a dependency's diagnostic — not ours to judge
        }
        if line.contains(r#""level":"warning""#) {
            warnings.push(line.to_owned());
        }
    }
    let log = format!("{stdout}{}", String::from_utf8_lossy(&out.stderr));
    (out.status.success(), warnings.len(), log)
}

fn assert_compiles(template: &str, extra: &[&str], label: &str) {
    assert_compiles_with(template, &[], extra, label);
}

/// As [`assert_compiles`], but `scaffold` flags shape the project first.
fn assert_compiles_with(template: &str, scaffold: &[&str], extra: &[&str], label: &str) {
    if !in_repo() {
        eprintln!("skipping: crates/rustango not alongside — not a repo checkout");
        return;
    }
    let (work, root) = generate(template, scaffold, label);
    let (ok, warnings, log) = check(&root, extra);
    let _ = std::fs::remove_dir_all(&work);
    assert!(ok, "`{template}` {label} did not compile:\n{log}");
    assert_eq!(
        warnings, 0,
        "`{template}` {label} compiled but emitted {warnings} warning(s) — a \
         freshly generated project must be clean (#1210):\n{log}"
    );
}

// ---------------------------------------------------------------- as generated

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn api_template_compiles() {
    assert_compiles("api", &[], "default");
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn fullstack_template_compiles() {
    assert_compiles("fullstack", &[], "default");
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn tenant_template_compiles() {
    assert_compiles("tenant", &[], "default");
}

// ------------------------------------------------- the advertised backend switch

// The generated manifest documents `cargo run --no-default-features --features
// sqlite`. That invocation used to be the one that *broke* the build: the
// dependency pinned `rustango/postgres` while the project's own `postgres`
// feature was off, and `#[derive(Model)]` gates its emissions on the project's
// features — so rustango demanded `MaybePgFromRow` and the derive had skipped
// it (#1211).

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn api_template_compiles_on_sqlite() {
    assert_compiles(
        "api",
        &["--no-default-features", "--features", "sqlite"],
        "sqlite",
    );
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn api_template_compiles_on_mysql() {
    assert_compiles(
        "api",
        &["--no-default-features", "--features", "mysql"],
        "mysql",
    );
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn fullstack_template_compiles_on_sqlite() {
    assert_compiles(
        "fullstack",
        &["--no-default-features", "--features", "sqlite"],
        "sqlite",
    );
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn tenant_template_compiles_on_sqlite() {
    assert_compiles(
        "tenant",
        &["--no-default-features", "--features", "sqlite"],
        "sqlite",
    );
}

// -------------------------------------------------------- `--backend` (#1345)
//
// The switch above is what a project can *reach*; these are what it is
// *generated as*. `--backend sqlite` has to compile with a bare `cargo check`,
// no flags — otherwise the flag only rewrote a comment.

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn backend_sqlite_compiles_as_generated() {
    assert_compiles_with("fullstack", &["--backend", "sqlite"], &[], "backend-sqlite");
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn backend_mysql_compiles_as_generated() {
    assert_compiles_with("fullstack", &["--backend", "mysql"], &[], "backend-mysql");
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn tenant_on_sqlite_backend_compiles_as_generated() {
    assert_compiles_with("tenant", &["--backend", "sqlite"], &[], "tenant-sqlite");
}

// ------------------------------------------------------- `--features` (#1345)

/// Opt-ins no template reaches — the reason the flag exists.
#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn selected_features_compile() {
    assert_compiles_with(
        "fullstack",
        &["--features", "csrf,cache-page,testkit"],
        &[],
        "features",
    );
}

/// `--features tenancy` on a non-tenant template: the feature is additive, so
/// it must not break a project whose `main.rs` never calls into it.
#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn tenancy_feature_on_fullstack_compiles() {
    assert_compiles_with(
        "fullstack",
        &["--features", "tenancy"],
        &[],
        "features-tenancy",
    );
}

// ------------------------------------------------------------ run it

/// Status line and headers of a signed-out `GET path` on the apex.
fn get_head(port: u16, path: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nAccept: text/html\r\nConnection: close\r\n\r\n"
    )
    .expect("send");
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    resp.split("\r\n\r\n").next().unwrap_or_default().to_owned()
}

/// A generated tenant project mounts the console's provisioning pages: signed
/// out they redirect to the login, where a route that is not mounted is a 404.
#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn tenant_project_mounts_the_provisioning_pages() {
    if !in_repo() {
        eprintln!("skipping: crates/rustango not alongside — not a repo checkout");
        return;
    }
    let (work, root) = generate("tenant", &["--backend", "sqlite"], "tenant-run");
    let built = Command::new(env!("CARGO"))
        .current_dir(&root)
        .arg("build")
        .env("CARGO_TARGET_DIR", scaffold_target())
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS", "fallback")
        .status()
        .expect("run cargo build");
    assert!(built.success(), "the tenant project did not build");

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();
    let log = work.join("server.log");
    let mut server = Command::new(scaffold_target().join("debug").join("probe"))
        .current_dir(&root)
        .env("DATABASE_URL", "sqlite://./probe_dev.db?mode=rwc")
        .env("RUSTANGO_BIND", format!("127.0.0.1:{port}"))
        .env("RUSTANGO_APEX_DOMAIN", "localhost")
        .stdout(std::fs::File::create(&log).expect("log file"))
        .stderr(std::fs::File::create(work.join("server.err")).expect("log file"))
        .spawn()
        .expect("start the server");
    let up = (0..600).any(|_| {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if server.try_wait().ok().flatten().is_some() {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        false
    });
    let heads: Vec<_> = if up {
        ["/orgs/new", "/db-servers"]
            .iter()
            .map(|p| (*p, get_head(port, p)))
            .collect()
    } else {
        Vec::new()
    };
    let _ = server.kill();
    let _ = server.wait();
    let logs = format!(
        "{}{}",
        std::fs::read_to_string(&log).unwrap_or_default(),
        std::fs::read_to_string(work.join("server.err")).unwrap_or_default()
    );
    let _ = std::fs::remove_dir_all(&work);
    assert!(up, "the server did not come up:\n{logs}");
    for (path, head) in heads {
        let status = head.lines().next().unwrap_or_default();
        assert!(
            status.contains(" 302 ") || status.contains(" 303 "),
            "{path} is not mounted: {head}\n{logs}"
        );
        assert!(
            head.to_ascii_lowercase().contains("login"),
            "{path}: {head}"
        );
    }
}
