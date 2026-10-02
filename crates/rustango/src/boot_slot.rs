//! A process-wide value set at boot, where an explicit call from app code
//! wins over `[auth]`-style settings, and settings win over the default.

use std::sync::{PoisonError, RwLock};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Default,
    #[cfg_attr(not(all(feature = "config", feature = "manage")), allow(dead_code))]
    Settings,
    Explicit,
}

/// Replaced values are leaked: [`BootSlot::get`] hands out `&'static`,
/// and replacement happens a few times at boot at most.
pub(crate) struct BootSlot<T: 'static> {
    cell: RwLock<Option<(&'static T, Source)>>,
}

impl<T: Send + Sync + 'static> BootSlot<T> {
    pub(crate) const fn new() -> Self {
        Self {
            cell: RwLock::new(None),
        }
    }

    /// The current value, building the default on first use.
    pub(crate) fn get(&self, default: impl FnOnce() -> T) -> &'static T {
        if let Some((v, _)) = *self.cell.read().unwrap_or_else(PoisonError::into_inner) {
            return v;
        }
        let mut cell = self.cell.write().unwrap_or_else(PoisonError::into_inner);
        cell.get_or_insert_with(|| (Box::leak(Box::new(default())), Source::Default))
            .0
    }

    /// Install from app code. Replaces a default or settings value;
    /// `false` when app code already installed one.
    pub(crate) fn set_explicit(&self, v: T) -> bool {
        self.set(v, Source::Explicit)
    }

    /// Install from settings. `false`, keeping the current value, when
    /// app code already installed one.
    #[cfg_attr(not(all(feature = "config", feature = "manage")), allow(dead_code))]
    pub(crate) fn set_from_settings(&self, v: T) -> bool {
        self.set(v, Source::Settings)
    }

    fn set(&self, v: T, source: Source) -> bool {
        let mut cell = self.cell.write().unwrap_or_else(PoisonError::into_inner);
        if matches!(*cell, Some((_, Source::Explicit))) {
            return false;
        }
        *cell = Some((Box::leak(Box::new(v)), source));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::BootSlot;

    #[test]
    fn explicit_beats_settings_in_either_order() {
        let s: BootSlot<u32> = BootSlot::new();
        assert_eq!(*s.get(|| 1), 1);
        assert!(s.set_from_settings(2));
        assert_eq!(*s.get(|| 1), 2);
        assert!(s.set_explicit(3));
        assert!(!s.set_from_settings(4));
        assert!(!s.set_explicit(5));
        assert_eq!(*s.get(|| 1), 3);
    }
}
