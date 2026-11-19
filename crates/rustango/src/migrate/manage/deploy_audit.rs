//! `check --deploy` and `check --settings`.

use super::*;

/// Snapshot of the env vars `manage check --deploy` cares about.
/// Lifted out so the audit logic is pure (testable without
/// `unsafe { env::set_var }` race conditions across `cargo test`'s
/// parallel runners). No `Debug`: it holds the session secret.
#[derive(Clone)]
pub(crate) struct DeployAuditEnv {
    pub rustango_env: Option<String>,
    pub session_secret: Option<String>,
    /// The resolver's URL, or its error text (never holds a URL).
    pub database_url: Result<String, String>,
    /// `DATABASE_URL` and `DB_*` both set: the resolver's warning.
    pub database_style_conflict: Option<String>,
    pub apex_domain: ApexDomain,
    pub bind: Option<String>,
    /// An admin was built without session auth (#1627).
    pub ungated_admin: bool,
}

/// The apex the deploy audit checks; only a tenancy Cli has one.
#[derive(Default, Clone)]
pub(crate) enum ApexDomain {
    #[default]
    SingleTenant,
    /// `RUSTANGO_APEX_DOMAIN` (or settings) as configured, if at all.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Tenancy(Option<String>),
}

pub(super) fn deploy_audit_env(tenancy: bool) -> DeployAuditEnv {
    DeployAuditEnv {
        rustango_env: std::env::var("RUSTANGO_ENV").ok(),
        session_secret: std::env::var("RUSTANGO_SESSION_SECRET").ok(),
        database_url: crate::env::database_url_from_env().map_err(|e| e.to_string()),
        database_style_conflict: crate::env::database_style_conflict(),
        #[cfg(feature = "tenancy")]
        apex_domain: if tenancy {
            ApexDomain::Tenancy(crate::tenancy::server::configured_apex_domain())
        } else {
            ApexDomain::SingleTenant
        },
        #[cfg(not(feature = "tenancy"))]
        apex_domain: {
            let _ = tenancy;
            ApexDomain::SingleTenant
        },
        bind: std::env::var("RUSTANGO_BIND").ok(),
        #[cfg(feature = "admin")]
        ungated_admin: crate::admin::ungated_admin_built(),
        #[cfg(not(feature = "admin"))]
        ungated_admin: false,
    }
}

/// Triple-bucket output of the `manage check --deploy` audit
/// pipeline. `info` rows are informational, `warnings` are
/// dev-default-leak flags, `errors` are hard misconfigurations.
///
/// `#[doc(hidden)] pub` for integration-test access; library
/// callers use [`run_settings_audit`] / `check_cmd` directly.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct DeployAuditFindings {
    pub info: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

pub(super) const APEX_UNSET: &str =
    "RUSTANGO_APEX_DOMAIN is unset / `localhost` — tenancy projects need \
    the public-facing apex (e.g. `app.example.com`) so subdomain resolution + cookie scoping \
    work in production.";

/// What `RUSTANGO_SESSION_SECRET` signs in this build.
#[cfg(feature = "tenancy")]
pub(super) const SIGNED_BY_SESSION_SECRET: &str =
    "session cookies (operator and tenant ones included) and JWTs";
#[cfg(not(feature = "tenancy"))]
pub(super) const SIGNED_BY_SESSION_SECRET: &str = "session cookies and JWTs";

/// Run the `manage check --deploy` audit checks against `env`.
/// Pure function so callers can test it with arbitrary env
/// snapshots without poking actual process env vars.
///
/// Modernized from the v0.27 shape that checked the wrong env
/// var (`SECRET_KEY` — never read by the framework). The
/// framework reads `RUSTANGO_SESSION_SECRET` for HMAC-signing
/// the operator-console + tenant-admin cookies AND the JWT
/// payloads issued by `auth_routes::JwtAuth` (#81). Same key
/// covers both surfaces.
pub(crate) fn run_deploy_audit(env: &DeployAuditEnv, out: &mut DeployAuditFindings) {
    if env.ungated_admin {
        out.warnings.push(
            "[admin] the admin router has no authentication — anyone can read and write \
             every model. Call `admin::Builder::with_session_auth(secret)`, or gate the \
             route yourself."
                .into(),
        );
    }
    // RUSTANGO_ENV — production should be explicitly tagged.
    match env.rustango_env.as_deref() {
        Some(tier) if crate::env::is_prod_tier(tier) => {
            out.info
                .push("RUSTANGO_ENV is set to a production value".into());
        }
        Some(other) => {
            out.warnings.push(format!(
                "RUSTANGO_ENV is `{other}` — set to `prod` (or `production`) in deployed env"
            ));
        }
        None => {
            out.warnings.push(
                "RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier"
                    .into(),
            );
        }
    }

    // RUSTANGO_SESSION_SECRET — required for cookie + JWT signing.
    // Recommended: 32+ bytes of base64-encoded entropy
    // (`openssl rand -base64 32`). The framework treats the raw
    // value as the HMAC key, so length matters.
    match env.session_secret.as_deref() {
        None => {
            out.errors.push(format!(
                "RUSTANGO_SESSION_SECRET is unset — on the prod tier the server will not \
                 start; off prod, {SIGNED_BY_SESSION_SECRET} use the dev key in ./var/, which \
                 a redeploy loses. Set via `openssl rand -base64 32`."
            ));
        }
        Some(s) if s.contains("change-me") || s.contains("placeholder") => {
            out.errors.push(
                "RUSTANGO_SESSION_SECRET still contains the scaffolder placeholder \
                 (`change-me-...`) — replace with a real secret via `openssl rand -base64 32`."
                    .into(),
            );
        }
        // Decode it the way the runtime does, rather than measuring the
        // encoded string (#1396). A 32-character base64 secret is 24
        // bytes of key: the old `s.len() >= 32` reported "length OK" for
        // a value the cookie layer then refused, so a green check meant
        // nothing about whether the app would come up with sessions.
        #[cfg(any(feature = "admin", feature = "tenancy"))]
        Some(s) => match crate::session::SessionSecret::from_b64(s) {
            Ok(_) => out
                .info
                .push("RUSTANGO_SESSION_SECRET decodes to a valid ≥32-byte key".into()),
            Err(e) => out.errors.push(format!(
                "{e}. This is the same check the runtime applies, so the app would \
                 fall back to an ephemeral key and sign everyone out on restart."
            )),
        },
        // `session` is gated on `admin` / `tenancy`, and so is the base64
        // crate it decodes with. In a build with neither there is no
        // cookie layer and no JWT router, so nothing signs with this
        // value — reporting it unvalidated is the honest answer, and
        // asserting a length here would be the encoded-string mistake
        // #1396 removed.
        #[cfg(not(any(feature = "admin", feature = "tenancy")))]
        Some(_) => out.info.push(
            "RUSTANGO_SESSION_SECRET is set but not validated — this build has \
             neither `admin` nor `tenancy`, so nothing in it signs cookies or JWTs."
                .into(),
        ),
    }

    // DATABASE_URL — required.
    match env.database_url.as_deref() {
        Err(e) => out.errors.push(format!("no usable database URL: {e}")),
        Ok(url) if url.contains("localhost") || url.contains("127.0.0.1") => {
            out.warnings.push(
                "DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in \
                 production (typically a managed service hostname)"
                    .into(),
            );
        }
        Ok(_) => out.info.push("DATABASE_URL set".into()),
    }
    if let Some(conflict) = &env.database_style_conflict {
        out.warnings.push(conflict.clone());
    }

    // A single-database deploy needs no apex.
    match &env.apex_domain {
        ApexDomain::SingleTenant => {}
        ApexDomain::Tenancy(None) => out.warnings.push(APEX_UNSET.into()),
        ApexDomain::Tenancy(Some(d)) if d == "localhost" => out.warnings.push(APEX_UNSET.into()),
        ApexDomain::Tenancy(Some(_)) => out
            .info
            .push("RUSTANGO_APEX_DOMAIN set to a non-localhost value".into()),
    }

    // RUSTANGO_BIND — warn if loopback-only. Common typo in dev
    // configs that get promoted to prod without rebinding.
    match env.bind.as_deref() {
        Some(b) if b.starts_with("127.0.0.1") => {
            out.warnings.push(format!(
                "RUSTANGO_BIND={b} only listens on loopback — production usually wants \
                 `0.0.0.0:<port>` to accept external traffic"
            ));
        }
        Some(_) | None => {} // either explicit non-loopback or framework default (0.0.0.0)
    }
}

/// Login security state that each replica keeps to itself (#1534).
#[cfg(feature = "cache")]
pub(crate) fn login_store_audit(
    lockout: &crate::account_lockout::Lockout,
    out: &mut DeployAuditFindings,
) {
    if lockout.stores_nothing() {
        out.warnings
            .push(crate::account_lockout::NULL_STORE_WARNING.into());
    } else if lockout.is_process_local() {
        out.info
            .push(crate::account_lockout::CHECK_DEPLOY_NOTE.into());
    }
    #[cfg(feature = "admin")]
    out.info
        .push(crate::login_throttle::PROCESS_LOCAL_NOTE.into());
}

/// `manage check --deploy` settings-side audit (#87 slice 4) —
/// loads `Settings::load_from_env()` and flags dev-defaults left in
/// the prod tier. Pure function over the loaded settings + resolved
/// tier, no env-var poking.
///
/// A missing `config/default.toml` warns on the prod tier only (the
/// project might not use the layered loader at all). Bad TOML
/// shape surfaces as a warning so the operator notices but the
/// rest of the audit still runs.
#[cfg(feature = "config")]
pub(super) fn run_settings_audit(out: &mut DeployAuditFindings) {
    use crate::config::Settings;

    let env_tier = Settings::current_env_tier();
    out.info
        .push(format!("config tier resolved to `{env_tier}`"));
    settings_audit_loaded(&env_tier, Settings::load_from_env(), out);
}

/// `check --settings`: where each setting comes from, values of secrets hidden.
pub(super) fn check_settings_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    if args.len() != 1 {
        return Err(MigrateError::Validation(
            "check --settings takes no other arguments".into(),
        ));
    }
    #[cfg(feature = "config")]
    {
        let tier = crate::config::Settings::current_env_tier();
        let loaded = crate::config::report(Path::new("config"), &tier);
        write_settings_report(w, &tier, loaded, &|n| std::env::var(n).ok())
    }
    #[cfg(not(feature = "config"))]
    {
        let _ = w;
        Err(MigrateError::Validation(
            "check --settings needs the `config` feature".into(),
        ))
    }
}

/// [`check_settings_cmd`] on a load result and an env lookup.
#[cfg(feature = "config")]
pub(super) fn write_settings_report<W: Write>(
    w: &mut W,
    tier: &str,
    loaded: Result<crate::config::Loaded, crate::config::ConfigError>,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<(), MigrateError> {
    use crate::config::registry::{Reader, KEYS};
    use crate::env::SettingSource;

    let loaded = match loaded {
        Ok(l) => Some(l),
        Err(e) if e.is_missing_config() => None,
        Err(e) => return Err(MigrateError::Validation(e.to_string())),
    };
    writeln!(w, "settings for tier `{tier}`:")?;
    if loaded.is_none() {
        writeln!(w, "  (no config/default.toml: only env vars apply)")?;
    }
    let blank = |v: &String| v.trim().is_empty();
    let file_source = |path: &str| -> SettingSource {
        let Some(l) = &loaded else {
            return SettingSource::Default;
        };
        l.sources
            .iter()
            .filter(|(k, _)| crate::config::registry::find(k).is_some_and(|r| r.path == path))
            .map(|(_, s)| s.clone())
            .max_by_key(|s| matches!(s, SettingSource::Override(_)))
            .unwrap_or(SettingSource::Default)
    };
    for key in KEYS {
        let mut missing = None;
        let source = if key.path == "database.url" {
            let file_url = loaded
                .as_ref()
                .and_then(|l| l.settings.database.url.clone());
            match crate::env::resolve_database_url_from(get, file_url.as_deref()) {
                Ok((_, SettingSource::File(_))) => file_source(key.path),
                Ok((_, src)) => src,
                Err(e) => {
                    let kind = match e {
                        crate::env::EnvError::MissingRequired { .. } => "MISSING (required)",
                        _ => "INVALID",
                    };
                    missing = Some(format!("{kind}: {e}"));
                    SettingSource::Default
                }
            }
        } else {
            match key.env.iter().find(|v| get(v).is_some_and(|x| !blank(&x))) {
                Some(var) => SettingSource::Env(var),
                None => file_source(key.path),
            }
        };
        let set = source != SettingSource::Default;
        let value = match (&source, set && key.secret) {
            (_, true) => " = <hidden>".to_owned(),
            (SettingSource::Env(var), false) => {
                format!(" = {}", get(var).unwrap_or_default())
            }
            (SettingSource::Default, false) => String::new(),
            (_, false) => loaded
                .as_ref()
                .and_then(|l| key.path.split('.').try_fold(&l.tree, |v, seg| v.get(seg)))
                .map(|v| format!(" = {}", toml_text(v)))
                .unwrap_or_default(),
        };
        let note = match (key.reader, set) {
            (_, false) if missing.is_some() => format!("  {}", missing.unwrap_or_default()),
            (Reader::App(by), true) => format!("  (read only by {by})"),
            (Reader::Tenancy, true) => "  (tenancy apps only)".to_owned(),
            _ => String::new(),
        };
        writeln!(w, "  {:<40} {source}{value}{note}", key.path)?;
    }
    Ok(())
}

/// A settings value as one line; strings unquoted.
#[cfg(feature = "config")]
pub(super) fn toml_text(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(toml_text).collect();
            format!("[{}]", items.join(", "))
        }
        toml::Value::Table(_) => "{…}".to_owned(),
    }
}

/// [`run_settings_audit`] for a load result, so tests skip the disk.
#[cfg(feature = "config")]
pub(super) fn settings_audit_loaded(
    env_tier: &str,
    loaded: Result<crate::config::Settings, crate::config::ConfigError>,
    out: &mut DeployAuditFindings,
) {
    let settings = match loaded {
        Ok(s) => s,
        Err(e) if e.is_missing_config() => {
            if crate::env::is_prod_tier(env_tier) {
                out.warnings.push(
                    "config/default.toml is missing in prod tier — no [security] settings \
                     (allowed_hosts, headers, SSL redirect) are applied"
                        .into(),
                );
            }
            return;
        }
        Err(e) => {
            out.warnings.push(format!(
                "config: failed to load settings for audit: {e} — fix the file shape \
                 to enable the rest of the deploy audit"
            ));
            return;
        }
    };

    settings_audit_check(&env_tier, &settings, out);
}

/// Pure-function half of [`run_settings_audit`] — caller supplies
/// the resolved tier + settings. Lifted out so unit tests can run
/// against any combination without touching the on-disk config
/// pipeline (which would race against parallel test runners).
///
/// `#[doc(hidden)] pub` so integration tests (which compile against
/// the crate as an external dep) can drive it without flipping
/// every field — internal callers should keep using
/// [`run_settings_audit`].
#[cfg(feature = "config")]
#[doc(hidden)]
pub fn settings_audit_check(
    env_tier: &str,
    settings: &crate::config::Settings,
    out: &mut DeployAuditFindings,
) {
    let in_prod = crate::env::is_prod_tier(env_tier);
    if !in_prod {
        // Non-prod tiers don't need the dev-default-leak audit.
        return;
    }

    // [security] headers_preset — must be `strict` (or unset →
    // strict by default at the layer). `"dev"` / `"none"` in prod
    // strips HSTS / XFO / nosniff / Referrer-Policy.
    // Same names as `SecurityHeadersLayer::from_preset`.
    match settings.security.headers_preset.as_deref() {
        None | Some("strict" | "relaxed") => {}
        Some(preset @ ("dev" | "none" | "empty")) => out.warnings.push(format!(
            "[security] headers_preset = `{preset}` in prod tier — promote to `strict` \
             so HSTS / X-Frame-Options / X-Content-Type-Options / Referrer-Policy are emitted"
        )),
        Some(preset) => out.warnings.push(format!(
            "[security] headers_preset = `{preset}` is not `strict`, `relaxed`, `dev` or \
             `none`; the layer falls back to `strict`. Fix the spelling."
        )),
    }

    if settings.security.secure_cookies == Some(false) {
        out.warnings.push(
            "[security] secure_cookies = false in prod tier — session and CSRF cookies are \
             sent over plain HTTP too. Remove it or set it to true."
                .into(),
        );
    }

    // [security] hsts_max_age_secs = 0 in prod disables HSTS.
    if matches!(settings.security.hsts_max_age_secs, Some(0)) {
        out.warnings.push(
            "[security] hsts_max_age_secs = 0 in prod tier — disables HSTS, leaving TLS-strip \
             attacks viable on first request"
                .into(),
        );
    }

    // [security] allowed_hosts empty in prod → host_validation
    // middleware passes every Host header (DEBUG-style opt-out).
    // Operators on the prod tier want host validation; flag it.
    if settings.security.allowed_hosts.is_empty() {
        out.warnings.push(
            "[security] allowed_hosts = [] in prod tier — `host_validation::AllowedHostsLayer` \
             passes every Host header (no DisallowedHost protection). Populate with the canonical \
             host + subdomain wildcards before deploying."
                .into(),
        );
    } else if settings
        .security
        .allowed_hosts
        .iter()
        .any(|h| h.trim() == "*")
    {
        out.warnings.push(
            "[security] allowed_hosts contains \"*\" in prod tier — every Host header passes, \
             so host validation is off. List the real hosts instead."
                .into(),
        );
    }

    if settings
        .security
        .cors_allowed_origins
        .iter()
        .any(|o| o.trim() == "*")
    {
        out.warnings.push(
            "[security] cors_allowed_origins contains \"*\" in prod tier — any site can read \
             non-credentialed responses. List the real origins."
                .into(),
        );
    }

    // Both layers are opt-in: unset installs no layer.
    if matches!(settings.server.request_timeout_secs, None | Some(0)) {
        out.warnings.push(
            "[server] request_timeout_secs unset or 0 in prod tier — no handler timeout, so \
             stuck requests hold workers. Set about 30."
                .into(),
        );
    }
    if settings.server.max_body_bytes.is_none() {
        out.info.push(
            "[server] max_body_bytes unset in prod tier — no body-limit layer; only axum's \
             2 MiB extractor default applies, and streamed bodies are unlimited."
                .into(),
        );
    }

    // Empty list = same-host only (#1529); a cross-host SPA's POSTs get 403.
    if settings.security.csrf_trusted_origins.is_empty() {
        out.info.push(
            "[security] csrf_trusted_origins = [] in prod tier — the CSRF Origin check accepts \
             only the request's own Host. Add any SPA or other origin that POSTs cross-host, \
             or its requests get 403."
                .into(),
        );
    }

    // [security] secure_ssl_redirect false in prod tier — plain-HTTP
    // requests aren't redirected to HTTPS. Worth flagging unless
    // the operator runs entirely behind a TLS-terminating LB that
    // already enforces this (common, hence "warning" not "error").
    if matches!(settings.security.secure_ssl_redirect, Some(false) | None) {
        out.info.push(
            "[security] secure_ssl_redirect unset/false in prod tier — plain-HTTP requests \
             aren't redirected. If your LB doesn't already enforce HTTPS, mount \
             `ssl_redirect::SslRedirectLayer` and set this true (configure `secure_proxy_ssl_header` \
             to avoid loops)."
                .into(),
        );
    }

    // [security] secure_proxy_ssl_header malformed (not length-2) →
    // ssl_redirect layer silently drops the trusted-header pair and
    // redirect-loops behind the LB.
    if !settings.security.secure_proxy_ssl_header.is_empty()
        && settings.security.secure_proxy_ssl_header.len() != 2
    {
        out.warnings.push(format!(
            "[security] secure_proxy_ssl_header has {} entries — expected exactly 2 \
             ([header_name, expected_value]); ssl_redirect layer will drop the trusted-header \
             pair and redirect-loop behind the LB",
            settings.security.secure_proxy_ssl_header.len()
        ));
    }

    // [auth] argon2 memory cost — OWASP 2024 floor is 19456 KiB.
    if let Some(kib) = settings.auth.argon2_memory_kib {
        if kib < 19_456 {
            out.warnings.push(format!(
                "[auth] argon2_memory_kib = {kib} in prod tier — OWASP 2024 recommends ≥ 19456 \
                 for password hashing brute-force resistance"
            ));
        }
    }

    // [auth.jwt] access TTL too long. Anything over 1 hour is
    // suspicious — refresh tokens should rotate access tokens.
    if let Some(ttl) = settings.auth.jwt.access_ttl_secs {
        if ttl > 3600 {
            out.warnings.push(format!(
                "[auth.jwt] access_ttl_secs = {ttl} in prod tier — access tokens > 1h widen \
                 the leaked-token blast radius. The refresh flow rotates them; keep this short."
            ));
        }
    }

    // [audit] retention_days unset → log grows forever. Not an
    // error (some compliance regimes mandate forever) but worth
    // info-flagging so the operator decides intentionally.
    if settings.audit.retention_days.is_none() {
        out.info.push(
            "[audit] retention_days unset — log grows forever; set it and schedule \
             `manage audit-cleanup`, which prunes to it"
                .into(),
        );
    }

    // [routes] legacy_preset = true is a deliberate choice (#85) but
    // worth surfacing in audit output so operators rationalize the
    // `/__admin` shape against the current default.
    if matches!(settings.routes.legacy_preset, Some(true)) {
        out.info.push(
            "[routes] legacy_preset = true — using the pre-v0.29 `__`-prefixed URLs \
             (`/__login`, `/__admin`, …). Switch to the friendly defaults when bookmarks allow."
                .into(),
        );
    }

    // Unset means console too: reset and magic-link URLs land in stdout.
    if matches!(settings.mail.backend.as_deref(), None | Some("console")) {
        out.warnings.push(
            "[mail] backend unset or `console` in prod tier — mail bodies, including reset \
             and magic-link URLs, are printed to stdout. Set `smtp` (or `null`)."
                .into(),
        );
    }

    // [server] bind on loopback in prod. The env-var path catches
    // RUSTANGO_BIND; this catches the TOML case that bypasses it.
    if let Some(bind) = settings.server.bind.as_deref() {
        if bind.starts_with("127.0.0.1") || bind.starts_with("localhost") {
            out.warnings.push(format!(
                "[server] bind = `{bind}` in prod tier — only listens on loopback. \
                 Production usually wants `0.0.0.0:<port>` to accept external traffic."
            ));
        }
    }

    // v0.36 slice 9 — backend × tenancy alignment check. Schema-mode
    // tenancy uses `SET search_path` which only Postgres supports.
    // Flag config files that pair a non-PG backend with tenancy.
    if let Some(backend) = settings.database.resolved_backend() {
        let tenancy_on = crate::config::Settings::detected_features()
            .iter()
            .any(|f| *f == "tenancy");
        if tenancy_on && backend != "postgres" {
            out.warnings.push(format!(
                "[database] backend = `{backend}` with `tenancy` feature on — \
                 schema-mode multi-tenancy is Postgres-only by language semantics \
                 (it relies on `SET search_path`). Either switch to a Postgres URL \
                 in prod, or use `crate::tenancy::DatabasePools<DB>` for the \
                 one-database-per-tenant model that works on sqlite/mysql."
            ));
        }
    }

    // v0.36 slice 7+10 — [admin] section deploy audit. The new
    // settings-driven admin Builder reads these in
    // `admin::Builder::from_settings`; flag dev-defaults left in prod.
    let admin = &settings.admin;

    // theme_mode allowlist — typo-detection.
    if let Some(mode) = admin.theme_mode.as_deref() {
        if !matches!(mode, "auto" | "light" | "dark") {
            out.warnings.push(format!(
                "[admin] theme_mode = `{mode}` is not one of `auto` / `light` / `dark` — \
                 the chrome will ignore it and fall back to `auto`."
            ));
        }
    }

    // url_prefix smell tests — trailing slash trips up some
    // template hrefs; root-mount empty string is legal but worth
    // info-flagging since it conflicts with most reverse proxies.
    if let Some(prefix) = admin.url_prefix.as_deref() {
        if prefix.ends_with('/') && prefix.len() > 1 {
            out.warnings.push(format!(
                "[admin] url_prefix = `{prefix}` ends with a trailing slash — Builder will \
                 strip it, but config files should write the canonical form (no trailing slash) \
                 so reviewers can grep across deployments."
            ));
        }
    }

    // [mcp] deploy audit (#1099) — only when the MCP server is compiled in.
    // The agent-token HMAC key (RUSTANGO_SESSION_SECRET) is already audited in
    // the env pass; here we flag MCP-specific config that affects access.
    if crate::config::Settings::detected_features()
        .iter()
        .any(|f| *f == "mcp")
    {
        if settings.mcp.allowed_origins.is_empty() {
            out.info.push(
                "[mcp] allowed_origins = [] in prod tier — the MCP endpoint is same-origin only. \
                 If browser-based agents call it cross-origin, populate the CORS allow-list; \
                 server-to-server agents (no browser) don't need it."
                    .into(),
            );
        }
        if matches!(settings.mcp.enable_sse, Some(false)) {
            out.info.push(
                "[mcp] enable_sse = false in prod tier — no server→client notifications \
                 (list_changed / progress); agents must re-list to observe capability changes."
                    .into(),
            );
        }
        if settings.mcp.rate_limit().is_none() {
            out.info.push(
                "[mcp] rate_limit_per_minute unset or 0 in prod tier — the agent token + JSON-RPC \
                 endpoints are unthrottled. Set a per-IP cap to blunt credential-stuffing / abuse."
                    .into(),
            );
        }
    }
}
