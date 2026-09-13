//! Turning a driver's connect failure into something an operator can act
//! on.
//!
//! `sqlx` reports what went wrong at its own layer, which is often three
//! layers below the thing the reader has to change. The motivating case
//! (#1306) cost a reporter a debugging session:
//!
//! ```text
//! encountered unexpected or invalid data: Postgres protocol error
//! (reading ErrorResponse): Postgres returned a non-UTF-8 string for its
//! error message. This is most likely due to an error that occurred
//! during authentication and the default lc_messages locale is not
//! binary-compatible with UTF-8.
//! ```
//!
//! What had actually happened: a local PostgreSQL install was already on
//! 5432, so the Docker container was never reached at all. The local
//! server answered, rejected the container's credentials, and returned
//! the rejection in cp1252. Every word of that message is true and none
//! of it points at the port conflict — and it names `lc_messages`, which
//! sends the reader off configuring server locales.
//!
//! So: classify the failure into something that names the fix.
//!
//! ## Classification is on the variant, not the message
//!
//! Matching driver strings is brittle and localised. Every rule here
//! keys on an `sqlx::Error` variant or a `SQLSTATE`/errno, both of which
//! are stable and language-independent. The driver's own text is still
//! carried through as `detail` — it is genuinely useful once you know
//! which *kind* of problem you are reading about.
//!
//! ## Credentials never appear
//!
//! A connection URL has a password in it, and these strings end up in
//! terminals, logs and HTTP responses. [`redact`] is applied to every
//! endpoint this module renders.

use std::fmt;

/// What went wrong, in terms of what the operator has to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectFault {
    /// Nothing answered: connection refused, host not found, network
    /// unreachable, or the attempt timed out.
    Unreachable,
    /// Something answered but the conversation made no sense — the
    /// classic symptom of a *different* server on the expected port.
    UnexpectedServer,
    /// TLS could not be negotiated.
    TlsRejected,
    /// Reached the server; it refused the credentials.
    AuthFailed,
    /// Reached the server and authenticated; the named database is not
    /// there.
    NoSuchDatabase,
    /// Connected fine, but this role may not do what a tenant needs —
    /// create tables, most importantly. The failure a naive `SELECT 1`
    /// check sails straight past and a migration then dies on.
    PermissionDenied,
    /// Nothing above matched.
    Other,
}

impl ConnectFault {
    /// One line naming what to change. Deliberately imperative: the
    /// reader is stuck, and a description of the symptom does not help
    /// them.
    #[must_use]
    pub fn advice(self) -> &'static str {
        match self {
            Self::Unreachable => {
                "nothing is listening there — check the host and port, that the server is \
                 running, and that a firewall or container network is not in the way"
            }
            // The #1306 hint. Leading with the port conflict because on
            // a developer machine that is overwhelmingly what it is.
            Self::UnexpectedServer => {
                "something other than the expected database answered on this port — often a \
                 local PostgreSQL or MySQL install shadowing a Docker container. Check what \
                 is actually listening, and that the URL's scheme matches that server"
            }
            Self::TlsRejected => {
                "TLS could not be negotiated — check the `sslmode`/`ssl-mode` in the URL and \
                 whether the server requires (or refuses) encryption"
            }
            Self::AuthFailed => {
                "the server refused these credentials — check the username and password"
            }
            Self::NoSuchDatabase => {
                "the server is reachable but has no such database — create it, or fix the \
                 name in the URL"
            }
            Self::PermissionDenied => {
                "connected, but this role cannot create tables — grant it schema-level CREATE, \
                 or migrations will fail partway through"
            }
            Self::Other => "the connection attempt failed",
        }
    }
}

/// A classified connect failure, ready to show someone.
#[derive(Debug, Clone)]
pub struct ConnectDiagnosis {
    pub fault: ConnectFault,
    /// Where we tried, with any password removed.
    pub endpoint: String,
    /// The driver's own message. Kept because it is the detail that
    /// distinguishes two failures of the same kind.
    pub detail: String,
}

impl ConnectDiagnosis {
    /// Classify `err`, which came from trying to connect to `url`.
    #[must_use]
    pub fn of(url: &str, err: &sqlx::Error) -> Self {
        Self {
            fault: classify(err),
            endpoint: redact(url),
            detail: err.to_string(),
        }
    }

    /// Build one directly — for a fault detected by a probe rather than
    /// raised by the driver, such as a `CREATE TABLE` that came back
    /// denied.
    #[must_use]
    pub fn new(fault: ConnectFault, url: &str, detail: impl Into<String>) -> Self {
        Self {
            fault,
            endpoint: redact(url),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ConnectDiagnosis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Advice first: it is the part that is actionable, and the part
        // a reader skimming a wall of output needs to land on.
        write!(
            f,
            "{} (tried {}) — driver said: {}",
            self.fault.advice(),
            self.endpoint,
            self.detail
        )
    }
}

/// Classify an `sqlx::Error` raised while connecting.
fn classify(err: &sqlx::Error) -> ConnectFault {
    match err {
        // A protocol error *at connect time* means the bytes coming back
        // were not what this driver speaks. On a dev machine that is
        // nearly always another server on the port. (#1306's non-UTF-8
        // `ErrorResponse` lands here, as does pointing a `postgres://`
        // URL at a MySQL server.)
        sqlx::Error::Protocol(_) => ConnectFault::UnexpectedServer,
        sqlx::Error::Tls(_) => ConnectFault::TlsRejected,
        // Every I/O failure at connect time — refused, no such host,
        // network unreachable, timed out — is one story to the reader:
        // nothing answered. `PoolTimedOut` is the same story told by
        // the pool rather than the socket.
        sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut => ConnectFault::Unreachable,
        sqlx::Error::Database(db) => classify_db(db.as_ref()),
        _ => ConnectFault::Other,
    }
}

/// Map a backend's own error code.
///
/// Three vocabularies with one meaning each — and one trap. `code()` on
/// the `DatabaseError` trait is the `SQLSTATE`, which `MySQL` also
/// reports, but its `SQLSTATE`s are far coarser than its error numbers:
/// a missing database is `42000`, the same generic class as a syntax
/// error. So `MySQL` is classified on `number()`, reached by downcast,
/// with `SQLSTATE` only as a fallback.
fn classify_db(db: &dyn sqlx::error::DatabaseError) -> ConnectFault {
    #[cfg(feature = "mysql")]
    if let Some(my) = db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
        return match my.number() {
            // 1045 access denied for user
            1045 => ConnectFault::AuthFailed,
            // 1049 unknown database
            1049 => ConnectFault::NoSuchDatabase,
            // 1044 access denied to database, 1142 table command denied
            1044 | 1142 => ConnectFault::PermissionDenied,
            _ => ConnectFault::Other,
        };
    }

    let Some(code) = db.code() else {
        return ConnectFault::Other;
    };
    // Two dialects' codes share a couple of meanings, so some arms have
    // identical bodies. Merging them is what clippy wants and would
    // make this unreadable: the grouping by dialect is the only thing
    // that makes a five-character SQLSTATE and a bare `8` legible side
    // by side, and the next person adding a code needs to see where it
    // goes.
    #[allow(clippy::match_same_arms)]
    match code.as_ref() {
        // ---- PostgreSQL (SQLSTATE) ----
        // 28P01 invalid_password, 28000 invalid_authorization_specification
        "28P01" | "28000" => ConnectFault::AuthFailed,
        // 3D000 invalid_catalog_name
        "3D000" => ConnectFault::NoSuchDatabase,
        // 42501 insufficient_privilege
        "42501" => ConnectFault::PermissionDenied,

        // ---- SQLite (extended result codes, as strings) ----
        // 14 SQLITE_CANTOPEN — the file (or its directory) is not there
        "14" => ConnectFault::Unreachable,
        // 8 SQLITE_READONLY, 3 SQLITE_PERM, 1032 READONLY_DBMOVED
        "8" | "3" | "1032" => ConnectFault::PermissionDenied,

        _ => ConnectFault::Other,
    }
}

/// A connection URL with its password replaced.
///
/// These strings reach terminals, log files and — once the operator
/// console lands — HTTP responses. A URL is the single most likely place
/// for a credential to escape into one of those, so redaction happens
/// here rather than at each call site, where it would eventually be
/// forgotten.
///
/// Everything else is kept: the host, the port and the database name are
/// exactly what the reader needs to see.
#[must_use]
pub fn redact(url: &str) -> String {
    // `scheme://user:password@host:port/db?params`. Only the segment
    // between the last `:` of the userinfo and the `@` is secret, and
    // userinfo is whatever precedes the *first* `@` after `://`.
    let Some((scheme, rest)) = url.split_once("://") else {
        // `sqlite:path` and friends carry no credentials.
        return url.to_owned();
    };
    let Some((userinfo, hostpart)) = rest.split_once('@') else {
        return url.to_owned();
    };
    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
    if userinfo.contains(':') {
        format!("{scheme}://{user}:***@{hostpart}")
    } else {
        format!("{scheme}://{userinfo}@{hostpart}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_removes_the_password_and_keeps_everything_useful() {
        assert_eq!(
            redact("postgres://app:s3cret@db.internal:5432/acme"),
            "postgres://app:***@db.internal:5432/acme"
        );
    }

    #[test]
    fn redact_leaves_a_url_with_no_password_alone() {
        assert_eq!(
            redact("postgres://app@localhost:5432/acme"),
            "postgres://app@localhost:5432/acme"
        );
        assert_eq!(
            redact("postgres://localhost:5432/acme"),
            "postgres://localhost:5432/acme"
        );
    }

    #[test]
    fn redact_leaves_sqlite_paths_alone() {
        assert_eq!(
            redact("sqlite:./dev.db?mode=rwc"),
            "sqlite:./dev.db?mode=rwc"
        );
        assert_eq!(redact("sqlite://./dev.db"), "sqlite://./dev.db");
    }

    /// An `@` inside the password must not be mistaken for the userinfo
    /// separator in a way that leaks the rest of it.
    #[test]
    fn redact_handles_an_at_sign_in_the_password() {
        // Split on the FIRST `@`: everything after it is host-ish, and
        // the tail of a password containing `@` would otherwise survive.
        let out = redact("mysql://root:p@ss@127.0.0.1:3306/app");
        assert!(!out.contains("p@ss"), "password leaked: {out}");
        assert!(out.starts_with("mysql://root:***@"), "{out}");
    }

    #[test]
    fn a_protocol_error_reads_as_a_port_conflict_not_a_locale_problem() {
        // #1306: the exact class of error a shadowing local server
        // produces.
        let err = sqlx::Error::Protocol(
            "Postgres returned a non-UTF-8 string for its error message. This is most likely \
             due to an error that occurred during authentication and the default lc_messages \
             locale is not binary-compatible with UTF-8."
                .into(),
        );
        let d = ConnectDiagnosis::of(
            "postgres://rustango:rustango@localhost:5432/backend_dev",
            &err,
        );

        assert_eq!(d.fault, ConnectFault::UnexpectedServer);
        let rendered = d.to_string();
        assert!(
            rendered.contains("something other than the expected database answered"),
            "{rendered}"
        );
        assert!(rendered.contains("localhost:5432"), "{rendered}");
        assert!(
            !rendered.contains("rustango:rustango"),
            "credentials leaked: {rendered}"
        );
        // The driver's own text is still there for anyone who wants it —
        // just no longer the headline.
        assert!(rendered.contains("lc_messages"), "{rendered}");
    }

    #[test]
    fn a_refused_connection_reads_as_unreachable() {
        let err = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Connection refused (os error 61)",
        ));
        let d = ConnectDiagnosis::of("postgres://app:pw@127.0.0.1:5999/x", &err);
        assert_eq!(d.fault, ConnectFault::Unreachable);
        assert!(d.to_string().contains("nothing is listening"), "{d}");
    }

    #[test]
    fn advice_is_distinct_for_every_fault() {
        let all = [
            ConnectFault::Unreachable,
            ConnectFault::UnexpectedServer,
            ConnectFault::TlsRejected,
            ConnectFault::AuthFailed,
            ConnectFault::NoSuchDatabase,
            ConnectFault::PermissionDenied,
            ConnectFault::Other,
        ];
        let mut seen: Vec<&str> = all.iter().map(|f| f.advice()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "two faults share advice text");
    }
}
