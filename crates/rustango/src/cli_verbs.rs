//! The built-in `manage` verbs, and the hint for a mistyped one.

/// The verbs a single-database project dispatches: `Cli` and `migrate::manage`.
/// Includes verbs this build's features drop.
pub(crate) const SINGLE_DB_VERBS: &[&str] = &[
    // `Cli` itself.
    "runserver",
    "run-server",
    "dbshell",
    "help",
    "--help",
    "-h",
    // `migrate::manage`.
    "makemigrations",
    "migrate",
    "downgrade",
    "showmigrations",
    "status",
    "sqlmigrate",
    "forget-pending",
    "startapp",
    "add-data-op",
    "about",
    "check",
    "docs",
    "version",
    "--version",
    "db:dump",
    "db:restore",
    "db:info",
    "dumpdata",
    "loaddata",
    "showurls",
    "showmodels",
    "create-admin",
    "set-admin-active",
    "reset-admin-totp",
    "flush",
    "prune",
    "clear-cache",
    "clearsessions",
    "createcachetable",
    "create-cache-table",
    "sendtestemail",
    "inspectdb",
    "reset-password",
    "change-password",
    "audit-cleanup",
    "make:viewset",
    "make:api_routes",
    "make:serializer",
    "make:form",
    "make:job",
    "make:scheduled",
    "make:worker",
    "make:notification",
    "make:middleware",
    "make:test",
];

/// The verbs `tenancy::manage` adds; the rest fall through to [`SINGLE_DB_VERBS`].
#[cfg(any(feature = "tenancy", feature = "manage", test))]
pub(crate) const TENANCY_VERBS: &[&str] = &[
    "create-tenant",
    "test-tenant-connection",
    "drop-tenant",
    "purge-tenant",
    "rotate-tenant-password",
    "list-tenants",
    "edit-tenant",
    "reseal-tenant-secrets",
    "list-hosts",
    "add-host",
    "remove-host",
    "set-host-enabled",
    "add-db-server",
    "edit-db-server",
    "list-db-servers",
    "test-db-server",
    "remove-db-server",
    "prewarm-pools",
    "migrate-tenants",
    "migrate-registry",
    "migrate-tenant-storage",
    "create-operator",
    "list-operators",
    "set-operator-active",
    "reset-operator-totp",
    "list-runs",
    "show-run",
    "audit-log",
    "create-user",
    "create-superuser",
    "set-superuser",
    "revoke-passkey",
    "set-password-link",
    "reset-operator-password",
    "change-operator-password",
    "init-tenancy",
    "wizard",
    "init",
    "menu",
    "actions",
    "create-role",
    "list-roles",
    "assign-role",
    "revoke-role",
    "grant-perm",
    "revoke-perm",
    "create-api-key",
    "create-agent",
    "rotate-agent-secret",
    "list-agents",
    "create-skill",
    "grant-skill",
    "revoke-skill",
    "list-skills",
    "create-user-key",
    "list-user-keys",
    "revoke-user-key",
    "map-skill-permission",
    "unmap-skill-permission",
    "seed-permissions",
    "sso-deactivate-idle",
];

/// Spellings from other tools that edit distance cannot reach.
const ALIASES: &[(&str, &str)] = &[("createsuperuser", "create-admin")];

/// `input` with case and the `-` / `_` / `:` separators dropped.
fn squash(input: &str) -> String {
    input
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ':'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Levenshtein distance over chars.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == *cb {
                prev
            } else {
                1 + prev.min(cur).min(row[j])
            };
            prev = cur;
        }
    }
    row[b.len()]
}

/// The known verb closest to `input`, if one is close enough to be a typo.
pub(crate) fn did_you_mean<'a>(input: &str, verbs: &[&'a [&'a str]]) -> Option<&'a str> {
    let known = || {
        verbs
            .iter()
            .flat_map(|v| v.iter().copied())
            .filter(|v| !v.starts_with('-'))
    };
    let typed = squash(input);
    let limit = (typed.chars().count() / 3).clamp(2, 3);
    let near = known()
        .map(|v| (distance(&typed, &squash(v)), v))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, v)| v);
    near.or_else(|| {
        ALIASES
            .iter()
            .find(|(from, _)| *from == input)
            .and_then(|(_, to)| known().find(|v| v == to))
    })
}

/// The error text for an unknown verb, with a hint when one is near.
pub(crate) fn unknown_verb(input: &str, verbs: &[&[&str]]) -> String {
    match did_you_mean(input, verbs) {
        Some(v) => format!(
            "unknown subcommand: `{input}` — did you mean `{v}`? (run with --help for usage)"
        ),
        None => format!("unknown subcommand: `{input}` (run with --help for usage)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typo_suggests_the_nearest_verb() {
        let single = [SINGLE_DB_VERBS];
        assert_eq!(did_you_mean("migrat", &single), Some("migrate"));
        assert_eq!(
            did_you_mean("showmigration", &single),
            Some("showmigrations")
        );
        assert_eq!(
            did_you_mean("change_password", &single),
            Some("change-password")
        );
        assert_eq!(did_you_mean("frobnicate", &single), None);
    }

    #[test]
    fn createsuperuser_maps_to_each_modes_own_verb() {
        assert_eq!(
            did_you_mean("createsuperuser", &[SINGLE_DB_VERBS]),
            Some("create-admin")
        );
        let tenant = [TENANCY_VERBS, SINGLE_DB_VERBS];
        assert_eq!(
            did_you_mean("createsuperuser", &tenant),
            Some("create-superuser")
        );
    }

    #[test]
    fn the_error_carries_the_hint() {
        let msg = unknown_verb("makemigration", &[SINGLE_DB_VERBS]);
        assert!(msg.contains("did you mean `makemigrations`?"), "{msg}");
        assert!(!unknown_verb("zzzzzz", &[SINGLE_DB_VERBS]).contains("did you mean"));
    }

    #[test]
    fn no_verb_is_in_both_lists() {
        for v in TENANCY_VERBS {
            assert!(!SINGLE_DB_VERBS.contains(v), "{v}");
        }
    }
}
