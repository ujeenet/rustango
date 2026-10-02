//! `RUSTANGO_SESSION_SECRET` has one meaning, not three (#1396).
//!
//! It was read in three places that disagreed about what "long enough"
//! meant:
//!
//! | reader | base64-decoded? | floor applied to |
//! |---|---|---|
//! | `SessionSecret::from_env_or_random` | yes | decoded bytes |
//! | `auth_routes::Config::build_jwt` | **no** | raw string bytes |
//! | `manage check --deploy` | **no** | raw string length |
//!
//! The value that breaks all three at once is not exotic — **32 base64
//! characters**, which several key generators emit and which looks
//! entirely right:
//!
//! ```text
//! 32 base64 characters  ->  24 bytes
//! ```
//!
//! `check --deploy` said "length OK" (32 >= 32). `build_jwt` signed
//! access and refresh tokens with the 32 raw characters, its 32-byte
//! floor measuring the encoded form so a 24-byte key sailed past the
//! assert written to stop exactly that. And the cookie layer decoded,
//! saw 24 bytes, and fell back to a random per-process key — so sessions
//! stopped surviving restarts.
//!
//! Three answers, one variable, and the tool whose job is catching this
//! reported green.
//!
//! Everything now goes through `SessionSecret::from_b64`, so a value
//! `check --deploy` accepts is a value the runtime accepts.

#![cfg(feature = "admin")]

use rustango::session::{SessionSecret, SessionSecretError};

/// The headline value. 32 base64 characters, 24 bytes of key.
const THIRTY_TWO_CHARS_TWENTY_FOUR_BYTES: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// What `openssl rand -base64 32` actually produces: 44 characters, 32 bytes.
const A_REAL_SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

#[test]
fn the_trap_value_is_thirty_two_chars_and_twenty_four_bytes() {
    // Pins the premise, so the tests below cannot quietly stop testing
    // the thing they were written for.
    assert_eq!(THIRTY_TWO_CHARS_TWENTY_FOUR_BYTES.len(), 32);
    assert!(
        matches!(
            SessionSecret::from_b64(THIRTY_TWO_CHARS_TWENTY_FOUR_BYTES),
            Err(SessionSecretError::TooShort { actual: 24 })
        ),
        "32 base64 characters must decode to 24 bytes and be refused"
    );
}

#[test]
fn a_real_secret_is_accepted() {
    assert_eq!(A_REAL_SECRET.len(), 44, "openssl rand -base64 32 shape");
    assert!(
        SessionSecret::from_b64(A_REAL_SECRET).is_ok(),
        "the guard must not reject the thing the docs tell you to generate"
    );
}

#[test]
fn a_non_base64_value_is_refused_however_long() {
    // 40 characters of passphrase. The old `build_jwt` signed with these
    // 40 raw bytes while the cookie layer failed to decode and used a
    // random key — two auth surfaces, two different keys, no error.
    let passphrase = "correct-horse-battery-staple-and-then-som";
    assert!(
        passphrase.len() >= 32,
        "the premise: long enough to clear the old raw-bytes floor"
    );
    assert!(
        matches!(
            SessionSecret::from_b64(passphrase),
            Err(SessionSecretError::BadBase64 { .. })
        ),
        "a long non-base64 value is still not a valid secret — it is the \
         case where JWTs and cookies used to end up with different keys"
    );
}

// `check --deploy` agreeing with the runtime is asserted in
// `migrate::manage`'s unit tests, where `run_deploy_audit` is reachable —
// it is `pub(crate)`. Asserting it here would only be able to call
// `from_b64` twice and compare, which proves nothing about the audit.
