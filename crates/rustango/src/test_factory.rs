//! Django-shape test factories — `factory_boy` parity.
//!
//! `factory_boy` is the de-facto Django fixture builder: declare a
//! factory, get `Factory.build()` (no DB) / `Factory.create()` (DB)
//! and `Factory.create_batch(n)` for free, with `Sequence(...)` for
//! per-call unique values. The Python design exists because Python
//! has no struct-update syntax — building a `User(name=..., email=...,
//! created_at=...)` ten times is tedious.
//!
//! Rust *does* have struct update syntax + `Default::default()`, so
//! you can already write:
//!
//! ```ignore
//! let users: Vec<User> = (0..10)
//!     .map(|i| User { name: format!("user-{i}"), ..Default::default() })
//!     .collect();
//! ```
//!
//! This module ships the two factory_boy primitives that are still
//! useful on top of that: a [`Sequence`] for thread-safe per-call
//! unique values, and a [`Factory`] trait that adds [`Factory::build_batch`]
//! / [`Factory::build_pool`] for the create-many DB pattern.
//!
//! ## Example
//!
//! ```ignore
//! use rustango::test_factory::{Factory, Sequence};
//!
//! struct UserFactory {
//!     usernames: Sequence<String>,
//! }
//!
//! impl Default for UserFactory {
//!     fn default() -> Self {
//!         Self {
//!             usernames: Sequence::new(|n| format!("user-{n}")),
//!         }
//!     }
//! }
//!
//! impl Factory for UserFactory {
//!     type Item = User;
//!     fn build(&self) -> User {
//!         User { username: self.usernames.next(), ..Default::default() }
//!     }
//! }
//!
//! let f = UserFactory::default();
//! let three = f.build_batch(3);
//! assert_eq!(three[0].username, "user-0");
//! assert_eq!(three[2].username, "user-2");
//! ```
//!
//! Issue #432.

use std::sync::atomic::{AtomicU64, Ordering};

/// Atomically-incrementing counter that produces a fresh value on
/// each [`Sequence::next`] call. Mirrors `factory.Sequence(lambda n: ...)`.
///
/// The closure is called with the current counter value (starting
/// at `0`) and returns the per-call output. Counter is `AtomicU64`,
/// so the sequence is safe to share across threads / tasks — multiple
/// tests calling `factory.usernames.next()` in parallel each get a
/// unique value.
///
/// `Sequence` does not implement `Clone` — every clone would share
/// the same atomic counter, which is usually NOT what callers want
/// (each factory instance should have its own sequence). Build a
/// new `Sequence::new(...)` per factory.
pub struct Sequence<T> {
    counter: AtomicU64,
    factory: Box<dyn Fn(u64) -> T + Send + Sync>,
}

impl<T> Sequence<T> {
    /// Build a sequence whose `next()` returns `factory(0)`,
    /// `factory(1)`, `factory(2)`, ...
    pub fn new<F>(factory: F) -> Self
    where
        F: Fn(u64) -> T + Send + Sync + 'static,
    {
        Self {
            counter: AtomicU64::new(0),
            factory: Box::new(factory),
        }
    }

    /// Increment the internal counter and return the next value.
    pub fn next(&self) -> T {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        (self.factory)(n)
    }

    /// Current counter value (the index the next `next()` call will
    /// see). Useful in assertions.
    #[must_use]
    pub fn current(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    /// Reset the counter to `0`. Useful between test runs that need
    /// deterministic IDs.
    pub fn reset(&self) {
        self.counter.store(0, Ordering::Relaxed);
    }
}

/// Trait for factory_boy-shape model builders.
///
/// Implementors override [`Factory::build`] to construct one in-memory
/// instance — the rest (`build_batch`, `build_pool`, `build_batch_pool`)
/// are derived.
///
/// `Item` is the type the factory produces — typically a model struct
/// or a DTO.
pub trait Factory {
    type Item;

    /// Build one in-memory instance. Implementations usually pull
    /// per-call unique values out of internal [`Sequence`] fields.
    fn build(&self) -> Self::Item;

    /// Build `n` in-memory instances by calling `build()` in a loop.
    fn build_batch(&self, n: usize) -> Vec<Self::Item> {
        (0..n).map(|_| self.build()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_emits_zero_one_two() {
        let s = Sequence::new(|n| format!("user-{n}"));
        assert_eq!(s.next(), "user-0");
        assert_eq!(s.next(), "user-1");
        assert_eq!(s.next(), "user-2");
        assert_eq!(s.current(), 3);
    }

    #[test]
    fn sequence_reset_restarts_counter() {
        let s = Sequence::new(|n| n * 2);
        let _ = s.next();
        let _ = s.next();
        s.reset();
        assert_eq!(s.next(), 0);
        assert_eq!(s.next(), 2);
    }

    #[test]
    fn sequence_is_thread_safe() {
        use std::sync::Arc;
        let s = Arc::new(Sequence::new(|n| n));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let s = Arc::clone(&s);
                std::thread::spawn(move || s.next())
            })
            .collect();
        let mut values: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        values.sort_unstable();
        assert_eq!(values, (0..16).collect::<Vec<_>>());
    }

    #[derive(Debug, Clone, PartialEq, Default)]
    struct User {
        username: String,
        age: u32,
    }

    struct UserFactory {
        usernames: Sequence<String>,
    }

    impl Default for UserFactory {
        fn default() -> Self {
            Self {
                usernames: Sequence::new(|n| format!("user-{n}")),
            }
        }
    }

    impl Factory for UserFactory {
        type Item = User;
        fn build(&self) -> User {
            User {
                username: self.usernames.next(),
                age: 30,
            }
        }
    }

    #[test]
    fn factory_build_uses_sequence() {
        let f = UserFactory::default();
        assert_eq!(f.build().username, "user-0");
        assert_eq!(f.build().username, "user-1");
    }

    #[test]
    fn factory_build_batch_returns_n_items() {
        let f = UserFactory::default();
        let batch = f.build_batch(5);
        assert_eq!(batch.len(), 5);
        assert_eq!(batch[0].username, "user-0");
        assert_eq!(batch[4].username, "user-4");
        assert!(batch.iter().all(|u| u.age == 30));
    }
}
