//! Who a request acts as, for an object-permission hook: an admin session
//! or an API principal, resolved from extensions a verifying middleware set.

use axum::http::request::Parts;

/// The identity behind a request, as a hook sees it.
///
/// Built only from extensions a verifying middleware set: the admin's
/// session gate, `require_auth`, the MCP agent gate or an app's own
/// [`Principal`](crate::tenancy::Principal). Cookies are never decoded here.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Actor {
    /// A signed-in user: admin session, Bearer token, API key or Basic.
    TenantUser {
        /// `rustango_users.id` (`rustango_admin_users.id` in the bare admin).
        id: i64,
        /// Superuser here. In the admin, `false` under per-user perms.
        is_superuser: bool,
        /// The tenant the credential is bound to.
        tenant: Option<String>,
    },
    /// An operator from the console. Superuser only in the tenant they
    /// impersonate, and only when the request was routed there.
    Operator {
        /// `rustango_operators.id`.
        id: i64,
        /// The tenant this request impersonates.
        impersonating: Option<String>,
    },
    /// A machine token acting for its owner. Never a superuser.
    Agent {
        /// The user who minted the key, if any.
        owner: Option<i64>,
        /// `rustango_agents.id`.
        agent_id: i64,
        /// The tenant the token is pinned to.
        tenant: String,
    },
    /// An admin mounted with no login gate: whoever reaches it acts as
    /// superuser, by configuration. Never built for an API request.
    OpenAdmin,
    /// No credential, or one for another tenant. Never a superuser.
    Anonymous,
}

#[cfg(feature = "tenancy")]
type Routed = crate::tenancy::TenantSlug;

/// Stands in for the routed tenant when there is no tenancy: never present.
#[cfg(not(feature = "tenancy"))]
#[allow(dead_code)]
struct Routed {
    slug: String,
}

/// Whether a credential for `slug` belongs on the `routed` tenant: the org id
/// must match too, so a credential of a purged tenant is nobody under the
/// tenant that reused its slug.
#[cfg_attr(not(feature = "tenancy"), allow(unused_variables))]
fn bound_to(parts: &Parts, slug: Option<&str>, routed: Option<&Routed>) -> bool {
    match (slug, routed) {
        (None, None) => true,
        #[cfg(feature = "tenancy")]
        (Some(slug), Some(routed)) => {
            credential_tenant(parts).is_some_and(|t| t.slug == slug && t == *routed)
        }
        _ => false,
    }
}

/// The tenant, with its org id, an auth layer bound the credential to.
#[cfg(feature = "tenancy")]
fn credential_tenant(parts: &Parts) -> Option<crate::tenancy::TenantSlug> {
    if let Some(t) = parts.extensions.get::<crate::tenancy::TenantSlug>() {
        return Some(t.clone());
    }
    #[cfg(feature = "mcp")]
    if let Some(a) = parts.extensions.get::<crate::mcp::auth::McpAgent>() {
        return Some(crate::tenancy::TenantSlug::new(a.org_id, a.tenant.clone()));
    }
    None
}

impl Actor {
    /// Resolve the actor of an admin or ViewSet request, the two that mark
    /// the routed tenant. A credential bound to another tenant, a credential
    /// with no tenant on a tenant route, or a tenant credential on an
    /// untenanted route resolves to `Anonymous`.
    #[must_use]
    pub fn of(parts: &Parts) -> Self {
        // The tenant the request was routed to: a credential pinned to another counts for nothing.
        #[cfg(feature = "tenancy")]
        let routed = crate::tenancy::RequestTenant::get(&parts.extensions)
            .and_then(crate::tenancy::RequestTenant::tenant);
        #[cfg(not(feature = "tenancy"))]
        let routed: Option<&Routed> = None;
        // The admin router marks its requests: only its session counts there.
        if let Some(scoped) = parts.extensions.get::<crate::admin::session::PermsScoped>() {
            return Self::admin(parts, scoped.0, routed.map(|t| t.slug.as_str()));
        }
        let actor = Self::api(parts);
        if bound_to(parts, actor.tenant(), routed) {
            actor
        } else {
            Self::Anonymous
        }
    }

    /// An admin request: the session, under the admin's per-user perms rule.
    fn admin(parts: &Parts, perms_scoped: bool, routed: Option<&str>) -> Self {
        let Some(s) = crate::admin::session::from_extensions(&parts.extensions) else {
            return if perms_scoped {
                Self::Anonymous
            } else {
                Self::OpenAdmin
            };
        };
        match s.impersonated_by {
            Some(id) => Self::Operator {
                id,
                impersonating: routed.filter(|_| !perms_scoped).map(str::to_owned),
            },
            None => Self::TenantUser {
                id: s.user_id,
                is_superuser: s.is_superuser && !perms_scoped,
                tenant: routed.map(str::to_owned),
            },
        }
    }

    /// An API request: the principal, else an admin session gate's session.
    fn api(parts: &Parts) -> Self {
        #[cfg(feature = "tenancy")]
        if let Some(p) = crate::tenancy::Principal::from_parts(parts) {
            return match (p.kind, p.agent_id) {
                (crate::tenancy::PrincipalKind::Agent, Some(agent_id)) => Self::Agent {
                    owner: Some(p.user_id),
                    agent_id,
                    tenant: p.tenant.unwrap_or_default(),
                },
                _ => Self::TenantUser {
                    id: p.user_id,
                    is_superuser: p.is_superuser,
                    tenant: p.tenant,
                },
            };
        }
        // An agent with no owner has no principal, but is still an agent.
        #[cfg(feature = "mcp")]
        if let Some(a) = parts.extensions.get::<crate::mcp::auth::McpAgent>() {
            return Self::Agent {
                owner: a.user_id,
                agent_id: a.agent_id,
                tenant: a.tenant.clone(),
            };
        }
        match parts
            .extensions
            .get::<crate::admin::session::AdminSession>()
        {
            // Impersonation lives only inside the tenant admin.
            Some(s) if s.impersonated_by.is_none() => Self::TenantUser {
                id: s.user_id,
                is_superuser: s.is_superuser,
                tenant: None,
            },
            _ => Self::Anonymous,
        }
    }

    /// The tenant the credential is bound to.
    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Self::TenantUser { tenant, .. } => tenant.as_deref(),
            Self::Operator { impersonating, .. } => impersonating.as_deref(),
            Self::Agent { tenant, .. } => Some(tenant),
            Self::OpenAdmin | Self::Anonymous => None,
        }
    }

    /// Whether this actor may do what a superuser may.
    #[must_use]
    pub fn is_superuser(&self) -> bool {
        match self {
            Self::TenantUser { is_superuser, .. } => *is_superuser,
            Self::Operator { impersonating, .. } => impersonating.is_some(),
            Self::OpenAdmin => true,
            Self::Agent { .. } | Self::Anonymous => false,
        }
    }
}

/// [`Actor::is_superuser`] of the request's actor: the one superuser
/// rule for object-permission hooks, admin and API alike.
#[must_use]
pub fn is_superuser(parts: &Parts) -> bool {
    Actor::of(parts).is_superuser()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::session::{AdminSession, PermsScoped};

    fn parts() -> Parts {
        axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn with(f: impl FnOnce(&mut axum::http::Extensions)) -> Parts {
        let mut p = parts();
        f(&mut p.extensions);
        p
    }

    /// The marker the dispatcher, the tenant admin and ViewSets set.
    #[cfg(feature = "tenancy")]
    fn routed_to(slug: &str) -> crate::tenancy::RequestTenant {
        crate::tenancy::RequestTenant::Tenant(crate::tenancy::TenantSlug::new(1, slug))
    }

    #[test]
    fn nothing_is_anonymous_and_never_superuser() {
        assert_eq!(Actor::of(&parts()), Actor::Anonymous);
        assert!(!is_superuser(&parts()));
    }

    /// The admin's rule: per-user perms win over a superuser session.
    #[test]
    fn an_admin_session_follows_the_perms_rule() {
        let su = |scoped| {
            with(|e| {
                e.insert(PermsScoped(scoped));
                e.insert(AdminSession::new(3, "a", true));
            })
        };
        assert!(is_superuser(&su(false)));
        assert!(!is_superuser(&su(true)));
        let open = with(|e| {
            e.insert(PermsScoped(false));
        });
        assert_eq!(Actor::of(&open), Actor::OpenAdmin);
        let closed = with(|e| {
            e.insert(PermsScoped(true));
        });
        assert_eq!(Actor::of(&closed), Actor::Anonymous);
    }

    /// Impersonation is superuser only in the tenant the request was routed to.
    #[cfg(feature = "tenancy")]
    #[test]
    fn impersonation_counts_only_for_the_routed_tenant() {
        let imp = |routed: Option<&str>| {
            with(|e| {
                e.insert(PermsScoped(false));
                e.insert(AdminSession::impersonation(9));
                if let Some(t) = routed {
                    e.insert(routed_to(t));
                }
            })
        };
        assert_eq!(
            Actor::of(&imp(Some("acme"))),
            Actor::Operator {
                id: 9,
                impersonating: Some("acme".into())
            }
        );
        assert!(is_superuser(&imp(Some("acme"))));
        assert!(!is_superuser(&imp(None)));
        // Outside the admin an impersonation session is nobody.
        let api = with(|e| {
            e.insert(AdminSession::impersonation(9));
        });
        assert_eq!(Actor::of(&api), Actor::Anonymous);
    }

    #[cfg(feature = "tenancy")]
    #[test]
    fn an_admin_session_outside_the_admin_is_its_user() {
        let p = with(|e| {
            e.insert(AdminSession::new(4, "b", true));
        });
        assert!(is_superuser(&p));
        // A tenant route does not take an untenanted session.
        let routed = with(|e| {
            e.insert(AdminSession::new(4, "b", true));
            e.insert(routed_to("acme"));
        });
        assert_eq!(Actor::of(&routed), Actor::Anonymous);
    }

    #[cfg(feature = "tenancy")]
    #[test]
    fn an_api_user_is_bound_to_its_tenant() {
        use crate::tenancy::{middleware::AuthenticatedUser, TenantSlug};
        let user = |slug: &str, routed: &str| {
            with(|e| {
                e.insert(AuthenticatedUser {
                    id: 7,
                    username: "ada".into(),
                    is_superuser: true,
                });
                e.insert(TenantSlug::new(1, slug));
                e.insert(routed_to(routed));
            })
        };
        assert_eq!(
            Actor::of(&user("acme", "acme")),
            Actor::TenantUser {
                id: 7,
                is_superuser: true,
                tenant: Some("acme".into())
            }
        );
        // A credential for one tenant on the other's route is nobody.
        assert_eq!(Actor::of(&user("acme", "beta")), Actor::Anonymous);
        assert!(!is_superuser(&user("acme", "beta")));
        // Nor on the route of the tenant that reused its slug (org 2).
        let heir = with(|e| {
            e.insert(AuthenticatedUser {
                id: 7,
                username: "ada".into(),
                is_superuser: true,
            });
            e.insert(TenantSlug::new(1, "acme"));
            e.insert(crate::tenancy::RequestTenant::Tenant(TenantSlug::new(
                2, "acme",
            )));
        });
        assert_eq!(Actor::of(&heir), Actor::Anonymous);
    }

    /// A tenant credential on a route with no tenant counts for nothing.
    #[cfg(feature = "tenancy")]
    #[test]
    fn a_tenant_credential_off_a_tenant_route_is_anonymous() {
        use crate::tenancy::{middleware::AuthenticatedUser, TenantSlug};
        let p = with(|e| {
            e.insert(AuthenticatedUser {
                id: 7,
                username: "ada".into(),
                is_superuser: true,
            });
            e.insert(TenantSlug::new(1, "acme"));
        });
        assert_eq!(Actor::of(&p), Actor::Anonymous);
        assert!(!is_superuser(&p));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn an_agent_is_never_superuser() {
        let agent = |owner| {
            with(|e| {
                e.insert(crate::mcp::auth::McpAgent {
                    org_id: 1,
                    agent_id: 5,
                    tenant: "acme".into(),
                    skills: vec![],
                    tools: vec![],
                    user_id: owner,
                    jti: "j".into(),
                    secret_prefix: "p".into(),
                });
                e.insert(routed_to("acme"));
            })
        };
        // A token of org 1 is nobody under org 2 that reused the slug.
        let mut heir = agent(Some(7));
        heir.extensions
            .insert(crate::tenancy::RequestTenant::Tenant(
                crate::tenancy::TenantSlug::new(2, "acme"),
            ));
        assert_eq!(Actor::of(&heir), Actor::Anonymous);
        for owner in [Some(7), None] {
            let p = agent(owner);
            assert_eq!(
                Actor::of(&p),
                Actor::Agent {
                    owner,
                    agent_id: 5,
                    tenant: "acme".into()
                }
            );
            assert!(!is_superuser(&p));
        }
    }
}
