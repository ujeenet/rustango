//! Where a sign-in without `?next=` lands: the tenant's ordered landing
//! rules. A landing never refuses a sign-in; a bad or unreadable rule is skipped.

use super::identity::{claim_matches, Identity};
use super::models::SsoLandingRule;
use crate::core::Model as _;
use crate::query::QuerySet;
use crate::sql::{FetcherPool as _, Pool};

/// What one rule matches. Parsed from the row, so a kind never carries
/// the wrong fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LandingMatch {
    /// The user holds this role, by exact name.
    Role(String),
    /// The SSO sign-in carries this claim with this value.
    Claim { name: String, value: String },
    /// Every sign-in: the default landing.
    Any,
}

impl LandingMatch {
    /// Parse the kind columns, trimmed.
    ///
    /// # Errors
    /// An unknown kind, or a field the kind needs or forbids.
    pub(crate) fn parse(
        kind: &str,
        name: Option<&str>,
        value: Option<&str>,
    ) -> Result<Self, String> {
        let set = |v: Option<&str>| {
            v.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let (name, value) = (set(name), set(value));
        match (kind.trim(), name, value) {
            ("role", None, Some(role)) => Ok(Self::Role(role)),
            ("role", Some(_), _) => Err("match_name: empty for a `role` rule".into()),
            ("role", None, None) => Err("match_value: the role name".into()),
            ("claim", Some(name), Some(value)) => Ok(Self::Claim { name, value }),
            ("claim", _, _) => Err("match_name and match_value: both needed".into()),
            ("any", None, None) => Ok(Self::Any),
            ("any", _, _) => Err("match_name and match_value: empty for `any`".into()),
            _ => Err("match_kind: `role`, `claim` or `any`".into()),
        }
    }

    fn matches(&self, who: &Signer<'_>) -> bool {
        match self {
            Self::Role(role) => who.roles.iter().any(|r| r == role),
            // Trusted like a mapped role: a signed id_token's claims only.
            Self::Claim { name, value } => who
                .identity
                .is_some_and(|i| claim_matches(i.role_claim(name), std::slice::from_ref(value))),
            Self::Any => true,
        }
    }
}

/// A stored landing path: a same-site path a browser keeps as written.
///
/// # Errors
/// A scheme, a host, a control character, a `.` or `..` segment, or too long.
pub(crate) fn landing_path(url: &str) -> Result<String, String> {
    use crate::auth_decorators::{has_dot_segment, resolved_path, safe_next};
    const MAX: usize = 500;
    let err = || Err("landing_url: a path on this site starting with `/`".to_owned());
    let Some(path) = safe_next(url) else {
        return err();
    };
    if path.chars().count() > MAX || has_dot_segment(&resolved_path(&path)) {
        return err();
    }
    Ok(path)
}

/// Who is signing in, as the rules see them.
pub(crate) struct Signer<'a> {
    /// Role names held after admit.
    pub roles: &'a [String],
    /// The SSO identity, scoped by the policy; `None` for a password sign-in.
    pub identity: Option<&'a Identity>,
}

#[derive(Debug, Clone)]
struct LandingRule {
    id: i64,
    matcher: LandingMatch,
    url: String,
}

/// One surface's landing rules, in evaluation order.
#[derive(Debug, Default)]
pub(crate) struct LandingRules {
    rules: Vec<LandingRule>,
    /// For the log line only.
    tenant: Option<String>,
}

impl LandingRules {
    /// The rules on `pool`. A missing or unreadable table holds none.
    pub(crate) async fn load(pool: &Pool, tenant: Option<&str>) -> Self {
        let table = SsoLandingRule::SCHEMA.table;
        let tenant_log = tenant.unwrap_or("-");
        // Check first: a schema-mode `search_path` would read `public`'s copy.
        match crate::migrate::try_table_exists_here(pool, table).await {
            Ok(true) => {}
            Ok(false) => return Self::default(),
            Err(e) => {
                tracing::warn!(target: "rustango::sso::landing", tenant = tenant_log, "landing rules unreadable: {e}");
                return Self::default();
            }
        }
        let rows = match QuerySet::<SsoLandingRule>::new().fetch(pool).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(target: "rustango::sso::landing", tenant = tenant_log, "landing rules unreadable: {e}");
                return Self::default();
            }
        };
        let mut rules: Vec<(i32, LandingRule)> = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.id.get().copied().unwrap_or_default();
            let parsed = LandingMatch::parse(
                &row.match_kind,
                row.match_name.as_deref(),
                row.match_value.as_deref(),
            )
            .and_then(|m| Ok((m, landing_path(&row.landing_url)?)));
            match parsed {
                Ok((matcher, url)) => rules.push((row.position, LandingRule { id, matcher, url })),
                Err(_) => skipped(id, tenant_log),
            }
        }
        rules.sort_by_key(|(position, r)| (*position, r.id));
        Self {
            rules: rules.into_iter().map(|(_, r)| r).collect(),
            tenant: tenant.map(str::to_owned),
        }
    }

    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether a rule reads roles, so the caller loads them only then.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    #[must_use]
    pub(crate) fn needs_roles(&self) -> bool {
        self.rules
            .iter()
            .any(|r| matches!(r.matcher, LandingMatch::Role(_)))
    }

    /// The first matching rule's path that `check` accepts for this request.
    pub(crate) fn landing(
        &self,
        who: &Signer<'_>,
        check: impl Fn(&str) -> Option<String>,
    ) -> Option<String> {
        self.rules
            .iter()
            .filter(|r| r.matcher.matches(who))
            .find_map(|r| {
                let to = check(&r.url);
                if to.is_none() {
                    skipped(r.id, self.tenant.as_deref().unwrap_or("-"));
                }
                to
            })
    }
}

/// Only the id and tenant: a rule names roles and claims.
fn skipped(id: i64, tenant: &str) {
    tracing::warn!(target: "rustango::sso::landing", rule_id = id, tenant, "landing rule skipped");
}

/// Admin hook: a rule saves only in the shape [`LandingRules::load`] reads.
pub(crate) fn admin_landing_rule<'a>(
    values: &'a mut Vec<(&'static str, crate::core::SqlValue)>,
    before: Option<&'a serde_json::Value>,
) -> crate::admin::derived_fields::DeriveFuture<'a> {
    use crate::core::SqlValue;
    // The submitted value (an emptied field included), else the stored one.
    let get = |values: &[(&'static str, SqlValue)], col: &str| -> Option<String> {
        match values.iter().find(|(c, _)| *c == col) {
            Some((_, SqlValue::String(s))) => Some(s.clone()),
            Some(_) => None,
            None => before?.get(col)?.as_str().map(str::to_owned),
        }
    };
    let checked = LandingMatch::parse(
        &get(values, "match_kind").unwrap_or_default(),
        get(values, "match_name").as_deref(),
        get(values, "match_value").as_deref(),
    )
    .and_then(|m| {
        Ok((
            m,
            landing_path(&get(values, "landing_url").unwrap_or_default())?,
        ))
    });
    let out = checked.map(|(m, url)| {
        let (kind, name, value) = match m {
            LandingMatch::Role(r) => ("role", None, Some(r)),
            LandingMatch::Claim { name, value } => ("claim", Some(name), Some(value)),
            LandingMatch::Any => ("any", None, None),
        };
        let text = |v: Option<String>| v.map_or(SqlValue::Null, SqlValue::String);
        values.retain(|(c, _)| {
            !matches!(
                *c,
                "match_kind" | "match_name" | "match_value" | "landing_url"
            )
        });
        values.push(("match_kind", SqlValue::String(kind.into())));
        values.push(("match_name", text(name)));
        values.push(("match_value", text(value)));
        values.push(("landing_url", SqlValue::String(url)));
    });
    Box::pin(async move { out })
}

inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_landing_rules",
        derive: admin_landing_rule,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(id: i64, matcher: LandingMatch, url: &str) -> LandingRule {
        LandingRule {
            id,
            matcher,
            url: url.into(),
        }
    }

    fn rules(list: Vec<LandingRule>) -> LandingRules {
        LandingRules {
            rules: list,
            tenant: None,
        }
    }

    fn profile() -> crate::oauth2::NormalizedUser {
        crate::oauth2::NormalizedUser {
            provider: "oidc".into(),
            provider_user_id: "sub-1".into(),
            email: None,
            email_verified: false,
            name: None,
            avatar_url: None,
            raw: json!({}),
        }
    }

    fn identity(claims: serde_json::Value) -> Identity {
        let profile = profile();
        let mut token = claims.as_object().cloned().unwrap_or_default();
        token.insert("sub".into(), json!("sub-1"));
        super::super::identity::test_identity(profile, Some(token), false)
    }

    fn ok(s: &str) -> Option<String> {
        Some(s.to_owned())
    }

    #[test]
    fn parse_needs_the_fields_of_its_kind() {
        use LandingMatch as M;
        assert_eq!(
            M::parse("role", None, Some(" Ops ")),
            Ok(M::Role("Ops".into()))
        );
        assert_eq!(
            M::parse("claim", Some("dept"), Some("eng")),
            Ok(M::Claim {
                name: "dept".into(),
                value: "eng".into()
            })
        );
        assert_eq!(M::parse(" any ", Some(""), None), Ok(M::Any));
        for (k, n, v) in [
            ("role", None, None),
            ("role", Some("x"), Some("y")),
            ("claim", None, Some("y")),
            ("claim", Some("x"), None),
            ("any", None, Some("y")),
            ("provider", None, Some("y")),
            ("", None, None),
        ] {
            assert!(M::parse(k, n, v).is_err(), "{k} {n:?} {v:?}");
        }
    }

    #[test]
    fn landing_path_refuses_what_leaves_the_path() {
        assert_eq!(
            landing_path(" /admin/x?tab=1 "),
            Ok("/admin/x?tab=1".into())
        );
        for bad in [
            "//evil.example",
            "https://x",
            "/\\evil",
            "%2F%2Fevil",
            "/a/../b",
            "/a/%2e%2e/b",
            "/a/./b",
            "javascript:x",
            "/a\tb",
            "admin",
            "",
        ] {
            assert!(landing_path(bad).is_err(), "{bad:?}");
        }
        assert!(landing_path(&format!("/{}", "a".repeat(500))).is_err());
        assert!(landing_path(&format!("/{}", "a".repeat(499))).is_ok());
    }

    #[test]
    fn first_match_wins_and_roles_match_exactly() {
        let r = rules(vec![
            rule(1, LandingMatch::Role("Admin".into()), "/admin/a"),
            rule(2, LandingMatch::Role("ops".into()), "/admin/ops"),
            rule(3, LandingMatch::Any, "/admin/home"),
        ]);
        let land = |roles: &[String]| {
            r.landing(
                &Signer {
                    roles,
                    identity: None,
                },
                ok,
            )
        };
        assert_eq!(land(&["ops".into()]).as_deref(), Some("/admin/ops"));
        assert_eq!(land(&["admin".into()]).as_deref(), Some("/admin/home"));
        assert_eq!(
            land(&["ops".into(), "Admin".into()]).as_deref(),
            Some("/admin/a")
        );
        assert!(r.needs_roles());
        assert!(!rules(vec![rule(1, LandingMatch::Any, "/x")]).needs_roles());
    }

    #[test]
    fn a_refused_path_falls_through_to_the_next_rule() {
        let r = rules(vec![
            rule(1, LandingMatch::Any, "/login"),
            rule(2, LandingMatch::Any, "/admin"),
        ]);
        let who = Signer {
            roles: &[],
            identity: None,
        };
        let check = |u: &str| (u != "/login").then(|| u.to_owned());
        assert_eq!(r.landing(&who, check).as_deref(), Some("/admin"));
        assert_eq!(r.landing(&who, |_| None), None);
    }

    #[test]
    fn claims_match_lists_scalars_and_never_without_an_identity() {
        let c = |name: &str, value: &str| LandingMatch::Claim {
            name: name.into(),
            value: value.into(),
        };
        let id = identity(json!({"groups": ["a", "eng"], "staff": true, "level": 3}));
        let with = Signer {
            roles: &[],
            identity: Some(&id),
        };
        let none = Signer {
            roles: &[],
            identity: None,
        };
        assert!(c("groups", "eng").matches(&with));
        assert!(c("staff", "true").matches(&with));
        assert!(c("level", "3").matches(&with));
        assert!(!c("groups", "Eng").matches(&with));
        assert!(!c("missing", "x").matches(&with));
        assert!(!c("groups", "eng").matches(&none));
    }

    /// When the provider signs id_tokens, a claim only userinfo sent steers nothing.
    #[test]
    fn a_claim_is_trusted_like_a_mapped_role() {
        let mut p = profile();
        p.raw = json!({"dept": "eng"});
        let token = json!({"sub": "sub-1"}).as_object().cloned();
        let signed = super::super::identity::test_identity(p, token, false).with_jwks();
        let m = LandingMatch::Claim {
            name: "dept".into(),
            value: "eng".into(),
        };
        assert!(
            signed.directory_claim("dept").is_some(),
            "the claim is there"
        );
        let who = Signer {
            roles: &[],
            identity: Some(&signed),
        };
        assert!(!m.matches(&who));
    }

    /// A multi-tenant Microsoft claim steers nothing until the policy pins its `tid`.
    #[test]
    fn an_unpinned_microsoft_claim_does_not_match() {
        let profile = profile();
        let token = json!({"sub": "sub-1", "tid": "t1", "dept": "eng"})
            .as_object()
            .cloned();
        let ms = super::super::identity::test_identity(profile, token, true);
        let m = LandingMatch::Claim {
            name: "dept".into(),
            value: "eng".into(),
        };
        let who = |i| Signer {
            roles: &[],
            identity: Some(i),
        };
        assert!(!m.matches(&who(&ms)));
        assert!(m.matches(&who(&ms.clone().pinned(true))));
    }

    #[tokio::test]
    async fn the_admin_saves_only_a_rule_load_reads() {
        use crate::core::SqlValue as V;
        let s = |v: &str| V::String(v.into());
        let mut ok = vec![
            ("position", V::I32(0)),
            ("match_kind", s(" role ")),
            ("match_name", s(" ")),
            ("match_value", s(" ops ")),
            ("landing_url", s(" /admin/ops ")),
        ];
        admin_landing_rule(&mut ok, None).await.unwrap();
        let get = |c: &str| ok.iter().find(|(k, _)| *k == c).unwrap().1.clone();
        assert_eq!(get("match_kind"), s("role"));
        assert_eq!(get("match_name"), V::Null);
        assert_eq!(get("match_value"), s("ops"));
        assert_eq!(get("landing_url"), s("/admin/ops"));
        for (kind, name, value, url) in [
            ("any", "", "", "//evil.example"),
            ("any", "", "", "/a/../b"),
            ("any", "x", "", "/ok"),
            ("role", "", "", "/ok"),
            ("claim", "", "eng", "/ok"),
            ("provider", "", "corp", "/ok"),
        ] {
            let mut bad = vec![
                ("match_kind", s(kind)),
                ("match_name", s(name)),
                ("match_value", s(value)),
                ("landing_url", s(url)),
            ];
            assert!(
                admin_landing_rule(&mut bad, None).await.is_err(),
                "{kind} {url}"
            );
        }
        // An update that sends one column is checked against the stored row.
        let before = json!({"match_kind": "any", "landing_url": "/admin"});
        let mut edit = vec![("match_value", s("x"))];
        assert!(admin_landing_rule(&mut edit, Some(&before)).await.is_err());
        let mut edit = vec![("landing_url", s("/admin/x"))];
        admin_landing_rule(&mut edit, Some(&before)).await.unwrap();
    }

    /// Only a superuser adds, changes, deletes or views a rule: rules name roles and claims.
    #[test]
    fn landing_rules_are_superuser_only_in_the_admin() {
        use crate::admin::object_permissions::is_allowed;
        let parts = |scoped: bool| {
            let mut p = axum::http::Request::builder()
                .body(())
                .unwrap()
                .into_parts()
                .0;
            p.extensions
                .insert(crate::admin::session::PermsScoped(scoped));
            p
        };
        for action in ["add", "change", "delete", "view"] {
            let t = SsoLandingRule::SCHEMA.table;
            assert!(!is_allowed(t, action, &parts(true), None), "{action}");
            assert!(is_allowed(t, action, &parts(false), None), "{action}");
        }
    }

    /// Required claims and landing claims read values through one matcher.
    #[test]
    fn required_claims_and_landing_agree() {
        let id = identity(json!({"groups": ["eng"], "n": 1}));
        for (name, value) in [("groups", "eng"), ("groups", "ops"), ("n", "1"), ("x", "1")] {
            let req = super::super::RequiredClaims::parse(&json!({ name: value })).unwrap();
            let landing = LandingMatch::Claim {
                name: name.into(),
                value: value.into(),
            };
            let who = Signer {
                roles: &[],
                identity: Some(&id),
            };
            assert_eq!(
                req.check(&id).is_ok(),
                landing.matches(&who),
                "{name}={value}"
            );
        }
    }
}
