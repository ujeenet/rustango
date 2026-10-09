//! Argon2id password hashing — used by the registry-scoped
//! [`super::Operator`] and the per-tenant [`super::User`] models.
//!
//! Both identity domains share [`crate::passwords`]: hashes are stored as
//! the standard PHC string (`$argon2id$v=19$m=...,t=...,p=...$salt$hash`)
//! so verification is self-describing — the parameters travel with the
//! hash. New hashes use [`crate::passwords::argon2_params`]: the
//! OWASP cost (m=19456, t=2, p=1) unless `[auth] argon2_*` or
//! [`crate::passwords::configure_argon2`] set another.
//!
//! From async code use the `*_async` variants; the sync calls block a
//! runtime worker.

// This module owns the sync calls the lint bans elsewhere.
#![allow(clippy::disallowed_methods)]

use argon2::password_hash::rand_core::OsRng;

use super::error::TenancyError;
use crate::passwords::PasswordError;

/// Hash a plaintext password with the configured Argon2id parameters.
///
/// Returns the PHC-format string suitable for storing in
/// `Operator.password_hash` / `User.password_hash`.
///
/// # Errors
/// Returns [`TenancyError::Validation`] for empty passwords (refused
/// up front to catch the trivial misuse) or for a downstream argon2
/// error (extremely unlikely).
pub fn hash(plaintext: &str) -> Result<String, TenancyError> {
    if plaintext.is_empty() {
        return Err(TenancyError::Validation(
            "password must not be empty".into(),
        ));
    }
    crate::passwords::hash(plaintext).map_err(|e| match e {
        PasswordError::Hash(m) => TenancyError::Validation(format!("argon2 hash failed: {m}")),
        other => TenancyError::Validation(other.to_string()),
    })
}

/// Generate a random password of the requested length.
///
/// Uses an alphabet of 58 characters (a–z, A–Z, 2–9; ambiguous
/// characters `0`, `O`, `1`, `l`, `I` are excluded so the password
/// can be read aloud or transcribed without confusion). The generator
/// is `OsRng`-backed; output is suitable for one-shot operator-driven
/// resets and for first-boot bootstrap accounts.
///
/// Caller must surface the generated password to the operator —
/// [`hash`] discards it on the way to the database, so a forgotten
/// `--generate` output cannot be recovered.
///
/// # Panics
/// If `length` is zero. Lengths under 12 are accepted but should be
/// avoided for production use.
#[must_use]
pub fn generate(length: usize) -> String {
    assert!(length > 0, "password length must be > 0");
    use argon2::password_hash::rand_core::RngCore;
    // 58 unambiguous chars (no 0/O, 1/l/I).
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = OsRng;
    let mut out = String::with_capacity(length);
    let mut buf = [0u8; 64];
    let mut filled = 0;
    while out.len() < length {
        if filled == 0 {
            rng.fill_bytes(&mut buf);
            filled = buf.len();
        }
        let byte = buf[buf.len() - filled];
        filled -= 1;
        // Reject-and-retry to avoid modulo bias.
        let max = u8::try_from(ALPHABET.len() - 1).expect("alphabet < 256 chars");
        if byte <= max.saturating_mul(255 / max) {
            out.push(ALPHABET[(byte as usize) % ALPHABET.len()] as char);
        }
    }
    out
}

/// Verify a plaintext password against a PHC-format hash.
///
/// Returns `true` for a match, `false` for a mismatch. **Constant-
/// time** in the matching path (argon2's default verifier is
/// constant-time over the digest comparison); we don't add additional
/// timing protections at this layer.
///
/// # Errors
/// Returns [`TenancyError::Validation`] when `phc_hash` is malformed
/// (not a valid PHC string).
pub fn verify(plaintext: &str, phc_hash: &str) -> Result<bool, TenancyError> {
    crate::passwords::verify(plaintext, phc_hash).map_err(|e| match e {
        PasswordError::Verify(m) => {
            TenancyError::Validation(format!("malformed password hash: {m}"))
        }
        other => TenancyError::Validation(other.to_string()),
    })
}

/// Spend a verification's worth of work against a fixed dummy hash and
/// discard the result. Call on the user-not-found / inactive branch of
/// a login flow so timing doesn't reveal whether an account exists
/// (audit H1). Delegates to [`crate::passwords::verify_dummy`], at the
/// cost new hashes get.
pub fn verify_dummy(plaintext: &str) {
    crate::passwords::verify_dummy(plaintext);
}

/// [`hash`] on the blocking pool. Use this from async code.
///
/// # Errors
/// As [`hash`], or [`TenancyError::Busy`] when no hashing slot frees up.
pub async fn hash_async(plaintext: &str) -> Result<String, TenancyError> {
    let plaintext = plaintext.to_owned();
    crate::passwords::off_runtime(move || hash(&plaintext))
        .await
        .map_err(|_| TenancyError::Busy)?
}

/// [`verify`] on the blocking pool. Use this from async code.
///
/// # Errors
/// As [`verify`], or [`TenancyError::Busy`].
pub async fn verify_async(plaintext: &str, phc_hash: &str) -> Result<bool, TenancyError> {
    verify_async_in(HashLane::Login, plaintext, phc_hash).await
}

/// [`verify_dummy`] on the blocking pool. Use this from async code.
///
/// # Errors
/// [`TenancyError::Busy`], exactly when [`verify_async`] would be busy.
pub async fn verify_dummy_async(plaintext: &str) -> Result<(), TenancyError> {
    verify_dummy_async_in(HashLane::Login, plaintext).await
}

pub(crate) use crate::passwords::HashLane;

/// [`verify_async`] in `lane`.
pub(crate) async fn verify_async_in(
    lane: HashLane,
    plaintext: &str,
    phc_hash: &str,
) -> Result<bool, TenancyError> {
    let (plaintext, phc_hash) = (plaintext.to_owned(), phc_hash.to_owned());
    crate::passwords::off_runtime_in(lane, move || verify(&plaintext, &phc_hash))
        .await
        .map_err(|_| TenancyError::Busy)?
}

/// [`verify_dummy_async`] in `lane`.
pub(crate) async fn verify_dummy_async_in(
    lane: HashLane,
    plaintext: &str,
) -> Result<(), TenancyError> {
    let plaintext = plaintext.to_owned();
    crate::passwords::off_runtime_in(lane, move || verify_dummy(&plaintext))
        .await
        .map_err(|_| TenancyError::Busy)
}

/// The first of `rows` whose hash `secret` verifies against. Lookup
/// prefixes are random, not unique, so every row sharing one is tried
/// (#2250). No rows still costs one verify, so an unknown prefix times
/// like a wrong secret.
///
/// # Errors
/// [`TenancyError::Busy`]. A hash that will not parse counts as a miss.
pub(crate) async fn first_verified<T>(
    rows: Vec<T>,
    secret: &str,
    hash: impl Fn(&T) -> &str,
) -> Result<Option<T>, TenancyError> {
    if rows.is_empty() {
        verify_dummy_async_in(HashLane::Credential, secret).await?;
        return Ok(None);
    }
    for row in rows {
        match verify_async_in(HashLane::Credential, secret, hash(&row)).await {
            Ok(true) => return Ok(Some(row)),
            Err(TenancyError::Busy) => return Err(TenancyError::Busy),
            _ => {}
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    use crate::core::Column as _;
    use crate::passwords::ticks_while;
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    use crate::sql::{FetcherPool as _, UpdaterPool as _};

    #[tokio::test(flavor = "current_thread")]
    async fn async_variants_do_not_block_the_runtime() {
        let (h, n) = ticks_while(hash_async("hunter2")).await;
        assert!(n >= 2, "hash_async stalled the runtime ({n} ticks)");
        let (ok, n) = ticks_while(verify_async("hunter2", &h.unwrap())).await;
        assert!(ok.unwrap());
        assert!(n >= 2, "verify_async stalled the runtime ({n} ticks)");
        let (r, n) = ticks_while(verify_dummy_async("hunter2")).await;
        assert!(r.is_ok());
        assert!(n >= 2, "verify_dummy_async stalled the runtime ({n} ticks)");
    }

    /// A user row on a fresh SQLite pool, read once: the copy a password
    /// change holds while Argon2 runs.
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    async fn user_row() -> (crate::sql::Pool, super::super::User) {
        use super::super::User;
        let pool = crate::sql::Pool::connect("sqlite::memory:").await.unwrap();
        crate::testkit::create_tables_for::<User>(&pool)
            .await
            .unwrap();
        let mut u = User {
            username: "ada".into(),
            password_hash: "OLD".into(),
            ..crate::testkit::user()
        };
        u.insert_pool(&pool).await.unwrap();
        (pool, u)
    }

    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    async fn reread(pool: &crate::sql::Pool, id: i64) -> super::super::User {
        use super::super::User;
        User::objects()
            .where_(User::id.eq(id))
            .fetch(pool)
            .await
            .unwrap()
            .remove(0)
    }

    /// Change `row`'s password to `NEW` the way the console and CLI do.
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    async fn change(
        pool: &crate::sql::Pool,
        row: &super::super::User,
    ) -> Result<bool, crate::sql::ExecError> {
        use crate::core::Model as _;
        crate::passwords::store_password_change(
            pool,
            super::super::User::SCHEMA,
            &row.id,
            &row.password_hash,
            "NEW",
        )
        .await
    }

    /// #2467 — an unsaved row is an error, not an UPDATE of id 0.
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    #[tokio::test]
    async fn a_password_change_on_an_unsaved_row_errors() {
        let (pool, _) = user_row().await;
        let unsaved = super::super::User {
            password_hash: "OLD".into(),
            ..crate::testkit::user()
        };
        assert!(matches!(
            change(&pool, &unsaved).await,
            Err(crate::sql::ExecError::UnsavedRow { .. })
        ));
    }

    /// #2467 — a deactivate landing while the hash runs must stand.
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    #[tokio::test]
    async fn a_password_write_keeps_a_concurrent_deactivate() {
        use super::super::User;
        let (pool, stale) = user_row().await;
        let id = *stale.id.get().unwrap();
        User::objects()
            .where_(User::id.eq(id))
            .update()
            .set_typed(User::active.set(false))
            .execute_pool(&pool)
            .await
            .unwrap();
        assert!(change(&pool, &stale).await.unwrap());
        let now = reread(&pool, id).await;
        assert!(!now.active, "the password write undid the deactivate");
        assert_eq!(now.password_hash, "NEW");
        assert!(now.password_changed_at.is_some());
    }

    /// #2467 — a password changed meanwhile is not overwritten.
    #[cfg(all(feature = "sqlite", feature = "testkit"))]
    #[tokio::test]
    async fn a_password_write_loses_to_a_concurrent_change() {
        use super::super::User;
        let (pool, stale) = user_row().await;
        let id = *stale.id.get().unwrap();
        User::objects()
            .where_(User::id.eq(id))
            .update()
            .set_typed(User::password_hash.set("OTHER".to_owned()))
            .execute_pool(&pool)
            .await
            .unwrap();
        assert!(!change(&pool, &stale).await.unwrap());
        assert_eq!(reread(&pool, id).await.password_hash, "OTHER");
    }

    #[test]
    fn hash_and_verify_round_trip() {
        let h = hash("hunter2").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("hunter2", &h).unwrap());
        assert!(!verify("wrong", &h).unwrap());
    }

    #[test]
    fn hash_rejects_empty() {
        let err = hash("").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("must not be empty"), "got: {msg}");
    }

    #[test]
    fn verify_rejects_malformed_hash() {
        let err = verify("hunter2", "not-a-phc-string").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("malformed"), "got: {msg}");
    }

    #[test]
    fn generate_produces_correct_length_and_charset() {
        let p = generate(24);
        assert_eq!(p.len(), 24);
        for c in p.chars() {
            assert!(
                c.is_ascii_alphanumeric(),
                "generated char outside expected alphabet: {c:?}"
            );
            assert!(
                !"0O1lI".contains(c),
                "ambiguous char in generated password: {c:?}"
            );
        }
    }

    #[test]
    fn generate_round_trips_through_hash_and_verify() {
        let p = generate(20);
        let h = hash(&p).unwrap();
        assert!(verify(&p, &h).unwrap());
    }

    #[test]
    fn two_calls_to_generate_differ() {
        // Best-effort uniqueness — collisions on 58^16 are vanishingly rare.
        assert_ne!(generate(16), generate(16));
    }

    #[test]
    fn two_hashes_of_same_password_differ() {
        // Salts are random — same plaintext, different stored hashes,
        // both verify.
        let h1 = hash("same").unwrap();
        let h2 = hash("same").unwrap();
        assert_ne!(h1, h2);
        assert!(verify("same", &h1).unwrap());
        assert!(verify("same", &h2).unwrap());
    }
}
