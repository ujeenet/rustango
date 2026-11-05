//! The one place cache keys get their tenant: `tenant:#org:<id>:<slug>:<area>:<rest>`.
//!
//! Global keys keep their old bytes, and none of them can start with `tenant:`.

use super::TENANT_PREFIX;

/// What a key is for. Each area is its own subtree under a tenant, so a tenant
/// app `clear()` never frees its locks or idempotency markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Which areas a build writes depends on its features.
pub(crate) enum Area {
    App,
    Lock,
    Idem,
    Page,
    Rl,
    Flag,
    Session,
}

impl Area {
    fn name(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Lock => "lock",
            Self::Idem => "idem",
            Self::Page => "page",
            Self::Rl => "rl",
            Self::Flag => "flag",
            Self::Session => "session",
        }
    }

    /// Global keys with no area name: app keys, and page and rate-limit keys,
    /// which start with their layer's own prefix.
    fn bare(self) -> bool {
        matches!(self, Self::App | Self::Page | Self::Rl)
    }
}

/// Who a key belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeySpace {
    Global,
    #[cfg(feature = "tenancy")]
    Tenant(crate::tenancy::TenantSlug),
}

/// Put in front of a global key that would start with `tenant:` (or with this).
const ESCAPE: char = '~';

impl KeySpace {
    /// The key of `rest` in `area`.
    pub(crate) fn key(&self, area: Area, rest: &str) -> String {
        match self {
            Self::Global => {
                let key = if area.bare() {
                    rest.to_owned()
                } else {
                    format!("{}:{rest}", area.name())
                };
                // Injective: `~` keys get one more, so no two keys meet.
                let tenant_like = key
                    .strip_prefix(TENANT_PREFIX)
                    .is_some_and(|r| r.starts_with(':'));
                if tenant_like || key.starts_with(ESCAPE) {
                    format!("{ESCAPE}{key}")
                } else {
                    key
                }
            }
            #[cfg(feature = "tenancy")]
            Self::Tenant(t) => format!("{TENANT_PREFIX}:{}:{}:{rest}", t.namespace(), area.name()),
        }
    }

    /// The tenant's `#org:<id>:<slug>`; `None` for global.
    pub(crate) fn namespace(&self) -> Option<String> {
        match self {
            Self::Global => None,
            #[cfg(feature = "tenancy")]
            Self::Tenant(t) => Some(t.namespace()),
        }
    }

    #[cfg(feature = "tenancy")]
    pub(crate) fn tenant(&self) -> Option<&crate::tenancy::TenantSlug> {
        match self {
            Self::Global => None,
            Self::Tenant(t) => Some(t),
        }
    }

    /// The 0.63 key of `rest`, still read (locks: also taken) so a pod on 0.63
    /// and one on this release never both run it. Remove in 0.65.0.
    pub(crate) fn legacy_key(&self, area: Area, rest: &str) -> Option<String> {
        let ns = self.namespace()?;
        match area {
            Area::Lock => Some(format!("lock:{TENANT_PREFIX}:{ns}:{rest}")),
            Area::Idem => Some(format!("idem:{rest}")),
            _ => None,
        }
    }

    /// Every key of this tenant, in every area, starts with one of these
    /// (the second is the 0.63 lock layout: remove in 0.65.0). Empty for global.
    #[cfg(feature = "tenancy")]
    pub(crate) fn purge_prefixes(&self) -> Vec<String> {
        self.namespace().map_or_else(Vec::new, |ns| {
            vec![
                format!("{TENANT_PREFIX}:{ns}:"),
                format!("lock:{TENANT_PREFIX}:{ns}:"),
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREAS: [Area; 7] = [
        Area::App,
        Area::Lock,
        Area::Idem,
        Area::Page,
        Area::Rl,
        Area::Flag,
        Area::Session,
    ];

    /// Global keys are byte for byte what each module wrote before.
    #[test]
    fn global_keys_are_unchanged() {
        let got: Vec<String> = AREAS
            .iter()
            .map(|a| KeySpace::Global.key(*a, "x:1"))
            .collect();
        assert_eq!(
            got,
            [
                "x:1",
                "lock:x:1",
                "idem:x:1",
                "x:1",
                "x:1",
                "flag:x:1",
                "session:x:1"
            ]
        );
        assert_eq!(KeySpace::Global.legacy_key(Area::Lock, "j"), None);
    }

    /// No global key, bare or not, can land in a tenant's keyspace.
    #[test]
    fn a_global_key_never_starts_with_tenant() {
        for area in AREAS {
            for rest in ["tenant:#org:1:acme:session:s", "~tenant:x", "~", "tenant:"] {
                let k = KeySpace::Global.key(area, rest);
                assert!(!k.starts_with("tenant:"), "{area:?} {rest}: {k}");
            }
        }
        let app = |r| KeySpace::Global.key(Area::App, r);
        assert_eq!(app("tenant:#org:1:acme:app:k"), "~tenant:#org:1:acme:app:k");
        assert_eq!(app("~tenant:x"), "~~tenant:x");
        assert_eq!(app("~"), "~~");
        assert_eq!(app("tenantx"), "tenantx");
        assert_eq!(KeySpace::Global.key(Area::Page, "tenant|p"), "tenant|p");
    }

    /// The tenant layout of every area.
    #[cfg(feature = "tenancy")]
    #[test]
    fn tenant_keys_have_one_layout() {
        let acme = KeySpace::Tenant(crate::tenancy::TenantSlug::new(1, "acme"));
        let got: Vec<String> = AREAS.iter().map(|a| acme.key(*a, "x:1")).collect();
        assert_eq!(
            got,
            [
                "tenant:#org:1:acme:app:x:1",
                "tenant:#org:1:acme:lock:x:1",
                "tenant:#org:1:acme:idem:x:1",
                "tenant:#org:1:acme:page:x:1",
                "tenant:#org:1:acme:rl:x:1",
                "tenant:#org:1:acme:flag:x:1",
                "tenant:#org:1:acme:session:x:1",
            ]
        );
        assert_eq!(
            acme.legacy_key(Area::Lock, "j").as_deref(),
            Some("lock:tenant:#org:1:acme:j")
        );
        assert_eq!(
            acme.legacy_key(Area::Idem, "ab").as_deref(),
            Some("idem:ab")
        );
        assert_eq!(acme.legacy_key(Area::App, "k"), None);
        assert!(KeySpace::Global.purge_prefixes().is_empty());
        assert_eq!(
            acme.purge_prefixes(),
            ["tenant:#org:1:acme:", "lock:tenant:#org:1:acme:"]
        );
    }
}
