//! A scaffolded worker must be able to name a scaffolded job.
//!
//! `manage make:worker` emits `src/bin/<name>.rs`, and the register
//! example inside it said:
//!
//! ```text
//!   queue.register::<crate::jobs::WelcomeEmail>().await;
//! ```
//!
//! Two things were wrong with that, and together they made the verb
//! produce a worker that could not be finished:
//!
//! 1. A file under `src/bin/` **is its own crate**. `crate::` there
//!    means the worker binary, so the path could never resolve to the
//!    project's code no matter what the project contained.
//! 2. `cargo rustango new` wrote no `src/lib.rs` at all, so there was
//!    no library for a binary to reach into even with the right path.
//!    Both committed examples had hand-written a `lib.rs` to get past
//!    this, which is precisely the tell.
//!
//! The property is end-to-end and cannot be asserted on template
//! strings: generate a project, scaffold a job and a worker with the
//! project's own verbs, follow the advice each verb prints, and
//! compile. Before the fix this does not build.
//!
//! `#[ignore]` because it compiles rustango twice over.
//!
//! This header used to say "CI runs it with the rest: `cargo test -p
//! cargo-rustango -- --ignored`". It did not — the only `--ignored` in
//! `ci.yml` was scoped to `--test generated_project_compiles`, so both
//! tests here ran nowhere while the file asserted they ran (#1592).
//! `scaffolded_projects` now names this suite explicitly.

use std::path::{Path, PathBuf};
use std::process::Command;

fn rustango_crate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("rustango")
}

/// Only meaningful inside the repo — the published tarball carries this
/// file but not `crates/rustango`.
fn in_repo() -> bool {
    rustango_crate().join("Cargo.toml").is_file()
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustango-worker-jobs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// One target dir for the whole file, so rustango compiles once.
fn shared_target() -> PathBuf {
    std::env::temp_dir().join("rustango-scaffold-target")
}

/// Run one of the project's own management verbs via `cargo run`.
///
/// Deliberately the real path a user takes, not a direct call into
/// `rustango::migrate`: it also proves the verb dispatches without a
/// database, which a fresh project has no way to provide yet.
fn manage(root: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["run", "--quiet", "--"])
        .args(args)
        .env("CARGO_TARGET_DIR", shared_target())
        .env("CARGO_INCREMENTAL", "0")
        .output()
        .unwrap_or_else(|e| panic!("run manage {args:?}: {e}"));
    assert!(
        out.status.success(),
        "`cargo run -- {}` failed:\n{}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `cargo check --all-targets`, returning (compiled, probe warnings, log).
fn check(root: &Path) -> (bool, usize, String) {
    let out = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "check",
            "--all-targets",
            "--message-format=json-diagnostic-rendered-ansi",
        ])
        .env("CARGO_TARGET_DIR", shared_target())
        .env("CARGO_INCREMENTAL", "0")
        .output()
        .expect("run cargo check");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let warnings = stdout
        .lines()
        .filter(|l| l.contains(r#""reason":"compiler-message""#))
        .filter(|l| l.contains("probe"))
        .filter(|l| l.contains(r#""level":"warning""#))
        .count();
    let log = format!("{stdout}{}", String::from_utf8_lossy(&out.stderr));
    (out.status.success(), warnings, log)
}

/// Uncomment the one `queue.register::<..>()` line the worker ships as
/// an example, exactly as a user would.
fn uncomment_register(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(i) if line.contains("queue.register::<") => {
                let indent = &line[..i];
                let code = line[i + 2..].trim_start();
                format!("{indent}{code}")
            }
            _ => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
#[ignore = "compiles rustango; run with --ignored"]
fn a_generated_worker_compiles_against_a_generated_job() {
    if !in_repo() {
        eprintln!("skipping: crates/rustango not alongside — not a repo checkout");
        return;
    }
    let work = scratch();

    let out = Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
        .current_dir(&work)
        .args(["rustango", "new", "probe", "--template", "fullstack"])
        .arg("--rustango-path")
        .arg(rustango_crate())
        .output()
        .expect("run the scaffolder");
    assert!(
        out.status.success(),
        "scaffolding failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let root = work.join("probe");

    // The library target is the whole precondition: without it a binary
    // under `src/bin/` has nothing to import.
    let lib_rs = root.join("src/lib.rs");
    assert!(
        lib_rs.is_file(),
        "`cargo rustango new` wrote no src/lib.rs, so a worker binary has no \
         library to reach into and `make:worker` cannot be completed"
    );

    // `make:job` before `make:worker`: the second binary makes plain
    // `cargo run` ambiguous, which is what the verb warns about.
    let job_out = manage(&root, &["make:job", "WelcomeEmail"]);
    assert!(
        root.join("src/welcome_email.rs").is_file(),
        "make:job wrote no src/welcome_email.rs; got:\n{job_out}"
    );
    assert!(
        job_out.contains("pub mod welcome_email;") && job_out.contains("src/lib.rs"),
        "with a library target present, make:job must advise `pub mod ...;` in \
         src/lib.rs — `mod` in main.rs would hide the job from src/bin/. Got:\n{job_out}"
    );

    let worker_out = manage(&root, &["make:worker", "Drain"]);
    let worker_path = root.join("src/bin/drain.rs");
    assert!(
        worker_path.is_file(),
        "make:worker wrote no src/bin/drain.rs; got:\n{worker_out}"
    );

    let worker_src = std::fs::read_to_string(&worker_path).expect("read the worker");
    assert!(
        worker_src.contains("probe::welcome_email::WelcomeEmail"),
        "the worker's register example must name the project crate. `crate::` \
         inside src/bin/ is the worker's own crate and can never resolve. Got:\n{worker_src}"
    );

    // Now follow the printed advice, which is all a user has to go on.
    // Appended, not prepended: `lib.rs` opens with `//!` docs and an
    // inner doc comment has to be the first thing in the file.
    let lib_src = std::fs::read_to_string(&lib_rs).expect("read lib.rs");
    std::fs::write(&lib_rs, format!("{lib_src}\npub mod welcome_email;\n")).expect("patch lib.rs");
    std::fs::write(&worker_path, uncomment_register(&worker_src)).expect("patch the worker");

    let (ok, warnings, log) = check(&root);
    let _ = std::fs::remove_dir_all(&work);
    assert!(
        ok,
        "a generated worker registering a generated job did not compile:\n{log}"
    );
    assert_eq!(
        warnings, 0,
        "it compiled but emitted {warnings} warning(s); a freshly generated \
         project must be clean:\n{log}"
    );
}

/// The helper above must actually uncomment, or the test would compile
/// a worker that registers nothing and still pass.
#[test]
fn uncomment_register_produces_code() {
    let src = concat!(
        "    //   queue.register::<probe::welcome_email::WelcomeEmail>().await;\n",
        "    // an ordinary comment\n",
        "    queue.start().await;",
    );
    let out = uncomment_register(src);
    assert!(
        out.contains("    queue.register::<probe::welcome_email::WelcomeEmail>().await;"),
        "the register line must come back as code, indentation intact: {out:?}"
    );
    assert!(
        out.contains("    // an ordinary comment"),
        "only the register line may be uncommented: {out:?}"
    );
}
