//! `{:?}` of a credential-holding type never prints the secret: a `dbg!`,
//! a `?row` tracing field or a panic message would leak it.

const SECRET: &str = "Hunter2-Debug-Leak-q7Zx";

fn assert_hidden(what: &str, debug: &str) {
    assert!(!debug.contains(SECRET), "{what} leaks: {debug}");
    assert!(debug.contains("[redacted]"), "{what}: {debug}");
}

#[test]
fn database_url_builder_hides_the_password() {
    let b = rustango::env::DatabaseUrlBuilder::new(rustango::env::DbDriver::Postgres)
        .host("db")
        .password(SECRET);
    assert_hidden("DatabaseUrlBuilder", &format!("{b:?}"));
}

#[cfg(feature = "casts")]
#[test]
fn an_encrypted_cast_hides_its_plaintext() {
    use rustango::casts::{Cast, EncryptedString};
    let c: Cast<EncryptedString> = Cast::new(SECRET.to_owned());
    assert_hidden("Cast<EncryptedString>", &format!("{c:?}"));
}

#[cfg(feature = "admin")]
#[test]
fn admin_user_hides_its_hash() {
    let u = rustango::admin::AdminUser {
        id: rustango::sql::Auto::Set(1),
        username: "alice".into(),
        password_hash: SECRET.into(),
        #[cfg(feature = "admin-sso")]
        email: None,
        is_superuser: false,
        active: true,
        created_at: chrono::Utc::now(),
        sessions_revoked_at: None,
    };
    let debug = format!("{u:?}");
    assert_hidden("AdminUser", &debug);
    assert!(debug.contains("alice"), "{debug}");
}

#[cfg(feature = "tenancy")]
#[test]
fn operator_hides_its_hash() {
    let op = rustango::tenancy::Operator {
        id: rustango::sql::Auto::Set(1),
        username: "root".into(),
        password_hash: SECRET.into(),
        active: true,
        created_at: chrono::Utc::now(),
        password_changed_at: None,
        sessions_revoked_at: None,
    };
    assert_hidden("Operator", &format!("{op:?}"));
}
