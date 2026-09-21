//! The layered loader: `default.toml`, then `{env}.toml`, then
//! `RUSTANGO__*` env vars, then a typed [`Settings`].
//!
//! The merge is hand-written rather than pulled from a crate, so the
//! only new dependency is `toml`. Three steps:
//!
//! 1. Parse each file into a `toml::Value` tree.
//! 2. Merge the env file over the defaults; the env file wins.
//! 3. Graft `RUSTANGO__SECTION__KEY=…` values into that tree,
//!    splitting the name on `__` to get the path.
//!
//! The tree is then deserialized into [`Settings`], so a type
//! mismatch names the field that caused it.

use std::path::Path;

use super::sections::Settings;

/// Errors the layered loader can raise.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `config/default.toml` could not be read. The per-env file is
    /// optional and is skipped when missing; this one is required.
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

    /// The merged tree did not fit [`Settings`], usually a type
    /// mismatch such as `[database].pool_max_size = "ten"`.
    #[error("config: settings shape mismatch: {0}")]
    Shape(toml::de::Error),

    /// A `RUSTANGO__*` env var did not parse into the field it
    /// targets, for example a malformed integer.
    #[error("config: env-var override `{var}` invalid: {detail}")]
    EnvOverride { var: String, detail: String },
}

pub(super) fn load_with_root(root: &Path, env: &str) -> Result<Settings, ConfigError> {
    load_with_root_and_env(root, env, std::env::vars())
}

/// Same as `load_with_root`, but the caller passes the env vars in.
/// Unit tests need that, because the workspace forbids
/// `std::env::set_var`.
///
/// Layers, in order. Only the first is required:
/// 1. `default.toml`, the shared defaults.
/// 2. `{env}_settings.toml`, the preferred per-tier name.
/// 3. `{env}.toml`, the older name, still supported.
/// 4. `RUSTANGO__*` env vars.
///
/// If both per-env files exist, `_settings` wins and the loader warns
/// on stderr about the unused one.
fn load_with_root_and_env<I>(root: &Path, env: &str, env_vars: I) -> Result<Settings, ConfigError>
where
    I: IntoIterator<Item = (String, String)>,
{
    let default_path = root.join("default.toml");
    let tiered_path = root.join(format!("{env}_settings.toml"));
    let legacy_path = root.join(format!("{env}.toml"));

    // With `required = true`, `read_toml` returns `Some` or errors,
    // so this expect cannot fire.
    let mut tree =
        read_toml(&default_path, true)?.expect("read_toml(_, required=true) returns Some on Ok");

    let tiered = read_toml(&tiered_path, false)?;
    let legacy = read_toml(&legacy_path, false)?;
    match (tiered, legacy) {
        (Some(t), Some(_)) => {
            // Both exist. `_settings` wins; warn so the unused file
            // does not go unnoticed.
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

/// Read a TOML file. With `required = true` a missing file is an
/// error; otherwise it gives `Ok(None)` so the caller skips the layer.
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

/// Merge `overlay` onto `dst` in place. Tables merge key by key;
/// anything else in `overlay` replaces what `dst` had.
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

/// Graft every `RUSTANGO__*` env var into the tree at its path. The
/// name splits on `__` and is lowercased, so
/// `RUSTANGO__DATABASE__POOL_MAX_SIZE=20` sets
/// `[database].pool_max_size = 20`.
///
/// Each value goes through TOML's own parser, so integers, booleans
/// and strings are detected without writing a lexer here.
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

/// Parse the raw string as a TOML scalar. A dummy key is prepended so
/// the parser sees a valid pair, then the value side is taken.
fn parse_env_value(_var: &str, raw: &str) -> Result<toml::Value, ConfigError> {
    let probe = format!("__rustango_envvar = {raw}");
    if let Ok(parsed) = probe.parse::<toml::Value>() {
        if let toml::Value::Table(t) = parsed {
            if let Some(v) = t.into_iter().next().map(|(_, v)| v) {
                return Ok(v);
            }
        }
    }
    // Otherwise treat it as a string. That covers the common
    // unquoted case, such as `RUSTANGO__DATABASE__URL=postgres://…`.
    Ok(toml::Value::String(raw.to_owned()))
}

/// Put `value` into `tree` at `path`, creating tables along the way.
/// The last segment replaces whatever was there.
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
        // Pass the env vars in, since the workspace forbids
        // `std::env::set_var`.
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
        // Splitting on `__` puts this in [admin].allowed_tables.
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

    /// `prod_settings.toml` is found and overrides the defaults.
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

    /// The older `<env>.toml` still loads when no `_settings` file
    /// exists.
    #[test]
    fn legacy_filename_still_loads() {
        let root = fresh_root("legacy");
        write(&root, "default.toml", "[database]\npool_max_size = 5\n");
        write(&root, "prod.toml", "[database]\npool_max_size = 25\n");
        let cfg = load_with_root(&root, "prod").unwrap();
        assert_eq!(cfg.database.pool_max_size, Some(25));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// When both files exist, `_settings` wins.
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

    /// The server, auth, brand, security, routes and audit sections
    /// all parse from one file, and merging leaves the older sections
    /// alone.
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
        // The older section is untouched.
        assert_eq!(
            cfg.database.url.as_deref(),
            Some("postgres://localhost/dev")
        );
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

    /// `Settings::detected_features` reports the compiled-in
    /// features. The lib-test build turns on every default feature,
    /// so the list is not empty and includes postgres.
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
