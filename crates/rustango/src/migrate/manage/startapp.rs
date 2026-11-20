//! `startapp`.

use super::*;

/// `startapp <name> [--into <dir>] [--with-manage-bin]` — scaffold an app
/// module under `src/<name>/` (`models.rs` + `views.rs` + `urls.rs` +
/// `mod.rs`). Idempotent — files that already exist are reported as
/// skipped. With `--with-manage-bin`, also writes `src/bin/manage.rs`
/// with the standard single-tenant dispatcher boilerplate.
pub(super) fn startapp<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut iter = args.iter();
    let app_name = iter
        .next()
        .cloned()
        .ok_or_else(|| MigrateError::Validation(usage()))?;
    let mut with_manage_bin = false;
    let mut into: Option<String> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--with-manage-bin" => with_manage_bin = true,
            "--into" => {
                into = Some(iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--into requires a directory argument".into())
                })?);
            }
            "--help" | "-h" => {
                writeln!(w, "{}", usage())?;
                return Ok(());
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "startapp: unknown argument `{other}` (run --help for usage)"
                )));
            }
        }
    }
    let opts = crate::migrate::scaffold::StartAppOptions {
        app_name: app_name.clone(),
        manage_bin: with_manage_bin.then_some(crate::migrate::scaffold::SINGLE_TENANT_MANAGE_BIN),
        base_dir: into.as_deref().map(crate::migrate::scaffold::into_base_dir),
        crate_root: None,
    };
    // Project root = current working directory. Most users run
    // `cargo run -- startapp …` from the project root,
    // which is exactly where Cargo.toml lives. Document this in the
    // help string so non-default invocations are an explicit `cd`.
    let cwd = std::env::current_dir()?;
    let report = crate::migrate::scaffold::startapp(&cwd, &opts)?;
    write_startapp_report(w, &app_name, &report)
}

pub(super) fn write_startapp_report<W: Write>(
    w: &mut W,
    app_name: &str,
    report: &crate::migrate::scaffold::StartAppReport,
) -> Result<(), MigrateError> {
    if report.written.is_empty() && report.skipped.is_empty() {
        writeln!(w, "startapp: nothing to do")?;
        return Ok(());
    }
    writeln!(w, "startapp `{app_name}`")?;
    for path in &report.written {
        writeln!(w, "  + wrote {path}")?;
    }
    for path in &report.skipped {
        writeln!(w, "  · {path} already exists — left untouched")?;
    }
    for path in &report.patched {
        writeln!(w, "  ~ patched {path} (auto-mounted new app)")?;
    }
    for hint in &report.manual_steps {
        writeln!(w, "  ! manual: {hint}")?;
    }
    if !report.written.is_empty() {
        writeln!(w, "next:")?;
        for cmd in crate::migrate::scaffold::STARTAPP_NEXT {
            writeln!(w, "  {cmd}")?;
        }
    }
    Ok(())
}

pub(super) fn usage() -> String {
    "startapp <name> [--into <dir>] [--with-manage-bin]\n  \
     Scaffold an app module under src/<name>/ (mod.rs +\n  \
     models.rs + views.rs + urls.rs). Idempotent: existing files\n  \
     are left untouched. <name> must be a valid Rust identifier.\n\n  \
     --into <dir>\n  \
     Write under src/<dir>/<name>/ instead, e.g. `--into apps`. The\n  \
     parent module and urls.rs lines are printed for you to add.\n\n  \
     --with-manage-bin\n  \
     Also write src/bin/manage.rs with the single-tenant dispatcher\n  \
     boilerplate. Skipped if the file already exists."
        .to_owned()
}
