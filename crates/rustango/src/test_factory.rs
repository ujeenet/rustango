//! Test fixture builders, in the style of `factory_boy`.
//!
//! Struct update syntax and `Default::default()` already cover most
//! of what `factory_boy` does in Python:
//!
//! ```ignore
//! let users: Vec<User> = (0..10)
//!     .map(|i| User { name: format!("user-{i}"), ..Default::default() })
//!     .collect();
//! ```
//!
//! This module adds the two pieces that are still useful on top:
//! [`Sequence`] for unique per-call values, and the [`Factory`]
//! trait with [`Factory::build_batch`].
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
//! [`Factory`]: crate::test_factory::Factory
//! [`Factory::build_batch`]: crate::test_factory::Factory::build_batch
//! [`Sequence`]: crate::test_factory::Sequence

use std::sync::atomic::{AtomicU64, Ordering};

/// A counter that gives a fresh value on every [`Sequence::next`]
/// call, like `factory.Sequence(lambda n: ...)`.
///
/// Your closure receives the counter, starting at `0`. The counter
/// is atomic, so parallel callers each get their own value.
///
/// There is no `Clone`: clones would share one counter. Give each
/// factory its own `Sequence::new(...)`.
pub struct Sequence<T> {
    counter: AtomicU64,
    factory: Box<dyn Fn(u64) -> T + Send + Sync>,
}

impl<T> Sequence<T> {
    /// A sequence whose `next()` returns `factory(0)`, `factory(1)`,
    /// and so on.
    pub fn new<F>(factory: F) -> Self
    where
        F: Fn(u64) -> T + Send + Sync + 'static,
    {
        Self {
            counter: AtomicU64::new(0),
            factory: Box::new(factory),
        }
    }

    /// Return the next value and bump the counter.
    pub fn next(&self) -> T {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        (self.factory)(n)
    }

    /// The index the next `next()` call will use.
    #[must_use]
    pub fn current(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    /// Set the counter back to `0`, for tests that need fixed IDs.
    pub fn reset(&self) {
        self.counter.store(0, Ordering::Relaxed);
    }
}

/// A builder for test objects.
///
/// Implement [`Factory::build`]; `build_batch` comes for free.
/// `Item` is what the factory produces, usually a model struct.
pub trait Factory {
    type Item;

    /// Build one object in memory. Most implementations take their
    /// unique values from a [`Sequence`] field.
    fn build(&self) -> Self::Item;

    /// Build `n` objects by calling `build()` in a loop.
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
