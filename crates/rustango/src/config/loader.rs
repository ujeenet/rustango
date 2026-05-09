//! TOML layered-loader pipeline: `default.toml` → `{env}.toml` →
//! `RUSTANGO__*` env-var overrides → typed [`Settings`].
//!
//! Hand-rolled merger (no figment) — the workspace already pulls
//! `serde` and we add only `toml = "0.8"` for parsing. Three steps:
//!
//! 1. Parse each TOML file to `toml::Value` (tree-shape).
//! 2. Recursively merge env file on top of defaults (env wins).
//! 3. Apply `RUSTANGO__SECTION__KEY=…` env-var overrides to the
//!    merged tree, splitting on `__` for the path.
//!
//! Then `serde_path_to_error` style deserialise to [`Settings`] so
//! field-shape mismatches surface with the offending path.

use std::path::Path;

use super::sections::Settings;

/// Errors the layered loader can raise.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `config/default.toml` couldn't be opened or read. The
    /// env-specific overlay is optional (skipped silently when
    /// missing); the default file is the contract.
    #[error("config: failed to read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// TOML syntax error somewhere in the merged tree.
    #[error("config: failed to parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    /// Final deserialise into [`Settings`] failed — usually a type
    /// mismatch (e.g. `[database].pool_max_size = "ten"`).
    #[error("config: settings shape mismatch: {0}")]
    Shape(toml::de::Error),

    /// A `RUSTANGO__*` env var couldn't be parsed into the path it
    /// targets (e.g. malformed integer).
    #[error("config: env-var override `{var}` invalid: {detail}")]
    EnvOverride { var: String, detail: String },
}

pub(super) fn load_with_root(root: &Path, env: &str) -> Result<Settings, ConfigError> {
    load_with_root_and_env(root, env, std::env::vars())
}

/// Test-friendly variant: caller supplies the env-var iterator
/// instead of reading the process environment. Avoids
/// `std::env::set_var` (which requires `unsafe` and is forbidden by
/// the workspace lint policy) in unit tests.
///
/// File-search order (each layer is optional except the first):
/// 1. `default.toml` — shared defaults, REQUIRED.
/// 2. `{env}_settings.toml` — tier convention (`dev_settings.toml`,
///    `staging_settings.toml`, `prod_settings.toml`), preferred since
///    v0.29 (#87) because the suffix makes the file's purpose clear
///    next to `default.toml`.
/// 3. `{env}.toml` — legacy convention (`prod.toml`, `staging.toml`),
///    kept for back-compat with v0.28 deployments.
/// 4. `RUSTANGO__*` env-var overrides.
///
/// If both `{env}_settings.toml` and `{env}.toml` exist, the `_settings`
/// variant wins — same shape as a more-specific override beating a
/// less-specific one, and the loader emits a stderr warning.
fn load_with_root_and_env<I>(root: &Path, env: &str, env_vars: I) -> Result<Settings, ConfigError>
where
    I: IntoIterator<Item = (String, String)>,
{
    let default_path = root.join("default.toml");
    let tiered_path = root.join(format!("{env}_settings.toml"));
    let legacy_path = root.join(format!("{env}.toml"));

    // `read_toml(path, required = true)` returns Some(_) or errors,
    // so this expect can never trip.
    let mut tree =
        read_toml(&default_path, true)?.expect("read_toml(_, required=true) returns Some on Ok");

    let tiered = read_toml(&tiered_path, false)?;
    let legacy = read_toml(&legacy_path, false)?;
    match (tiered, legacy) {
        (Some(t), Some(_)) => {
            // Both exist — _settings wins. Warn so the user notices
            // the dead file (typically left from a mid-migration v0.28
            // deployment that didn't clean up).
            eprintln!(
                "config: both {} and {} exist — using the `_settings` variant; \
                 delete the legacy file to silence this warning",
                tiered_path.display(),
                legacy_path.display(),
            );
            merge(&mut tree, t);
        }
        (Some(t), None) => merge(&mut tree, t),
        (None, Some(l)) => merge(&mut tree, l),
        (None, None) => {}
    }
    apply_env_overrides(&mut tree, env_vars)?;

    let settings: Settings = tree.try_into().map_err(ConfigError::Shape)?;
    Ok(settings)
}

/// Read a TOML file. `required = true` errors when the file is
/// missing; `required = false` returns `Ok(None)` so the caller can
/// skip the overlay layer cleanly.
fn read_toml(path: &Path, required: bool) -> Result<Option<toml::Value>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let value = s
                .parse::<toml::Value>()
                .map_err(|source| ConfigError::Parse {
                    path: path.display().to_string(),
                    source,
                })?;
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Ok(None),
        Err(e) => Err(ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        }),
    }
}

/// Recursively merge `overlay` onto `dst` in place. Tables merge key
/// by key; non-table values from `overlay` replace `dst` outright.
fn merge(dst: &mut toml::Value, overlay: toml::Value) {
    match (dst, overlay) {
        (toml::Value::Table(d), toml::Value::Table(o)) => {
            for (k, v) in o {
                match d.get_mut(&k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        d.insert(k, v);
                    }
                }
            }
        }
        (slot, other) => {
            *slot = other;
        }
    }
}

/// Walk `RUSTANGO__*` env vars and graft each one's value into the
/// TOML tree at the matching path. Path segments split on `__`,
/// lowercased, so `RUSTANGO__DATABASE__POOL_MAX_SIZE=20` → `tree`
/// gets `[database].pool_max_size = 20`.
///
/// Values are coerced through TOML's parser by writing
/// `key = "raw_string_value"` and parsing it as a 1-line TOML — this
/// gives us automatic type detection (integer, bool, string) without
/// us reinventing TOML's lexer.
fn apply_env_overrides<I>(tree: &mut toml::Value, env_vars: I) -> Result<(), ConfigError>
where
    I: IntoIterator<Item = (String, String)>,
{
    const PREFIX: &str = "RUSTANGO__";
    for (var, raw) in env_vars {
        let Some(rest) = var.strip_prefix(PREFIX) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let path: Vec<String> = rest
            .split("__")
            .map(|seg| seg.to_ascii_lowercase())
            .collect();
        let value = parse_env_value(&var, &raw)?;
        graft(tree, &path, value);
    }
    Ok(())
}

/// Parse the env var's raw string as a TOML scalar (int/float/bool/
/// string). We prepend `__rustango_envvar = ` so the parser sees a
/// well-formed key-value pair, then pull out the value side.
fn parse_env_value(_var: &str, raw: &str) -> Result<toml::Value, ConfigError> {
    let probe = format!("__rustango_envvar = {raw}");
    if let Ok(parsed) = probe.parse::<toml::Value>() {
        if let toml::Value::Table(t) = parsed {
            if let Some(v) = t.into_iter().next().map(|(_, v)| v) {
                return Ok(v);
            }
        }
    }
    // Fallback: treat as a string — covers the common case where the
    // user wrote `RUSTANGO__DATABASE__URL=postgres://…` (unquoted).
    Ok(toml::Value::String(raw.to_owned()))
}

/// Graft `value` into `tree` at `path`, creating intermediate tables
/// as needed. Last segment replaces whatever was there.
fn graft(tree: &mut toml::Value, path: &[String], value: toml::Value) {
    if path.is_empty() {
        *tree = value;
        return;
    }
    if !matches!(tree, toml::Value::Table(_)) {
        *tree = toml::Value::Table(toml::value::Table::new());
    }
    let toml::Value::Table(t) = tree else {
        unreachable!()
    };
    let head = &path[0];
    if path.len() == 1 {
        t.insert(head.clone(), value);
    } else {
        let entry = t
            .entry(head.clone())
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
        graft(entry, &path[1..], value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn fresh_root(label: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let mut p = std::env::temp_dir();
        p.push(format!("rustango_config_{label}_{pid}_{n}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(root: &std::path::Path, name: &str, body: &str) {
        std::fs::write(root.join(name), body).unwrap();
    }

    #[test]
    fn loads_default_only() {
        let root = fresh_root("default_only");
        write(
            &root,
            "default.toml",
            "[database]\nurl = \"postgres://localhost/dev\"\n",
        );
        let cfg = load_with_root(&root, "missing-env").unwrap();
        assert_eq!(
            cfg.database.url.as_deref(),
            Some("postgres://localhost/dev")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn env_file_overlays_default() {
        let root = fresh_root("overlay");
        write(
            &root,
            "default.toml",
            "[database]\nurl = \"postgres://localhost/dev\"\npool_max_size = 5\n",
        );
        write(&root, "prod.toml", "[database]\npool_max_size = 50\n");
        let cfg = load_with_root(&root, "prod").unwrap();
        assert_eq!(
            cfg.database.url.as_deref(),
            Some("postgres://localhost/dev"),
            "default url survives"
        );
        assert_eq!(cfg.database.pool_max_size, Some(50), "env file wins");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn env_var_overrides_file() {
        let root = fresh_root("env_var");
        write(
            &root,
            "default.toml",
            "[database]\nurl = \"postgres://from-file\"\n",
        );
        // Use the test-friendly variant that takes a mock env-var
        // iterator — avoids `std::env::set_var` (unsafe in 2024
        // edition) and the workspace `unsafe_code = forbid` lint.
        let mock_env = vec![(
            "RUSTANGO__DATABASE__URL".to_owned(),
            "postgres://from-env".to_owned(),
        )];
        let cfg = load_with_root_and_env(&root, "missing-env", mock_env).unwrap();
        assert_eq!(
            cfg.database.url.as_deref(),
            Some("postgres://from-env"),
            "env var beats file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn env_var_typed_int() {
        let root = fresh_root("typed_int");
        write(&root, "default.toml", "[database]\npool_max_size = 5\n");
        let mock_env = vec![(
            "RUSTANGO__DATABASE__POOL_MAX_SIZE".to_owned(),
            "42".to_owned(),
        )];
        let cfg = load_with_root_and_env(&root, "missing-env", mock_env).unwrap();
        assert_eq!(cfg.database.pool_max_size, Some(42));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_section_via_env_var() {
        let root = fresh_root("nested");
        write(&root, "default.toml", "[admin]\n");
        // The loader splits on `__` and lowercases — so
        // RUSTANGO__ADMIN__ALLOWED_TABLES grafts into [admin].allowed_tables.
        let mock_env = vec![(
            "RUSTANGO__ADMIN__ALLOWED_TABLES".to_owned(),
            r#"["user", "post"]"#.to_owned(),
        )];
        let cfg = load_with_root_and_env(&root, "missing-env", mock_env).unwrap();
        assert_eq!(
            cfg.admin.allowed_tables,
            vec!["user".to_owned(), "post".to_owned()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_default_errors() {
        let root = fresh_root("missing_default");
        let err = load_with_root(&root, "any").unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Tier convention: `prod_settings.toml` is found before the
    /// legacy `prod.toml` shape and overrides default settings.
    #[test]
    fn tiered_settings_filename_loads() {
        let root = fresh_root("tiered");
        write(
            &root,
            "default.toml",
            "[database]\nurl = \"postgres://localhost/dev\"\npool_max_size = 5\n",
        );
        write(
            &root,
            "prod_settings.toml",
            "[database]\npool_max_size = 100\n",
        );
        let cfg = load_with_root(&root, "prod").unwrap();
        assert_eq!(cfg.database.pool_max_size, Some(100));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Legacy `<env>.toml` still loads when no `_settings` variant
    /// exists — keeps v0.28 deployments working untouched.
    #[test]
    fn legacy_filename_still_loads() {
        let root = fresh_root("legacy");
        write(&root, "default.toml", "[database]\npool_max_size = 5\n");
        write(&root, "prod.toml", "[database]\npool_max_size = 25\n");
        let cfg = load_with_root(&root, "prod").unwrap();
        assert_eq!(cfg.database.pool_max_size, Some(25));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Both files present: `_settings` wins. Mirrors how a more
    /// specific override beats a less specific one — and the loader
    /// emits a stderr warning so the dead legacy file is visible.
    #[test]
    fn both_filenames_present_settings_wins() {
        let root = fresh_root("both");
        write(&root, "default.toml", "[database]\npool_max_size = 5\n");
        write(&root, "prod.toml", "[database]\npool_max_size = 25\n");
        write(
            &root,
            "prod_settings.toml",
            "[database]\npool_max_size = 100\n",
        );
        let cfg = load_with_root(&root, "prod").unwrap();
        assert_eq!(
            cfg.database.pool_max_size,
            Some(100),
            "_settings variant must win when both exist"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Section coverage (#87 slice 2): the new server/auth/brand/
    /// security/routes/audit sections parse cleanly from a single
    /// TOML and survive default-section merging without touching
    /// pre-existing sections.
    #[test]
    fn slice2_sections_parse_and_merge() {
        let root = fresh_root("slice2");
        write(
            &root,
            "default.toml",
            r##"
[database]
url = "postgres://localhost/dev"

[server]
bind = "127.0.0.1:9000"
request_timeout_secs = 30

[auth]
argon2_memory_kib = 19456
argon2_iterations = 2
lockout_threshold = 5

[auth.jwt]
access_ttl_secs = 900
refresh_ttl_secs = 604800
issuer = "myapp"

[brand]
name = "Acme Operator"
primary_color = "#2c6fb0"

[security]
headers_preset = "strict"
hsts_max_age_secs = 31536000

[routes]
admin_url = "/admin"
login_url = "/login"

[audit]
retention_days = 90
"##,
        );
        let cfg = load_with_root(&root, "missing-env").unwrap();
        // Pre-existing section unaffected.
        assert_eq!(
            cfg.database.url.as_deref(),
            Some("postgres://localhost/dev")
        );
        // New sections.
        assert_eq!(cfg.server.bind.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(cfg.server.request_timeout_secs, Some(30));
        assert_eq!(cfg.auth.argon2_memory_kib, Some(19456));
        assert_eq!(cfg.auth.lockout_threshold, Some(5));
        assert_eq!(cfg.auth.jwt.access_ttl_secs, Some(900));
        assert_eq!(cfg.auth.jwt.issuer.as_deref(), Some("myapp"));
        assert_eq!(cfg.brand.name.as_deref(), Some("Acme Operator"));
        assert_eq!(cfg.brand.primary_color.as_deref(), Some("#2c6fb0"));
        assert_eq!(cfg.security.headers_preset.as_deref(), Some("strict"));
        assert_eq!(cfg.security.hsts_max_age_secs, Some(31536000));
        assert_eq!(cfg.routes.admin_url.as_deref(), Some("/admin"));
        assert_eq!(cfg.routes.login_url.as_deref(), Some("/login"));
        assert_eq!(cfg.audit.retention_days, Some(90));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `Settings::detected_features` reflects compile-time feature
    /// flags. The lib-test build pulls every default feature, so
    /// the list is non-empty and includes a few we know are on
    /// (postgres, tenancy, admin).
    #[test]
    fn detected_features_lists_compiled_in_features() {
        use crate::config::Settings;
        let feats = Settings::detected_features();
        assert!(!feats.is_empty(), "expected at least one feature");
        assert!(
            feats.contains(&"postgres"),
            "postgres feature is in the default set; got {feats:?}"
        );
    }

    #[test]
    fn parse_error_includes_path() {
        let root = fresh_root("parse_error");
        write(&root, "default.toml", "this is = not [valid toml");
        let err = load_with_root(&root, "any").unwrap_err();
        match err {
            ConfigError::Parse { path, .. } => assert!(path.contains("default.toml")),
            other => panic!("expected ConfigError::Parse, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
