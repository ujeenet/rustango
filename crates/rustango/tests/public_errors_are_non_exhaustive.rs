//! A new public `*Error` enum is `#[non_exhaustive]`, so adding a variant
//! later is not a breaking change. The enums listed in `KNOWN` predate the
//! rule and get the attribute in 0.62.0; the list may only shrink.

use std::path::{Path, PathBuf};

/// `file:EnumName` for each public error enum still without the attribute.
const KNOWN: &[&str] = &[
    "admin/errors.rs:AdminError",
    "auth_backends.rs:AuthError",
    "base36.rs:Base36Error",
    "base62.rs:Base62Error",
    "bulk_actions.rs:BulkActionError",
    "cache/mod.rs:CacheError",
    "config/loader.rs:ConfigError",
    "email_templates.rs:EmailRenderError",
    "env.rs:EnvError",
    "fixtures.rs:FixtureError",
    "forms/mod.rs:FormError",
    "forms/mod.rs:ModelFormError",
    "http_client.rs:HttpError",
    "i18n/mod.rs:I18nError",
    "ip_filter.rs:IpFilterError",
    "jobs/mod.rs:JobError",
    "jwt.rs:JwtError",
    "mailable.rs:MailableError",
    "media/mod.rs:MediaError",
    "notifications/mod.rs:NotificationError",
    "oauth2/mod.rs:OAuthError",
    "pagination.rs:CursorError",
    "pagination.rs:PaginatorError",
    "passkey/error.rs:PasskeyError",
    "passwords.rs:PasswordError",
    "secrets.rs:SecretsError",
    "session.rs:SessionSecretError",
    "sessions.rs:SessionError",
    "shortcuts.rs:ShortcutError",
    "signed_url.rs:SignedUrlError",
    "signing.rs:LoadsError",
    "signing.rs:SignError",
    "soft_delete.rs:SoftDeleteError",
    "sql/pool.rs:PoolError",
    "sso/mod.rs:SsoError",
    "storage/mod.rs:StorageError",
    "tenancy/agents.rs:AgentError",
    "tenancy/auth_backends.rs:AuthError",
    "tenancy/branding.rs:BrandError",
    "tenancy/error.rs:TenancyError",
    "tenancy/impersonation_handoff.rs:HandoffError",
    "tenancy/jwt_lifecycle.rs:JwtIssueError",
    "tenancy/member_auth.rs:MemberSessionError",
    "tenancy/operators.rs:OperatorError",
    "tenancy/secrets.rs:SecretsError",
    "tenancy/session.rs:SessionError",
    "tenancy/sweep.rs:SweepError",
    "urls.rs:ReverseError",
];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `file:EnumName` for every `pub enum *Error` whose attributes lack
/// `#[non_exhaustive]`.
fn exhaustive_public_errors() -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(rest) = line.trim_start().strip_prefix("pub enum ") else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.ends_with("Error") {
                continue;
            }
            // Walk up over the doc comments and attributes above the enum.
            let marked = lines[..i]
                .iter()
                .rev()
                .map(|l| l.trim())
                .take_while(|l| l.starts_with("#[") || l.starts_with("//") || l.ends_with(")]"))
                .any(|l| l.contains("non_exhaustive"));
            if !marked {
                let rel = file.strip_prefix(&src).expect("under src");
                let rel = rel.to_string_lossy().replace('\\', "/");
                found.push(format!("{rel}:{name}"));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn new_public_errors_are_non_exhaustive() {
    let found = exhaustive_public_errors();
    let new: Vec<_> = found
        .iter()
        .filter(|f| !KNOWN.contains(&f.as_str()))
        .collect();
    assert!(
        new.is_empty(),
        "add #[non_exhaustive] to these public error enums: {new:?}"
    );
    let fixed: Vec<_> = KNOWN
        .iter()
        .filter(|k| !found.iter().any(|f| f == *k))
        .collect();
    assert!(
        fixed.is_empty(),
        "these are non_exhaustive now (or gone); drop them from KNOWN: {fixed:?}"
    );
}
