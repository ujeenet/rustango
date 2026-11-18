//! Who may call a generated route: decided when the router mounts, checked
//! per request. ViewSet and the template views share it, so a surface over a
//! model cannot mount with nobody having said who may use it. Routers over no
//! model take a [`RouteAccess`] argument instead.

use axum::http::request::Parts;

use crate::sql::Pool;

/// One route's access rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Nothing was said: [`require_decided`] refuses to mount it.
    #[default]
    Undecided,
    /// Any caller that reaches the router, behind whatever auth layer the app adds.
    Anonymous,
    /// Any signed-in user.
    #[cfg_attr(
        not(all(feature = "tenancy", feature = "template_views")),
        allow(dead_code)
    )]
    SignedIn,
    /// A signed-in user holding every codename; superusers pass.
    #[cfg_attr(not(feature = "admin"), allow(dead_code))]
    Codenames(Vec<String>),
    /// A signed-in superuser.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Superuser,
}

impl Gate {
    /// The check for one request. `caller` is the signed-in user, if any;
    /// codenames are read from `pool`.
    #[cfg_attr(
        not(all(feature = "tenancy", feature = "template_views")),
        allow(dead_code)
    )]
    pub(crate) async fn check(&self, caller: Option<&Caller>, pool: &Pool) -> Outcome {
        match self {
            Self::Codenames(codenames) => check_codenames(codenames, caller, pool).await,
            other => other.check_caller(caller),
        }
    }

    /// The check for a rule that reads no codenames; codenames deny here.
    fn check_caller(&self, caller: Option<&Caller>) -> Outcome {
        match (self, caller) {
            (Self::Anonymous, _) => Outcome::Allow,
            (Self::SignedIn | Self::Superuser | Self::Codenames(_), None) => {
                Outcome::Unauthenticated
            }
            (Self::SignedIn, Some(_)) => Outcome::Allow,
            (Self::Superuser, Some(c)) if c.is_superuser => Outcome::Allow,
            // Mount refuses `Undecided`; fail closed should one get here anyway.
            _ => Outcome::Forbidden,
        }
    }
}

/// Who may read a router that serves no model, such as the debug panel or
/// metrics. A required argument, so such a router cannot mount undecided.
#[cfg(feature = "admin")]
#[derive(Clone, Debug)]
pub struct RouteAccess(Gate);

#[cfg(feature = "admin")]
impl RouteAccess {
    /// Any caller that reaches the router: mount it on an internal port or
    /// behind your own auth layer.
    #[must_use]
    pub fn allow_anonymous() -> Self {
        Self(Gate::Anonymous)
    }

    /// Only a signed-in superuser, from `require_auth` / `optional_auth`:
    /// `401` without a user, `403` for anyone else or a tenant's credential.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn superuser() -> Self {
        Self(Gate::Superuser)
    }

    /// `router` with this rule checked before each of its routes.
    pub(crate) fn guard(self, router: axum::Router) -> axum::Router {
        if self.0 == Gate::Anonymous {
            return router;
        }
        let gate = std::sync::Arc::new(self.0);
        router.route_layer(axum::middleware::from_fn_with_state(gate, check_route))
    }
}

/// A process-wide route serves no tenant: the caller is checked as on a fixed pool.
#[cfg(feature = "admin")]
async fn check_route(
    axum::extract::State(gate): axum::extract::State<std::sync::Arc<Gate>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (parts, body) = req.into_parts();
    let outcome = match Caller::on_fixed(&parts) {
        Ok(caller) => gate.check_caller(caller.as_ref()),
        Err(refused) => refused,
    };
    match outcome.refusal() {
        Some(resp) => resp,
        None => {
            next.run(axum::extract::Request::from_parts(parts, body))
                .await
        }
    }
}

/// The signed-in user a check runs for.
pub(crate) struct Caller {
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    pub(crate) id: i64,
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    pub(crate) is_superuser: bool,
}

impl Caller {
    /// The user an auth layer (`require_auth`, a Bearer or custom backend) proved.
    fn authenticated(parts: &Parts) -> Option<Self> {
        #[cfg(feature = "tenancy")]
        if let Some(u) = parts
            .extensions
            .get::<crate::tenancy::middleware::AuthenticatedUser>()
        {
            return Some(Self {
                id: u.id,
                is_superuser: u.is_superuser,
            });
        }
        let _ = parts;
        None
    }

    /// The caller on a route whose pool was fixed at mount. That pool holds
    /// no tenant's users, so a tenant's credential (user id, superuser bit)
    /// names someone else there: refused.
    ///
    /// # Errors
    /// `Forbidden` for a principal bound to a tenant, or on a request
    /// routed to one.
    #[cfg_attr(not(feature = "admin"), allow(dead_code))]
    pub(crate) fn on_fixed(parts: &Parts) -> Result<Option<Self>, Outcome> {
        let caller = Self::authenticated(parts);
        #[cfg(feature = "tenancy")]
        {
            let principal = crate::tenancy::Principal::from_parts(parts);
            let tenant_bound = principal.as_ref().is_some_and(|p| p.tenant.is_some())
                || crate::object_permissions::credential_tenant(parts).is_some()
                || routed_to_tenant(&parts.extensions);
            if (caller.is_some() || principal.is_some()) && tenant_bound {
                return Err(Outcome::Forbidden);
            }
        }
        Ok(caller)
    }

    /// The caller on a route serving `org`. A credential bound to another
    /// tenant, or to none, is refused: its user id is not this tenant's.
    ///
    /// # Errors
    /// `Forbidden` for a foreign or untenanted principal.
    #[cfg(feature = "tenancy")]
    pub(crate) fn on_tenant(
        parts: &Parts,
        org: &crate::tenancy::Org,
    ) -> Result<Option<Self>, Outcome> {
        match Binding::of(parts, org) {
            Binding::Foreign | Binding::Untenanted => Err(Outcome::Forbidden),
            Binding::Fits => Ok(Self::authenticated(parts)),
        }
    }
}

/// Whether the request came through tenant routing: a dispatched org, a
/// mounted tenant context, or any marker but `Global`. Its users are a
/// tenant's, never a fixed pool's.
#[cfg(feature = "tenancy")]
#[must_use]
pub(crate) fn routed_to_tenant(ext: &axum::http::Extensions) -> bool {
    use crate::tenancy::RequestTenant;
    ext.get::<crate::tenancy::request_tenant::DispatchedOrg>()
        .is_some()
        || crate::extractors::MountedTenantContext::of(ext).is_some()
        || RequestTenant::get(ext).is_some_and(|t| *t != RequestTenant::Global)
}

/// What a check decided.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Authorised — proceed.
    Allow,
    /// No signed-in user → `401`. Only the `tenancy` engine builds it.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Unauthenticated,
    /// Signed in, but lacks a required codename → `403`.
    Forbidden,
    /// The permission read failed → logged `503`; carries the cause.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Unavailable(String),
}

impl Outcome {
    /// The plain-text answer to a refused request; `None` to proceed.
    pub(crate) fn refusal(self) -> Option<axum::response::Response> {
        use axum::http::StatusCode;
        use axum::response::IntoResponse as _;
        Some(match self {
            Self::Allow => return None,
            Self::Unauthenticated => {
                (StatusCode::UNAUTHORIZED, "authentication required").into_response()
            }
            Self::Forbidden => (StatusCode::FORBIDDEN, "permission denied").into_response(),
            Self::Unavailable(e) => {
                tracing::error!(target: "rustango::access", error = %e, "permission read failed");
                (StatusCode::SERVICE_UNAVAILABLE, "service unavailable").into_response()
            }
        })
    }
}

/// Every codename is required; superusers pass. Without `tenancy` there is
/// no engine, so deny: mount refuses codenames there anyway. 403, not 401:
/// signing in cannot satisfy it, and 401 would send a token client into a
/// refresh loop.
pub(crate) async fn check_codenames(
    codenames: &[String],
    caller: Option<&Caller>,
    pool: &Pool,
) -> Outcome {
    #[cfg(feature = "tenancy")]
    {
        let Some(caller) = caller else {
            return Outcome::Unauthenticated;
        };
        if caller.is_superuser {
            return Outcome::Allow;
        }
        // All of them: a second codename narrows, never widens.
        for cn in codenames {
            match crate::tenancy::permissions::has_perm_pool(caller.id, cn, pool).await {
                Ok(true) => {}
                Ok(false) => return Outcome::Forbidden,
                // A failed read is an outage, not a missing permission.
                Err(e) => return Outcome::Unavailable(e.to_string()),
            }
        }
        Outcome::Allow
    }
    #[cfg(not(feature = "tenancy"))]
    {
        let _ = (codenames, caller, pool);
        Outcome::Forbidden
    }
}

/// Refuse to mount `surface` over `table` while any of its `(action, gate)`
/// routes is undecided, or gated on users a build without `tenancy` cannot
/// read. `fix` names the builder calls this build has.
///
/// # Panics
/// On either; the router builders call it before mounting a route.
pub(crate) fn require_decided(surface: &str, table: &str, routes: &[(&str, &Gate)], fix: &str) {
    let open: Vec<&str> = routes
        .iter()
        .filter(|(_, g)| **g == Gate::Undecided)
        .map(|(action, _)| *action)
        .collect();
    assert!(
        open.is_empty(),
        "{surface} `{table}`: {open:?} would serve any caller, and nothing says that is \
         intended; {fix}"
    );
    assert!(
        !routes
            .iter()
            .any(|(_, g)| matches!(g, Gate::Codenames(c) if c.is_empty())),
        "{surface} `{table}`: an empty codename list would let any signed-in user in; \
         name the codenames, or call `.login_required()` if that is intended"
    );
    let needs_users = routes
        .iter()
        .any(|(_, g)| matches!(g, Gate::SignedIn | Gate::Codenames(_)));
    assert!(
        cfg!(feature = "tenancy") || !needs_users,
        "{surface} `{table}`: permission codenames need the `tenancy` feature; on a \
         single-database app {fix}"
    );
}

/// Whose tenant the request's credential is bound to, against the tenant
/// the request was routed to.
#[cfg(feature = "tenancy")]
pub(crate) enum Binding {
    /// Bound to another tenant (slug or registry id): refuse it.
    Foreign,
    /// A principal with no tenant at all.
    Untenanted,
    /// Bound to this tenant, or no principal.
    Fits,
}

#[cfg(feature = "tenancy")]
impl Binding {
    #[must_use]
    pub(crate) fn of(parts: &Parts, org: &crate::tenancy::Org) -> Self {
        let routed = crate::tenancy::TenantSlug::of(org);
        // The org id too: a credential of a purged tenant is not the heir's.
        let other_org =
            crate::object_permissions::credential_tenant(parts).is_some_and(|c| Some(c) != routed);
        match crate::tenancy::Principal::from_parts(parts).map(|p| p.tenant) {
            Some(Some(t)) if t != org.slug || other_org => Self::Foreign,
            Some(None) => Self::Untenanted,
            _ => Self::Fits,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "[\"delete\"] would serve any caller")]
    fn an_undecided_route_refuses_to_mount() {
        require_decided(
            "DeleteView",
            "post",
            &[("delete", &Gate::Undecided)],
            "say who",
        );
    }

    #[test]
    fn a_decided_surface_mounts() {
        require_decided("ListView", "post", &[("list", &Gate::Anonymous)], "x");
    }

    #[test]
    #[should_panic(expected = "call `.login_required()`")]
    fn empty_codenames_refuse_to_mount() {
        require_decided(
            "ListView",
            "post",
            &[("GET", &Gate::Codenames(Vec::new()))],
            "x",
        );
    }

    #[cfg(not(feature = "tenancy"))]
    #[test]
    #[should_panic(expected = "need the `tenancy` feature")]
    fn codenames_need_tenancy() {
        require_decided(
            "ViewSet",
            "post",
            &[("list", &Gate::Codenames(vec!["post.view".into()]))],
            "x",
        );
    }
}
