//! `about`, `docs` and `version`.

use super::*;

// ============================================================ about / check / docs / version

/// `DATABASE_URL` as `manage about` prints it: through the shared `redact`.
pub(super) fn about_db_url(raw: Option<String>) -> String {
    raw.map_or_else(|| "(unset)".into(), |s| redact_url(&s))
}

/// `manage about` — env summary for support tickets / debugging.
pub(super) async fn about_cmd<W: Write>(pool: &Pool, w: &mut W) -> Result<(), MigrateError> {
    let registered_models = crate::core::inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .count();
    let mut apps: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    for entry in crate::core::inventory::iter::<crate::core::ModelEntry> {
        if let Some(app) = entry.resolved_app_label() {
            apps.insert(app);
        }
    }

    writeln!(w, "rustango")?;
    writeln!(w, "  version:        {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(w, "  models:         {registered_models} registered")?;
    writeln!(
        w,
        "  apps:           {} ({})",
        apps.len(),
        if apps.is_empty() {
            "none".to_owned()
        } else {
            apps.iter().copied().collect::<Vec<_>>().join(", ")
        }
    )?;
    let env_label = std::env::var("RUSTANGO_ENV").unwrap_or_else(|_| "(unset)".into());
    writeln!(w, "  RUSTANGO_ENV:   {env_label}")?;
    let db_url = about_db_url(crate::env::database_url_from_env().ok());
    writeln!(w, "  DATABASE_URL:   {db_url}")?;

    // DB connectivity — tri-dialect: SELECT 1 is universal across PG/MySQL/SQLite.
    write!(w, "  db_connect:     ")?;
    let ok = crate::sql::raw_execute_pool(pool, "SELECT 1", Vec::new())
        .await
        .is_ok();
    writeln!(w, "{}", if ok { "ok" } else { "FAILED" })?;

    Ok(())
}

/// `manage docs` — try to open https://docs.rs/rustango in the user's browser.
///
/// The URL is always printed; the browser launch is skipped under `cfg!(test)`
/// and when `RUSTANGO_NO_BROWSER` is set. Without the test guard this verb
/// really did open a browser during `cargo test`: `docs` is in the pool-free
/// verb list that `pool_free_verbs_need_no_database` dispatches, so every
/// `cargo test --lib` spawned a tab. `RUSTANGO_NO_BROWSER` covers the same
/// nuisance for headless and CI shells, which have nothing to open.
pub(super) fn docs_cmd<W: Write>(w: &mut W) -> Result<(), MigrateError> {
    let url = "https://docs.rs/rustango";
    writeln!(w, "{url}")?;
    if cfg!(test) || std::env::var_os("RUSTANGO_NO_BROWSER").is_some() {
        return Ok(());
    }
    // Best-effort — don't fail if the OS has no `open` / `xdg-open` / `start`
    if let Some((cmd, args)) = browser_opener(std::env::consts::OS, url) {
        let _ = std::process::Command::new(cmd).args(args).spawn();
    }
    Ok(())
}

/// The command that opens `url` in a browser on `os`. `start` takes its
/// first quoted argument as a window title, hence the empty one.
/// `cmd` splits an unquoted `&`, so `url` must not contain one.
pub(super) fn browser_opener<'a>(os: &str, url: &'a str) -> Option<(&'static str, Vec<&'a str>)> {
    match os {
        "macos" => Some(("open", vec![url])),
        "linux" => Some(("xdg-open", vec![url])),
        "windows" => Some(("cmd", vec!["/C", "start", "", url])),
        _ => None,
    }
}

/// `manage version` — print the rustango framework version.
pub(super) fn version_cmd<W: Write>(w: &mut W) -> Result<(), MigrateError> {
    writeln!(w, "rustango {}", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
