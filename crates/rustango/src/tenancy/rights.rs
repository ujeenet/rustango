//! The database rights a tenant create needs, per storage mode, and what
//! the login doing it has. One report feeds the server Test, the New
//! tenant Test, the run's check step and the CLI, so all say the same.

use std::path::PathBuf;

use crate::sql::{AdminRight, Dialect, Pool, ServerAccount};

use super::server_admin::PrivilegeReport;

/// Whether a login has a right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Has {
    Yes,
    No,
    /// The server would not say. Does not block: the create will tell.
    Unknown,
}

impl Has {
    fn of(read: bool, has: bool) -> Self {
        match (read, has) {
            (false, _) => Self::Unknown,
            (true, true) => Self::Yes,
            (true, false) => Self::No,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }
}

/// What a storage mode makes, and so which rights it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequiredRights {
    /// A database and its own login on a saved server, made with the
    /// server's admin login.
    NewDatabase,
    /// A schema in the registry database, made with the registry's login.
    NewSchema,
}

impl RequiredRights {
    /// The rights needed on `backend` (`"postgres"`, `"mysql"`, `"sqlite"`).
    #[must_use]
    pub fn rights(self, backend: &str) -> &'static [AdminRight] {
        match (self, backend) {
            (Self::NewSchema, _) => &[AdminRight::CreateSchema],
            (Self::NewDatabase, "sqlite") => &[AdminRight::CreateDatabase],
            (Self::NewDatabase, _) => &[
                AdminRight::CreateDatabase,
                AdminRight::CreateLogin,
                AdminRight::Grant,
            ],
        }
    }
}

/// One right and whether the login has it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RightCheck {
    pub right: AdminRight,
    pub has: Has,
    /// What it lets the login do, with the backend's name for it.
    pub label: String,
}

/// The rights one storage mode needs, each yes, no or unknown, and what
/// to do about the missing ones. Holds no password.
#[derive(Debug, Clone)]
pub struct RightsReport {
    required: RequiredRights,
    backend: &'static str,
    server_version: Option<String>,
    account: Option<ServerAccount>,
    /// The registry database, for a schema.
    database: Option<String>,
    dir: Option<PathBuf>,
    /// Why the login was refused, already scrubbed.
    refused: Option<String>,
    /// A blocker that is not a right.
    gate: Option<String>,
    checks: Vec<RightCheck>,
}

fn label(right: AdminRight, backend: &str, database: Option<&str>, dir: Option<&str>) -> String {
    match (right, backend) {
        (AdminRight::CreateDatabase, "sqlite") => {
            format!("write files in `{}`", dir.unwrap_or("the directory"))
        }
        (AdminRight::CreateDatabase, "postgres") => "create databases (CREATEDB)".into(),
        (AdminRight::CreateDatabase, _) => "create databases (CREATE, DROP)".into(),
        (AdminRight::CreateLogin, "postgres") => "create logins (CREATEROLE)".into(),
        (AdminRight::CreateLogin, _) => "create logins (CREATE USER)".into(),
        (AdminRight::Grant, "postgres") => "grant rights on the new database (as its owner)".into(),
        (AdminRight::Grant, _) => "grant rights (GRANT OPTION)".into(),
        (AdminRight::CreateSchema, _) => format!(
            "create schemas in database `{}` (CREATE)",
            database.unwrap_or("?")
        ),
    }
}

/// `a`, `a or b`, `a, b or c`.
fn either(parts: &[&str]) -> String {
    match parts {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

fn dialect(backend: &str) -> Option<&'static dyn Dialect> {
    match backend {
        "postgres" => Some(&crate::sql::Postgres),
        "mysql" => Some(&crate::sql::MySql),
        _ => None,
    }
}

impl RightsReport {
    fn new(
        required: RequiredRights,
        backend: &'static str,
        database: Option<String>,
        dir: Option<PathBuf>,
        has: impl Fn(AdminRight) -> Has,
    ) -> Self {
        let dir_text = dir.as_ref().map(|d| d.display().to_string());
        let checks = required
            .rights(backend)
            .iter()
            .map(|&right| RightCheck {
                right,
                has: has(right),
                label: label(right, backend, database.as_deref(), dir_text.as_deref()),
            })
            .collect();
        Self {
            required,
            backend,
            server_version: None,
            account: None,
            database,
            dir,
            refused: None,
            gate: None,
            checks,
        }
    }

    /// A saved server's admin login, for a new tenant database.
    pub(crate) fn for_new_database(p: &PrivilegeReport) -> Self {
        let read = p.can_login && p.rights_read;
        let mut r = Self::new(
            RequiredRights::NewDatabase,
            p.backend,
            None,
            p.dir.clone(),
            |right| match right {
                AdminRight::CreateDatabase => Has::of(read, p.can_create_database),
                AdminRight::CreateLogin => Has::of(read, p.can_create_user),
                AdminRight::Grant => Has::of(read, p.can_grant),
                _ => Has::Unknown,
            },
        );
        r.server_version.clone_from(&p.server_version);
        r.account.clone_from(&p.account);
        if p.backend == "sqlite" && !p.can_login {
            r.gate = Some(format!(
                "The directory `{}` does not exist.",
                r.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default()
            ));
        } else if !p.can_login {
            r.refused = Some(
                p.login_error
                    .as_ref()
                    .map_or_else(|| "the login is refused".to_owned(), |d| d.summary()),
            );
        } else if p.pg_major.is_some_and(|m| m < 15) {
            r.gate = Some(
                "PostgreSQL 15 or later is needed: before 15 every login may create tables \
                 in the shared databases."
                    .into(),
            );
        }
        r
    }

    /// The registry's own login, for a new schema in its database. An
    /// unreadable answer is unknown, not a refusal.
    pub(crate) async fn for_new_schema(registry: &Pool) -> Self {
        let backend = registry.dialect().name();
        let row = match registry.dialect().schema_privileges_sql() {
            Some(sql) => {
                crate::sql::raw_query_pool::<(String, String, bool)>(sql, Vec::new(), registry)
                    .await
                    .ok()
                    .and_then(|rows| rows.into_iter().next())
            }
            None => None,
        };
        let (account, database, has) = match row {
            Some((user, db, ok)) => (
                Some(ServerAccount { user, host: None }),
                Some(db),
                Has::of(true, ok),
            ),
            None => (None, None, Has::Unknown),
        };
        let mut r = Self::new(RequiredRights::NewSchema, backend, database, None, |_| has);
        r.account = account;
        r
    }

    #[must_use]
    pub fn required(&self) -> RequiredRights {
        self.required
    }

    #[must_use]
    pub fn backend(&self) -> &'static str {
        self.backend
    }

    #[must_use]
    pub fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }

    /// `true` when the server let the login in.
    #[must_use]
    pub fn logged_in(&self) -> bool {
        self.refused.is_none()
    }

    #[must_use]
    pub fn checks(&self) -> &[RightCheck] {
        &self.checks
    }

    /// The rights the login does not have.
    #[must_use]
    pub fn missing(&self) -> Vec<&RightCheck> {
        self.checks.iter().filter(|c| c.has == Has::No).collect()
    }

    /// The rights the server would not report.
    #[must_use]
    pub fn unknown(&self) -> Vec<&RightCheck> {
        self.checks.iter().filter(|c| c.has == Has::Unknown).collect()
    }

    /// Nothing known blocks the create.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.refused.is_none() && self.gate.is_none() && self.missing().is_empty()
    }

    fn who(&self) -> String {
        let name = self.account.as_ref().map(|a| a.user.as_str());
        match (self.required, name) {
            (_, _) if self.backend == "sqlite" => "The app".into(),
            (RequiredRights::NewSchema, Some(u)) => {
                format!("The registry's login `{u}` (DATABASE_URL)")
            }
            (RequiredRights::NewSchema, None) => "The registry's login (DATABASE_URL)".into(),
            (_, Some(u)) => format!("The login `{u}`"),
            (_, None) => "The admin login".into(),
        }
    }

    /// What blocks the create, in one sentence; `None` when ready.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if let Some(why) = &self.refused {
            return Some(format!("The server refused the login: {why}."));
        }
        if let Some(gate) = &self.gate {
            return Some(gate.clone());
        }
        let missing: Vec<&str> = self.missing().iter().map(|c| c.label.as_str()).collect();
        (!missing.is_empty()).then(|| format!("{} cannot {}.", self.who(), either(&missing)))
    }

    /// The statements that would give the login what it lacks.
    #[must_use]
    pub fn fix_sql(&self) -> Vec<String> {
        let (Some(d), Some(account)) = (dialect(self.backend), &self.account) else {
            return Vec::new();
        };
        if !self.logged_in() || self.gate.is_some() {
            return Vec::new();
        }
        let missing: Vec<AdminRight> = self.missing().iter().map(|c| c.right).collect();
        d.grant_admin_rights_sql(account, self.database.as_deref(), &missing)
            .into_iter()
            .map(|s| format!("{s};"))
            .collect()
    }

    /// How to fix it, leading into [`Self::fix_sql`]; `None` when ready.
    #[must_use]
    pub fn fix(&self) -> Option<String> {
        if self.refused.is_some() {
            return Some("Check the server's saved admin login and password.".into());
        }
        if self.gate.is_some() && self.backend == "sqlite" {
            return Some("Create it and let the app's user write to it.".into());
        }
        if self.gate.is_some() {
            return None;
        }
        if self.missing().is_empty() {
            return None;
        }
        Some(
            match (self.required, self.backend) {
                (_, "sqlite") => "Let the app's user write to that directory.",
                (RequiredRights::NewSchema, _) => {
                    "Point DATABASE_URL at a login that has it, or run as a superuser or the \
                     database owner:"
                }
                (_, "mysql") => {
                    "Use an admin login such as root (with the generated docker-compose, use \
                     `root` as the server's admin login), or run as root:"
                }
                _ => "Use a superuser login such as postgres, or run as a superuser:",
            }
            .into(),
        )
    }

    /// A way round that needs no rights; `None` when ready.
    #[must_use]
    pub fn alternative(&self) -> Option<&'static str> {
        if self.is_ready() {
            return None;
        }
        Some(match self.required {
            RequiredRights::NewSchema => {
                "Or use database storage with an existing database (--mode database \
                 --database-url on the CLI): nothing is created, so no rights are needed."
            }
            RequiredRights::NewDatabase => {
                "Or use an existing database (\"Use an existing database URL\" in the console, \
                 --database-url on the CLI): nothing is created, so no rights are needed."
            }
        })
    }

    /// A note on rights the server would not report.
    #[must_use]
    pub fn unknown_note(&self) -> Option<String> {
        let unknown: Vec<&str> = self.unknown().iter().map(|c| c.label.as_str()).collect();
        (!unknown.is_empty() && self.logged_in()).then(|| {
            format!(
                "Could not read whether {} may {}; the create will try and report what the \
                 server says.",
                self.who().replacen("The", "the", 1),
                either(&unknown)
            )
        })
    }

    /// Every line of the refusal, `subject` first. Empty when ready.
    #[must_use]
    pub fn refusal_lines(&self, subject: &str) -> Vec<String> {
        let Some(problem) = self.problem() else {
            return Vec::new();
        };
        let mut out = vec![format!("{subject}: {problem}")];
        out.extend(self.fix());
        out.extend(self.fix_sql().into_iter().map(|s| format!("  {s}")));
        out.extend(self.alternative().map(ToOwned::to_owned));
        out
    }

    /// [`Self::refusal_lines`] as one text, a line each.
    #[must_use]
    pub fn refusal(&self, subject: &str) -> String {
        self.refusal_lines(subject).join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mysql_app() -> PrivilegeReport {
        let mut p = PrivilegeReport::for_tests("mysql");
        p.account = Some(ServerAccount {
            user: "rustango".into(),
            host: Some("%".into()),
        });
        p.can_create_database = true;
        p
    }

    #[test]
    fn a_mysql_app_login_names_the_missing_rights_and_the_grant() {
        let r = mysql_app().rights();
        assert!(!r.is_ready());
        let text = r.refusal("database server `main` cannot create tenant databases");
        assert!(
            text.contains(
                "The login `rustango` cannot create logins (CREATE USER) or grant rights \
                 (GRANT OPTION)."
            ),
            "{text}"
        );
        assert!(text.contains("GRANT CREATE USER ON *.* TO 'rustango'@'%';"), "{text}");
        assert!(
            text.contains("GRANT ALL PRIVILEGES ON *.* TO 'rustango'@'%' WITH GRANT OPTION;"),
            "{text}"
        );
        assert!(text.contains("root") && text.contains("existing database"), "{text}");
    }

    #[test]
    fn a_postgres_login_gets_alter_role() {
        let mut p = PrivilegeReport::for_tests("postgres");
        p.account = Some(ServerAccount {
            user: "app\"x".into(),
            host: None,
        });
        p.pg_major = Some(16);
        let r = p.rights();
        assert_eq!(r.missing().len(), 3);
        assert_eq!(r.fix_sql(), vec!["ALTER ROLE \"app\"\"x\" CREATEDB CREATEROLE;"]);
    }

    #[test]
    fn unknown_rights_do_not_block() {
        let mut p = mysql_app();
        p.rights_read = false;
        let r = p.rights();
        assert!(r.is_ready(), "{:?}", r.problem());
        assert_eq!(r.unknown().len(), 3);
        assert!(r.unknown_note().is_some_and(|n| n.contains("the create will try")));
    }

    #[test]
    fn a_missing_sqlite_directory_says_so() {
        let mut p = PrivilegeReport::for_tests("sqlite");
        p.can_login = false;
        p.dir = Some("/nope/tenants".into());
        let text = p.rights().refusal("x");
        assert!(text.contains("`/nope/tenants` does not exist"), "{text}");
        assert!(text.contains("Create it"), "{text}");
    }
}
