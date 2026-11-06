//! The wall clock in unix seconds, and the one answer for a broken one.
//!
//! A clock that reads before 1970, or before [`CLOCK_FLOOR`](crate::clock::CLOCK_FLOOR)
//! (an unset RTC boots near the epoch), is an error. Verifiers treat it as
//! "expired"; signers pick their fallback explicitly with
//! [`unix_now_or`](crate::clock::unix_now_or).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Earliest believable time, 2024-01-01: anything earlier is an unset clock.
pub const CLOCK_FLOOR: u64 = 1_704_067_200;

/// The system clock reads before [`CLOCK_FLOOR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the system clock is not set (it reads before 2024-01-01)")]
#[non_exhaustive]
pub struct ClockError;

/// Now, in unix seconds.
///
/// # Errors
/// [`ClockError`] when the clock reads before [`CLOCK_FLOOR`].
pub fn unix_now() -> Result<u64, ClockError> {
    #[cfg(test)]
    if BROKEN.with(std::cell::Cell::get) {
        return Err(ClockError);
    }
    checked(SystemTime::now().duration_since(UNIX_EPOCH))
}

/// [`unix_now`], or `fallback` on a broken clock. A verifier passes
/// `u64::MAX` (everything expired), a signer `0` (born expired).
#[must_use]
pub fn unix_now_or(fallback: u64) -> u64 {
    unix_now().unwrap_or(fallback)
}

/// [`unix_now`] as `i64`, for `chrono`-style timestamps.
///
/// # Errors
/// As [`unix_now`].
pub fn unix_now_i64() -> Result<i64, ClockError> {
    unix_now().map(|n| i64::try_from(n).unwrap_or(i64::MAX))
}

/// [`unix_now_i64`] for expiry checks: a broken clock answers `i64::MAX`,
/// so every `exp` has passed.
#[must_use]
pub fn unix_now_for_expiry() -> i64 {
    unix_now_i64().unwrap_or(i64::MAX)
}

fn checked<E>(elapsed: Result<Duration, E>) -> Result<u64, ClockError> {
    elapsed
        .ok()
        .map(|d| d.as_secs())
        .filter(|s| *s >= CLOCK_FLOOR)
        .ok_or(ClockError)
}

#[cfg(test)]
thread_local! {
    static BROKEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with this thread's clock broken.
#[cfg(test)]
pub(crate) fn with_broken_clock<T>(f: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            BROKEN.with(|b| b.set(false));
        }
    }
    BROKEN.with(|b| b.set(true));
    let _reset = Reset;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn before_the_floor_or_1970_is_an_error() {
        assert_eq!(checked::<()>(Ok(Duration::from_secs(5))), Err(ClockError));
        assert_eq!(checked(Err(())), Err(ClockError));
        assert_eq!(
            checked::<()>(Ok(Duration::from_secs(CLOCK_FLOOR))),
            Ok(CLOCK_FLOOR)
        );
        assert!(unix_now().is_ok());
    }

    #[test]
    fn a_broken_clock_reads_as_far_future_for_expiry() {
        with_broken_clock(|| {
            assert_eq!(unix_now_for_expiry(), i64::MAX);
            assert_eq!(unix_now_or(7), 7);
        });
        assert!(unix_now_for_expiry() < i64::MAX);
    }
}
