//! Generic password hashing + strength checking.
//!
//! argon2id hashing plus a small strength heuristic, with no tenancy
//! types involved. For the tenancy-integrated helpers see
//! [`crate::tenancy::password`].
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::passwords::{hash, verify, strength_score, StrengthIssue};
//!
//! // Signup:
//! let issues = strength_score(&new_password);
//! if !issues.is_empty() {
//!     return Err(format!("password too weak: {:?}", issues));
//! }
//! let hashed = hash(&new_password)?;
//! // Store `hashed` in user row.
//!
//! // Login:
//! let user = users::find_by_email(&email).await?;
//! if !verify(&attempted, &user.password_hash)? {
//!     return Err("bad credentials");
//! }
//! ```
//!
//! From async code call [`hash_async`] / [`verify_async`] /
//! [`verify_dummy_async`]; the sync calls block a runtime worker.

// This module owns the sync calls the lint bans elsewhere.
#![allow(clippy::disallowed_methods)]

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("hashing failed: {0}")]
    Hash(String),
    #[error("verification error: {0}")]
    Verify(String),
    /// No hashing slot freed up in time. Answer 503; the same for a
    /// known and an unknown user.
    #[error("password hashing is busy")]
    Busy,
}

/// Hash a password with argon2id at [`argon2_params`]. Returns a standard PHC string.
///
/// argon2id is deliberately slow and memory-hungry, and every hash
/// gets a fresh random salt, so a stolen table cannot be attacked with
/// precomputed or shared work — each password must be guessed on its
/// own, slowly. Never store a plain or fast hash instead.
///
/// # Errors
/// [`PasswordError::Hash`] on argon2 failures.
pub fn hash(password: &str) -> Result<String, PasswordError> {
    hash_with(argon2_params(), password)
}

fn hash_with(params: Argon2Params, password: &str) -> Result<String, PasswordError> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};

    let salt = SaltString::generate(&mut OsRng);
    params
        .hasher()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))
}

/// Verify a password against an argon2 PHC hash. The comparison is
/// constant time, so timing does not reveal how much of the hash the
/// guess got right.
///
/// # Errors
/// [`PasswordError::Verify`] when `stored_hash` isn't a valid PHC string.
pub fn verify(password: &str, stored_hash: &str) -> Result<bool, PasswordError> {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;

    let parsed =
        PasswordHash::new(stored_hash).map_err(|e| PasswordError::Verify(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// A valid argon2id hash of a fixed throwaway password at the current
/// [`argon2_params`] cost, like a real stored hash. Backs
/// [`verify_dummy`].
fn dummy_hash() -> &'static str {
    dummy_hash_for(argon2_params())
}

fn dummy_hash_for(params: Argon2Params) -> &'static str {
    use std::sync::{PoisonError, RwLock};
    // Rebuilt when the cost changes, so an unknown user costs what a real one does.
    static DUMMY: RwLock<Option<(Argon2Params, &'static str)>> = RwLock::new(None);
    if let Some((p, h)) = *DUMMY.read().unwrap_or_else(PoisonError::into_inner) {
        if p == params {
            return h;
        }
    }
    let h: &'static str = hash_with(params, "rustango-timing-equalization-dummy")
        .expect("argon2id hashing of a fixed dummy input cannot fail")
        .leak();
    *DUMMY.write().unwrap_or_else(PoisonError::into_inner) = Some((params, h));
    h
}

/// Argon2id cost for new hashes. Only a valid combination can be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Params {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

impl Argon2Params {
    /// The argon2 crate default: m=19456 KiB, t=2, p=1 (OWASP floor).
    pub const DEFAULT: Self = Self {
        memory_kib: argon2::Params::DEFAULT_M_COST,
        iterations: argon2::Params::DEFAULT_T_COST,
        parallelism: argon2::Params::DEFAULT_P_COST,
    };

    /// # Errors
    /// [`PasswordError::Hash`] when argon2 rejects the combination.
    pub fn new(memory_kib: u32, iterations: u32, parallelism: u32) -> Result<Self, PasswordError> {
        argon2::Params::new(memory_kib, iterations, parallelism, None)
            .map_err(|e| PasswordError::Hash(e.to_string()))?;
        Ok(Self {
            memory_kib,
            iterations,
            parallelism,
        })
    }

    #[must_use]
    pub fn memory_kib(&self) -> u32 {
        self.memory_kib
    }

    #[must_use]
    pub fn iterations(&self) -> u32 {
        self.iterations
    }

    #[must_use]
    pub fn parallelism(&self) -> u32 {
        self.parallelism
    }

    fn hasher(self) -> argon2::Argon2<'static> {
        let params = argon2::Params::new(self.memory_kib, self.iterations, self.parallelism, None)
            .expect("validated in Argon2Params::new");
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
    }
}

impl Default for Argon2Params {
    fn default() -> Self {
        Self::DEFAULT
    }
}

static ARGON2: crate::boot_slot::BootSlot<Argon2Params> = crate::boot_slot::BootSlot::new();

/// Set the argon2id cost for new hashes. Call at boot. It replaces the
/// `[auth] argon2_*` values; `false` if an earlier call won.
pub fn configure_argon2(params: Argon2Params) -> bool {
    ARGON2.set_explicit(params)
}

/// The `[auth] argon2_*` values; `false` if app code already set them.
// Only `manage` applies settings (#1948).
#[cfg(all(feature = "config", feature = "manage"))]
pub(crate) fn configure_argon2_from_settings(params: Argon2Params) -> bool {
    ARGON2.set_from_settings(params)
}

/// The argon2id cost [`hash`] uses now.
#[must_use]
pub fn argon2_params() -> Argon2Params {
    *ARGON2.get(|| Argon2Params::DEFAULT)
}

/// Do one verification's worth of work and throw the result away.
///
/// Call this on the **user-not-found** and inactive branches of a
/// login. Without it an unknown user answers fast while a real one
/// pays for argon2, and that difference in response time tells an
/// attacker which accounts exist.
pub fn verify_dummy(password: &str) {
    let _ = verify(password, dummy_hash());
}

/// `true` when `stored` is not argon2id v19 at least as strong as
/// [`argon2_params`] on every axis, so a login should store a new hash.
#[must_use]
pub fn needs_rehash(stored: &str) -> bool {
    let Ok(parsed) = argon2::password_hash::PasswordHash::new(stored) else {
        return true;
    };
    let Ok(p) = argon2::Params::try_from(&parsed) else {
        return true;
    };
    let want = argon2_params();
    parsed.algorithm != argon2::Algorithm::Argon2id.ident()
        || parsed.version != Some(argon2::Version::V0x13.into())
        || p.m_cost() < want.memory_kib
        || p.t_cost() < want.iterations
        || p.p_cost() < want.parallelism
}

// ------------------------------------------------------------------ Async variants

/// [`hash`] on the blocking pool. Use this from async code: an inline
/// argon2 call parks a runtime worker for the whole hash.
///
/// # Errors
/// As [`hash`], or [`PasswordError::Busy`] when no slot frees up in time.
pub async fn hash_async(password: &str) -> Result<String, PasswordError> {
    let password = password.to_owned();
    off_runtime(move || hash(&password)).await?
}

/// [`verify`] on the blocking pool.
///
/// # Errors
/// As [`verify`], or [`PasswordError::Busy`].
pub async fn verify_async(password: &str, stored_hash: &str) -> Result<bool, PasswordError> {
    let (password, stored_hash) = (password.to_owned(), stored_hash.to_owned());
    off_runtime(move || verify(&password, &stored_hash)).await?
}

/// [`verify_dummy`] on the blocking pool. It waits for a slot like a
/// real verify, so an unknown user is busy exactly when a known one is.
///
/// # Errors
/// [`PasswordError::Busy`] when no slot frees up in time.
pub async fn verify_dummy_async(password: &str) -> Result<(), PasswordError> {
    let password = password.to_owned();
    off_runtime(move || verify_dummy(&password)).await
}

/// A fresh hash of `password` when `stored` is below today's cost. Call
/// only after a successful verify; `None` when current or busy.
pub async fn rehash_async(password: &str, stored: &str) -> Option<String> {
    if !needs_rehash(stored) {
        return None;
    }
    hash_async(password).await.ok()
}

/// After a successful login on `model` row `id`, store a fresh hash when
/// `stored` is weak. Returns the hash now in force; a concurrent change wins.
pub async fn upgrade_stored_hash(
    pool: &crate::sql::Pool,
    model: &'static crate::core::ModelSchema,
    id: i64,
    password: &str,
    stored: &str,
) -> String {
    let Some(new) = rehash_async(password, stored).await else {
        return stored.to_owned();
    };
    store_rehash(pool, model, id, stored, new).await
}

/// Store `new` over `stored` on row `id`; the hash now in force.
pub(crate) async fn store_rehash(
    pool: &crate::sql::Pool,
    model: &'static crate::core::ModelSchema,
    id: i64,
    stored: &str,
    new: String,
) -> String {
    let q = rehash_update(model, id, stored, &new, None);
    let applied = crate::sql::update_pool(pool, &q).await;
    rehash_applied(applied, model, id, stored, new)
}

/// A password change: store `new` over `old` and stamp `password_changed_at`,
/// writing no other column, so a deactivate or demote meanwhile stands
/// (#2467). `false` when the password changed meanwhile.
///
/// # Errors
/// [`crate::sql::ExecError::UnsavedRow`] for a row never saved; driver failures.
#[cfg(feature = "tenancy")]
pub(crate) async fn store_password_change(
    pool: &crate::sql::Pool,
    model: &'static crate::core::ModelSchema,
    id: &crate::sql::Auto<i64>,
    old: &str,
    new: &str,
) -> Result<bool, crate::sql::ExecError> {
    let id = id
        .get()
        .copied()
        .ok_or(crate::sql::ExecError::UnsavedRow { table: model.table })?;
    let q = rehash_update(model, id, old, new, Some(chrono::Utc::now()));
    Ok(crate::sql::update_pool(pool, &q).await? == 1)
}

/// `UPDATE model SET password_hash = new WHERE id = ? AND password_hash = old`,
/// also stamping `password_changed_at` when `changed_at` is given.
pub(crate) fn rehash_update(
    model: &'static crate::core::ModelSchema,
    id: i64,
    old: &str,
    new: &str,
    changed_at: Option<chrono::DateTime<chrono::Utc>>,
) -> crate::core::UpdateQuery {
    use crate::core::{Assignment, Expr, Filter, Op, SqlValue, UpdateQuery, WhereExpr};
    let eq = |column, value| {
        WhereExpr::Predicate(Filter {
            column,
            op: Op::Eq,
            value,
        })
    };
    let mut set = vec![Assignment {
        column: "password_hash",
        value: Expr::Literal(SqlValue::String(new.to_owned())),
    }];
    if let Some(at) = changed_at {
        set.push(Assignment::new(
            "password_changed_at",
            SqlValue::DateTime(at),
        ));
    }
    UpdateQuery {
        model,
        set,
        where_clause: WhereExpr::And(vec![
            eq("id", SqlValue::I64(id)),
            eq("password_hash", SqlValue::String(old.to_owned())),
        ]),
    }
}

/// The hash in force after running [`rehash_update`].
pub(crate) fn rehash_applied<E: std::fmt::Display>(
    applied: Result<u64, E>,
    model: &'static crate::core::ModelSchema,
    id: i64,
    stored: &str,
    new: String,
) -> String {
    match applied {
        Ok(1) => new,
        Ok(_) => stored.to_owned(),
        Err(e) => {
            tracing::warn!(target: "rustango::passwords", table = model.table, id, error = %e, "cannot store the upgraded password hash");
            stored.to_owned()
        }
    }
}

/// Default for how long a hash job waits for a free slot.
pub const DEFAULT_HASH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

static HASH_WAIT: crate::boot_slot::BootSlot<std::time::Duration> =
    crate::boot_slot::BootSlot::new();

/// Set how long a hash job waits for a slot before [`PasswordError::Busy`].
/// Call at boot. It replaces the `[auth] hash_wait_ms` value; `false`
/// if an earlier call won.
pub fn configure_hash_wait(wait: std::time::Duration) -> bool {
    HASH_WAIT.set_explicit(wait)
}

/// The `[auth] hash_wait_ms` value; `false` if app code already set one.
// Only `manage` applies settings (#1948).
#[cfg(all(feature = "config", feature = "manage"))]
pub(crate) fn configure_hash_wait_from_settings(wait: std::time::Duration) -> bool {
    HASH_WAIT.set_from_settings(wait)
}

/// How long a hash job currently waits for a slot.
#[must_use]
pub fn hash_wait() -> std::time::Duration {
    *HASH_WAIT.get(|| DEFAULT_HASH_WAIT)
}

/// Which share of the [`HashQueue`] a job may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashLane {
    /// Login forms, JWT login, password changes: every slot.
    Login,
    /// Credentials sent per request (HTTP Basic, API keys, agent
    /// secrets): at most half the slots, so a flood of them cannot
    /// starve the login forms.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Credential,
}

/// At most `slots` argon2 jobs at once, [`HashLane::Credential`] jobs
/// at most half of them; a job that cannot get a slot in time gives up
/// with [`PasswordError::Busy`].
pub(crate) struct HashQueue {
    #[cfg_attr(not(feature = "testkit"), allow(dead_code))]
    size: usize,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    credential: std::sync::Arc<tokio::sync::Semaphore>,
}

impl HashQueue {
    pub(crate) fn new(slots: usize) -> Self {
        let slots = slots.max(1);
        Self {
            size: slots,
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(slots)),
            credential: std::sync::Arc::new(tokio::sync::Semaphore::new((slots / 2).max(1))),
        }
    }

    /// Run `f` on the blocking pool once a slot is free, waiting at most
    /// `wait` in all. A panic in `f` resumes in the caller, as inline.
    pub(crate) async fn run<T, F>(
        &self,
        lane: HashLane,
        wait: std::time::Duration,
        f: F,
    ) -> Result<T, PasswordError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let deadline = tokio::time::Instant::now() + wait;
        let acquire = |s: &std::sync::Arc<tokio::sync::Semaphore>| {
            tokio::time::timeout_at(deadline, std::sync::Arc::clone(s).acquire_owned())
        };
        let lane_permit = match lane {
            HashLane::Login => None,
            HashLane::Credential => Some(
                acquire(&self.credential)
                    .await
                    .map_err(|_| PasswordError::Busy)?
                    .expect("password semaphore is never closed"),
            ),
        };
        let permit = acquire(&self.slots)
            .await
            .map_err(|_| PasswordError::Busy)?
            .expect("password semaphore is never closed");
        // The permits move into the job, so a dropped caller still holds
        // its slot until the hash finishes.
        Ok(tokio::task::spawn_blocking(move || {
            let _permits = (permit, lane_permit);
            f()
        })
        .await
        .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic())))
    }
}

/// Run argon2 work on the process-wide [`HashQueue`], one slot per CPU.
pub(crate) async fn off_runtime<T, F>(f: F) -> Result<T, PasswordError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    off_runtime_in(HashLane::Login, f).await
}

/// [`off_runtime`] in `lane`.
pub(crate) async fn off_runtime_in<T, F>(lane: HashLane, f: F) -> Result<T, PasswordError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    queue().run(lane, hash_wait(), f).await
}

/// The process-wide [`HashQueue`], one slot per CPU.
fn queue() -> &'static HashQueue {
    static QUEUE: std::sync::OnceLock<HashQueue> = std::sync::OnceLock::new();
    QUEUE.get_or_init(|| {
        HashQueue::new(std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get))
    })
}

/// Hold every hashing slot until the permit drops, so each hash job
/// answers [`PasswordError::Busy`]. For tests of the busy path.
#[cfg(feature = "testkit")]
pub async fn hold_all_hash_slots() -> tokio::sync::OwnedSemaphorePermit {
    let q = queue();
    std::sync::Arc::clone(&q.slots)
        .acquire_many_owned(u32::try_from(q.size).unwrap_or(u32::MAX))
        .await
        .expect("password semaphore is never closed")
}

// ------------------------------------------------------------------ Strength check

/// One thing wrong with a candidate password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrengthIssue {
    /// Shorter than the recommended 12 characters.
    TooShort,
    /// Contains only letters (no digits or symbols).
    NoDigitsOrSymbols,
    /// Uses only lowercase letters (no uppercase, digits, or symbols).
    NoVariety,
    /// Matches a list of well-known weak passwords.
    KnownWeak,
}

/// Score a candidate password. An empty `Vec` means no issue found.
///
/// The rules are deliberately simple — nudges, not a policy gate. Pair
/// them with HIBP / pwned-passwords for a real deployment.
/// - Length < 12 → [`StrengthIssue::TooShort`]
/// - No digit or symbol → [`StrengthIssue::NoDigitsOrSymbols`]
/// - Lowercase letters only → [`StrengthIssue::NoVariety`]
/// - On the built-in weak list → [`StrengthIssue::KnownWeak`]
#[must_use]
pub fn strength_score(password: &str) -> Vec<StrengthIssue> {
    let mut issues = Vec::new();

    if password.chars().count() < 12 {
        issues.push(StrengthIssue::TooShort);
    }

    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    let has_symbol = password
        .chars()
        .any(|c| !c.is_alphanumeric() && !c.is_whitespace());
    let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = password.chars().any(|c| c.is_ascii_lowercase());

    if !has_digit && !has_symbol {
        issues.push(StrengthIssue::NoDigitsOrSymbols);
    }
    if !has_digit && !has_symbol && !has_upper && has_lower {
        issues.push(StrengthIssue::NoVariety);
    }

    let lower = password.to_ascii_lowercase();
    if KNOWN_WEAK.iter().any(|&w| w == lower) {
        issues.push(StrengthIssue::KnownWeak);
    }

    issues
}

/// Top weak passwords from public breach lists. Tiny on purpose; real
/// apps should also check HIBP's pwned-passwords API.
const KNOWN_WEAK: &[&str] = &[
    "password",
    "password1",
    "password123",
    "12345678",
    "123456789",
    "qwerty",
    "qwerty123",
    "letmein",
    "admin",
    "admin123",
    "welcome",
    "welcome1",
    "iloveyou",
    "monkey",
    "abc123",
    "111111",
    "000000",
    "passw0rd",
];

/// Awaits `fut` next to a 1 ms ticker and returns its output with the
/// ticks seen meanwhile. On a current-thread runtime inline argon2 sees 0.
#[cfg(test)]
pub(crate) async fn ticks_while<F: std::future::Future>(fut: F) -> (F::Output, usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let ticks = Arc::new(AtomicUsize::new(0));
    let t = Arc::clone(&ticks);
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            t.fetch_add(1, Ordering::Relaxed);
        }
    });
    tokio::task::yield_now().await;
    let before = ticks.load(Ordering::Relaxed);
    let out = fut.await;
    let n = ticks.load(Ordering::Relaxed) - before;
    ticker.abort();
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_match() {
        let h = hash("CorrectHorseBatteryStaple!42").unwrap();
        assert!(verify("CorrectHorseBatteryStaple!42", &h).unwrap());
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let h = hash("real-password").unwrap();
        assert!(!verify("wrong-password", &h).unwrap());
    }

    #[test]
    fn verify_invalid_hash_errors() {
        let r = verify("anything", "not-a-valid-hash");
        assert!(r.is_err());
    }

    #[test]
    fn dummy_hash_is_valid_and_verify_dummy_does_real_work() {
        // The dummy hash must be a valid PHC string. Otherwise
        // verify() returns Err early, skips the argon2 work, and the
        // timing gap it exists to close comes back.
        assert!(!verify("whatever-an-attacker-types", dummy_hash()).unwrap());
        // The public entry point never panics.
        verify_dummy("whatever-an-attacker-types");
    }

    /// On a current-thread runtime an inline hash freezes every other
    /// task; off the runtime a 1 ms ticker keeps running (#1709).
    #[tokio::test(flavor = "current_thread")]
    async fn async_variants_do_not_block_the_runtime() {
        let (h, n) = ticks_while(hash_async("correct horse battery staple")).await;
        assert!(n >= 2, "hash_async stalled the runtime ({n} ticks)");
        let h = h.unwrap();
        let (ok, n) = ticks_while(verify_async("correct horse battery staple", &h)).await;
        assert!(ok.unwrap());
        assert!(n >= 2, "verify_async stalled the runtime ({n} ticks)");
        let (r, n) = ticks_while(verify_dummy_async("nobody")).await;
        assert!(r.is_ok());
        assert!(n >= 2, "verify_dummy_async stalled the runtime ({n} ticks)");
    }

    /// A cost change rebuilds the dummy hash at the new cost.
    #[test]
    fn dummy_hash_follows_the_cost() {
        let a = Argon2Params::new(8, 1, 1).unwrap();
        let b = Argon2Params::new(16, 1, 1).unwrap();
        assert!(dummy_hash_for(a).contains("$m=8,t=1,p=1$"));
        assert!(dummy_hash_for(b).contains("$m=16,t=1,p=1$"));
        assert!(dummy_hash_for(a).contains("$m=8,t=1,p=1$"));
    }

    #[tokio::test]
    async fn off_runtime_resumes_the_original_panic() {
        struct Marker;
        let err = tokio::spawn(off_runtime(|| std::panic::panic_any(Marker)))
            .await
            .unwrap_err();
        assert!(err.into_panic().downcast::<Marker>().is_ok());
    }

    /// A full queue answers `Busy` after the wait instead of queueing
    /// forever, and a freed slot is usable again (#1732).
    #[tokio::test]
    async fn a_full_queue_times_out_then_recovers() {
        let wait = std::time::Duration::from_millis(20);
        let q = HashQueue::new(1);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let hold = tokio::spawn({
            let slots = std::sync::Arc::clone(&q.slots);
            async move {
                let _p = slots.acquire_owned().await.unwrap();
                tokio::task::spawn_blocking(move || rx.recv())
                    .await
                    .unwrap()
            }
        });
        while q.slots.available_permits() > 0 {
            tokio::task::yield_now().await;
        }
        let started = std::time::Instant::now();
        let r = q.run(HashLane::Login, wait, || ()).await;
        assert!(matches!(r, Err(PasswordError::Busy)));
        assert!(started.elapsed() >= wait);
        tx.send(()).unwrap();
        hold.await.unwrap().unwrap();
        assert!(q.run(HashLane::Login, wait, || 7).await.is_ok());
    }

    /// Credential jobs holding their whole share leave the rest of the
    /// slots to logins.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credential_jobs_cannot_take_every_slot() {
        let wait = std::time::Duration::from_millis(50);
        let q = std::sync::Arc::new(HashQueue::new(4));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        let hogs: Vec<_> = (0..6)
            .map(|_| {
                let (q, rx) = (std::sync::Arc::clone(&q), std::sync::Arc::clone(&rx));
                tokio::spawn(async move {
                    q.run(
                        HashLane::Credential,
                        std::time::Duration::from_secs(5),
                        move || {
                            let _ = rx.lock().unwrap().recv();
                        },
                    )
                    .await
                })
            })
            .collect();
        while q.credential.available_permits() > 0 {
            tokio::task::yield_now().await;
        }
        let r = q.run(HashLane::Credential, wait, || ()).await;
        assert!(matches!(r, Err(PasswordError::Busy)), "share is capped");
        assert!(q.run(HashLane::Login, wait, || 1).await.is_ok());
        for _ in 0..6 {
            tx.send(()).unwrap();
        }
        for h in hogs {
            assert!(h.await.unwrap().is_ok());
        }
    }

    #[test]
    fn strong_password_has_no_issues() {
        let issues = strength_score("Tr0ub4dor&3-CorrectBattery");
        assert!(issues.is_empty(), "got issues: {:?}", issues);
    }

    #[test]
    fn short_password_flagged() {
        let issues = strength_score("aB3!");
        assert!(issues.contains(&StrengthIssue::TooShort));
    }

    #[test]
    fn all_letter_password_flagged() {
        let issues = strength_score("abcdefghijklmnop");
        assert!(issues.contains(&StrengthIssue::NoDigitsOrSymbols));
        assert!(issues.contains(&StrengthIssue::NoVariety));
    }

    #[test]
    fn mixed_case_no_digits_only_flags_no_digits() {
        let issues = strength_score("ABCDEFGHIJKLMnop");
        assert!(issues.contains(&StrengthIssue::NoDigitsOrSymbols));
        assert!(!issues.contains(&StrengthIssue::NoVariety));
    }

    #[test]
    fn known_weak_password_flagged() {
        let issues = strength_score("password123");
        assert!(issues.contains(&StrengthIssue::KnownWeak));
    }

    #[test]
    fn known_weak_check_is_case_insensitive() {
        let issues = strength_score("PASSWORD123");
        assert!(issues.contains(&StrengthIssue::KnownWeak));
    }

    #[test]
    fn long_password_with_digit_passes_length_and_variety_check() {
        let issues = strength_score("ThisIsLongEnough1");
        assert!(!issues.contains(&StrengthIssue::TooShort));
        assert!(!issues.contains(&StrengthIssue::NoDigitsOrSymbols));
    }
}
