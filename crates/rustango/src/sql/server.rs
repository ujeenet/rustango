//! Validated names for server-level DDL: a tenant's database and its
//! login. The `Dialect` emitters take these types, so a system database
//! or an unquotable name never reaches `CREATE DATABASE` / `CREATE USER`.

use std::fmt;

/// Postgres databases no tenant operation may create, drop or own.
pub(crate) const PG_SYSTEM_DATABASES: &[&str] = &["postgres", "template0", "template1"];

/// MySQL / MariaDB schemas no tenant operation may create, drop or own.
pub(crate) const MYSQL_SYSTEM_DATABASES: &[&str] =
    &["mysql", "information_schema", "performance_schema", "sys"];

/// The Postgres database an admin connects to for server-level DDL:
/// `CREATE`/`DROP DATABASE` cannot run inside the target database.
#[cfg(feature = "postgres")]
pub(crate) const PG_MAINTENANCE_DATABASE: &str = "postgres";

/// MySQL caps user names at 32 characters; the strictest backend wins.
const LOGIN_MAX_LEN: usize = 32;

/// A name refused by [`DatabaseName`], [`LoginName`], [`Login`] or
/// [`LoginPassword`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidName(String);

/// A tenant database name: a tenant slug (`[a-z0-9-]`, at most 63
/// characters, no hyphen at either end), never a system database.
///
/// No `_` or `%` either, so the name is also safe in MySQL's
/// `GRANT … ON db.*`, where both are wildcards.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatabaseName(String);

impl DatabaseName {
    /// # Errors
    /// [`InvalidName`] when `name` is not a slug or names a system database.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidName> {
        let name = name.into();
        if name.is_empty() || name.len() > 63 {
            return Err(InvalidName(format!(
                "database name `{name}` must be 1 to 63 characters"
            )));
        }
        let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if !name.bytes().all(legal) || name.starts_with('-') || name.ends_with('-') {
            return Err(InvalidName(format!(
                "database name `{name}` may hold only lowercase letters, digits and inner hyphens"
            )));
        }
        if PG_SYSTEM_DATABASES
            .iter()
            .chain(MYSQL_SYSTEM_DATABASES)
            .any(|s| *s == name)
        {
            return Err(InvalidName(format!(
                "`{name}` is a system database and cannot belong to a tenant"
            )));
        }
        Ok(Self(name))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DatabaseName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A database login name: `[a-z0-9_-]`, at most 32 characters, starting
/// with a letter or digit. Built-in and reserved role names are refused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoginName(String);

impl LoginName {
    /// # Errors
    /// [`InvalidName`] when `name` breaks the rule above.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidName> {
        let name = name.into();
        if name.is_empty() || name.len() > LOGIN_MAX_LEN {
            return Err(InvalidName(format!(
                "login name `{name}` must be 1 to {LOGIN_MAX_LEN} characters"
            )));
        }
        let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_';
        if !name.bytes().all(legal) || !name.as_bytes()[0].is_ascii_alphanumeric() {
            return Err(InvalidName(format!(
                "login name `{name}` may hold only lowercase letters, digits, `_` and `-`, \
                 and must start with a letter or digit"
            )));
        }
        let reserved = [
            "postgres",
            "public",
            "none",
            "current_user",
            "current_role",
            "session_user",
            "root",
            "mysql",
            "sys",
        ];
        if reserved.contains(&name.as_str()) || name.starts_with("pg_") {
            return Err(InvalidName(format!("login name `{name}` is reserved")));
        }
        Ok(Self(name))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LoginName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A login and, on MySQL, the host it may connect from (`%` by default).
/// Postgres ignores the host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Login {
    name: LoginName,
    host: String,
}

impl Login {
    #[must_use]
    pub fn new(name: LoginName) -> Self {
        Self {
            name,
            host: "%".to_owned(),
        }
    }

    /// Limit a MySQL login to `host` (`10.0.%`, `app.internal`, …).
    ///
    /// # Errors
    /// [`InvalidName`] for an empty, over-long or control-character host.
    pub fn with_host(mut self, host: impl Into<String>) -> Result<Self, InvalidName> {
        let host = host.into();
        if host.is_empty() || host.len() > 255 || host.chars().any(char::is_control) {
            return Err(InvalidName(format!("login host `{host}` is not valid")));
        }
        self.host = host;
        Ok(self)
    }

    #[must_use]
    pub fn name(&self) -> &LoginName {
        &self.name
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }
}

/// A login's password. Never printed: `Debug` shows `***`.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginPassword(String);

impl LoginPassword {
    /// Any non-empty text without NUL; the emitters escape the rest.
    ///
    /// # Errors
    /// [`InvalidName`] for an empty password or one holding NUL.
    pub fn new(password: impl Into<String>) -> Result<Self, InvalidName> {
        let password = password.into();
        if password.is_empty() || password.contains('\0') || password.len() > 255 {
            return Err(InvalidName(
                "a password must be 1 to 255 bytes and hold no NUL".to_owned(),
            ));
        }
        Ok(Self(password))
    }

    /// 32 URL-safe characters from the OS CSPRNG: 192 bits, and safe in a
    /// connection URL without escaping.
    #[cfg(feature = "_rand")]
    #[must_use]
    pub fn generate() -> Self {
        Self(crate::random::get_random_token_urlsafe(32))
    }

    /// The plain text, for the one place that needs it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// What Postgres' `PASSWORD` gets: a SCRAM-SHA-256 verifier for
    /// printable ASCII (SASLprep leaves it unchanged, so clients agree),
    /// else the text. The verifier needs the `tenancy` build's crypto.
    pub(crate) fn pg_secret(&self) -> std::borrow::Cow<'_, str> {
        #[cfg(feature = "tenancy")]
        if self.0.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            use rand::RngCore as _;
            let mut salt = [0u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut salt);
            return scram_sha256_verifier(&self.0, &salt, 4096).into();
        }
        std::borrow::Cow::Borrowed(&self.0)
    }
}

/// The verifier Postgres stores for SCRAM-SHA-256 (RFC 5802 / 7677).
#[cfg(feature = "tenancy")]
pub(crate) fn scram_sha256_verifier(password: &str, salt: &[u8], iterations: u32) -> String {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    let hmac = |key: &[u8], parts: &[&[u8]]| {
        let mut m = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC accepts any key length");
        for p in parts {
            m.update(p);
        }
        m.finalize().into_bytes()
    };
    // Hi(): PBKDF2-HMAC-SHA-256 with one output block.
    let mut u = hmac(password.as_bytes(), &[salt, &1u32.to_be_bytes()]);
    let mut salted = u;
    for _ in 1..iterations {
        u = hmac(password.as_bytes(), &[&u]);
        for (s, b) in salted.iter_mut().zip(u.iter()) {
            *s ^= b;
        }
    }
    let stored = Sha256::digest(hmac(&salted, &[b"Client Key"]));
    let server = hmac(&salted, &[b"Server Key"]);
    format!(
        "SCRAM-SHA-256${iterations}:{}${}:{}",
        base64_std(salt),
        base64_std(&stored),
        base64_std(&server)
    )
}

/// Standard, padded base64; `tenancy` does not pull the base64 crate.
#[cfg(feature = "tenancy")]
fn base64_std(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A collation for a new MySQL tenant database: `utf8mb4_` and then
/// `[a-z0-9_]`, so it is safe unquoted and fits the utf8mb4 character set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collation(String);

impl Collation {
    /// # Errors
    /// [`InvalidName`] for anything but a utf8mb4 collation name.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidName> {
        let name = name.into();
        let rest = name.strip_prefix("utf8mb4_").unwrap_or_default();
        let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_';
        if rest.is_empty() || name.len() > 64 || !rest.bytes().all(legal) {
            return Err(InvalidName(format!("`{name}` is not a utf8mb4 collation")));
        }
        Ok(Self(name))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LoginPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LoginPassword(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{Dialect, MySql, Postgres, Sqlite};

    fn db(s: &str) -> DatabaseName {
        DatabaseName::new(s).unwrap()
    }

    fn login(s: &str) -> Login {
        Login::new(LoginName::new(s).unwrap())
    }

    #[test]
    fn database_name_is_a_slug_and_not_a_system_database() {
        assert_eq!(db("acme-shop").as_str(), "acme-shop");
        for bad in [
            "",
            "-a",
            "a-",
            "Acme",
            "a_b",
            "a%b",
            "a\"b",
            "a`b",
            "postgres",
            "template1",
            "mysql",
            "sys",
        ] {
            assert!(DatabaseName::new(bad).is_err(), "{bad:?} accepted");
        }
        assert!(DatabaseName::new("a".repeat(64)).is_err());
    }

    #[test]
    fn login_name_rule() {
        assert!(LoginName::new("acme-shop").is_ok());
        assert!(LoginName::new("a_b").is_ok());
        for bad in [
            "", "_a", "-a", "A", "root", "postgres", "pg_x", "a'b", "a b",
        ] {
            assert!(LoginName::new(bad).is_err(), "{bad:?} accepted");
        }
        assert!(LoginName::new("a".repeat(33)).is_err());
    }

    #[test]
    fn password_debug_hides_it() {
        let pw = LoginPassword::new("s3cret").unwrap();
        assert!(!format!("{pw:?}").contains("s3cret"));
        assert!(LoginPassword::new("").is_err());
        assert!(LoginPassword::new("a\0b").is_err());
    }

    #[test]
    fn postgres_emitters_quote_names_and_escape_the_password() {
        let d = Postgres;
        // Non-ASCII, so sent as text even where SCRAM is available.
        let pw = LoginPassword::new(r#"é'b\c"d"#).unwrap();
        assert_eq!(
            d.create_database_sql(&db("acme-shop"), None).unwrap(),
            r#"CREATE DATABASE "acme-shop""#
        );
        assert_eq!(
            d.drop_database_sql(&db("acme-shop")).unwrap(),
            r#"DROP DATABASE IF EXISTS "acme-shop""#
        );
        assert_eq!(
            d.force_drop_database_sql(&db("acme-shop")).unwrap(),
            r#"DROP DATABASE IF EXISTS "acme-shop" WITH (FORCE)"#
        );
        // E'' reads `\` as an escape whatever `standard_conforming_strings` says.
        assert_eq!(
            d.create_login_sql(&login("acme-shop"), &pw).unwrap(),
            r#"CREATE ROLE "acme-shop" LOGIN PASSWORD E'é''b\\c"d'"#
        );
        assert_eq!(
            d.create_login_sql(&login("u"), &LoginPassword::new("é'y").unwrap())
                .unwrap(),
            r#"CREATE ROLE "u" LOGIN PASSWORD 'é''y'"#
        );
        let exists = d.login_exists_sql(&login("u")).unwrap();
        assert_eq!(
            exists.sql,
            "SELECT count(*) FROM pg_roles WHERE rolname = $1"
        );
        assert_eq!(exists.params.len(), 1);
        assert_eq!(
            d.drop_login_sql(&login("u")).unwrap(),
            r#"DROP ROLE IF EXISTS "u""#
        );
        assert_eq!(
            d.grant_database_sql(&db("acme-shop"), &login("u")),
            vec![
                r#"REVOKE ALL ON DATABASE "acme-shop" FROM PUBLIC"#.to_owned(),
                r#"GRANT ALL PRIVILEGES ON DATABASE "acme-shop" TO "u""#.to_owned(),
            ]
        );
        assert_eq!(
            d.grant_in_database_sql(&login("u")),
            vec![r#"GRANT ALL ON SCHEMA public TO "u""#.to_owned()]
        );
        // No dual passwords on Postgres: retaining is ignored.
        assert_eq!(
            d.alter_login_password_sql(&login("u"), &LoginPassword::new("é'y").unwrap(), true)
                .unwrap(),
            r#"ALTER ROLE "u" PASSWORD 'é''y'"#
        );
        assert!(d.discard_old_password_sql(&login("u")).is_none());
        let rights = d.tenant_rights_sql(&db("acme-shop"), &login("u")).unwrap();
        assert!(rights.sql.contains("ADMIN OPTION") && rights.sql.contains("pg_signal_backend"));
        assert_eq!(rights.params.len(), 2);
        let sessions = d
            .database_sessions_sql(&db("a"), "rustango-tenant:7")
            .unwrap();
        assert!(
            sessions.sql.contains("IS DISTINCT FROM $2"),
            "{}",
            sessions.sql
        );
        let end = d.end_sessions_sql(&db("a"), "rustango-tenant:7").unwrap();
        assert!(end.sql.contains("pg_terminate_backend") && end.sql.contains("= $2"));
    }

    #[test]
    fn mysql_rotation_can_keep_the_old_password() {
        let d = MySql;
        let u = login("u");
        let pw = LoginPassword::new("a'b").unwrap();
        assert_eq!(
            d.alter_login_password_sql(&u, &pw, false).unwrap(),
            "ALTER USER `u`@`%` IDENTIFIED BY 'a''b'"
        );
        assert_eq!(
            d.alter_login_password_sql(&u, &pw, true).unwrap(),
            "ALTER USER `u`@`%` IDENTIFIED BY 'a''b' RETAIN CURRENT PASSWORD"
        );
        assert_eq!(
            d.discard_old_password_sql(&u).unwrap(),
            "ALTER USER `u`@`%` DISCARD OLD PASSWORD"
        );
        assert!(d.tenant_rights_sql(&db("a"), &u).is_none());
        assert!(d.end_sessions_sql(&db("a"), "t").is_none());
    }

    /// The new password never reaches the statement as text on Postgres.
    #[cfg(feature = "tenancy")]
    #[test]
    fn postgres_rotation_sends_a_verifier() {
        let sql = Postgres
            .alter_login_password_sql(
                &login("u"),
                &LoginPassword::new("n3w-secret").unwrap(),
                false,
            )
            .unwrap();
        assert!(
            sql.starts_with(r#"ALTER ROLE "u" PASSWORD 'SCRAM-SHA-256$4096:"#),
            "{sql}"
        );
        assert!(!sql.contains("n3w-secret"), "{sql}");
    }

    #[test]
    fn mysql_emitters_quote_names_and_escape_the_password() {
        let d = MySql;
        let pw = LoginPassword::new(r#"a'b\c"d"#).unwrap();
        let u = login("u").with_host("10.0.%").unwrap();
        assert_eq!(
            d.create_database_sql(&db("acme-shop"), None).unwrap(),
            "CREATE DATABASE `acme-shop` CHARACTER SET utf8mb4"
        );
        let cs = Collation::new("utf8mb4_0900_as_cs").unwrap();
        assert_eq!(
            d.create_database_sql(&db("a"), Some(&cs)).unwrap(),
            "CREATE DATABASE `a` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs"
        );
        for bad in [
            "latin1_swedish_ci",
            "utf8mb4_x; DROP",
            "utf8mb4_",
            "UTF8MB4_BIN",
        ] {
            assert!(Collation::new(bad).is_err(), "{bad}");
        }
        assert_eq!(
            d.create_login_sql(&u, &pw).unwrap(),
            r#"CREATE USER `u`@`10.0.%` IDENTIFIED BY 'a''b\\c"d'"#
        );
        assert_eq!(
            d.drop_login_sql(&u).unwrap(),
            "DROP USER IF EXISTS `u`@`10.0.%`"
        );
        assert_eq!(
            d.grant_database_sql(&db("acme-shop"), &u),
            vec!["GRANT ALL PRIVILEGES ON `acme-shop`.* TO `u`@`10.0.%`".to_owned()]
        );
        assert!(d.grant_in_database_sql(&u).is_empty());
        assert!(d.force_drop_database_sql(&db("a")).is_none());
        let exists = d.login_exists_sql(&u).unwrap();
        assert_eq!(
            exists.sql,
            "SELECT count(*) FROM mysql.user WHERE User = ? AND Host = ?"
        );
        assert_eq!(exists.params.len(), 2);
        // A backtick in a host is doubled, not a way out of the quotes.
        let odd = login("u").with_host("a`b").unwrap();
        assert_eq!(
            d.drop_login_sql(&odd).unwrap(),
            "DROP USER IF EXISTS `u`@`a``b`"
        );
    }

    /// Expected value from Python's `hashlib.pbkdf2_hmac` + `hmac`.
    #[cfg(feature = "tenancy")]
    #[test]
    fn scram_verifier_matches_an_independent_implementation() {
        let salt: Vec<u8> = (0..16).collect();
        assert_eq!(
            scram_sha256_verifier("pencil", &salt, 4096),
            "SCRAM-SHA-256$4096:AAECAwQFBgcICQoLDA0ODw==\
             $zHCdol2044/ZyWzPLi7oxApCkamKw9Z+E4U/QApd/5Y=\
             :dd5peBOitVnLNFu7VmwP+HiDaaw4OUCv396eVCWhYiE="
        );
    }

    /// Printable ASCII goes to the server as a verifier, never as text.
    #[cfg(feature = "tenancy")]
    #[test]
    fn postgres_sends_a_scram_verifier_for_ascii() {
        let sql = Postgres
            .create_login_sql(&login("u"), &LoginPassword::new("s3cret'?").unwrap())
            .unwrap();
        assert!(
            sql.starts_with(r#"CREATE ROLE "u" LOGIN PASSWORD 'SCRAM-SHA-256$4096:"#),
            "{sql}"
        );
        assert!(!sql.contains("s3cret"), "{sql}");
    }

    #[test]
    fn sqlite_has_no_server_ddl() {
        let d = Sqlite;
        assert!(d.create_database_sql(&db("a"), None).is_none());
        assert!(d
            .create_login_sql(&login("a"), &LoginPassword::new("x").unwrap())
            .is_none());
        assert!(d.grant_database_sql(&db("a"), &login("a")).is_empty());
        let pw = LoginPassword::new("x").unwrap();
        assert!(d.alter_login_password_sql(&login("a"), &pw, true).is_none());
        assert!(d.database_sessions_sql(&db("a"), "t").is_none());
    }
}
