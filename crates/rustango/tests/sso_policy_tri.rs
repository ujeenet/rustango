//! The SSO sign-in policy on every backend: stored rules are read and
//! enforced, a bad rule refuses, and tables not yet migrated still sign in.

#![cfg(all(feature = "sso", feature = "tenancy"))]

use rustango::casts::{Cast, EncryptedString};
use rustango::core::SqlValue;
use rustango::oauth2::NormalizedUser;
use rustango::sql::{Auto, FetcherPool as _, Pool, UpdaterPool as _};
use rustango::sso::link::{
    create_link, key_sha256, sign_in, Account, AccountLookup, EmailLookup, LinkRefusal, SsoLink,
};
use rustango::sso::policy::{
    admit, AdmitRequest, Denied, Identity, Refusal, SignInPolicy, Surface,
};
use rustango::sso::{resolve_by_slug, LinkSource, SsoProvider};
use rustango::{tri_dialect_test, Model};

/// `rustango_sso_providers` before the policy columns.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_sso_providers", managed = false)]
#[allow(dead_code)]
pub struct LegacyProvider {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)]
    pub slug: String,
    #[rustango(max_length = 150)]
    pub label: String,
    #[rustango(max_length = 32)]
    pub kind: String,
    #[rustango(max_length = 255)]
    pub issuer_url: Option<String>,
    #[rustango(max_length = 255)]
    pub client_id: String,
    #[rustango(max_length = 1024)]
    pub client_secret: Cast<EncryptedString>,
    pub enabled: bool,
    pub sort_order: i32,
    #[rustango(max_length = 255)]
    pub scopes: Option<String>,
    pub allow_email_link: bool,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// `rustango_sso_links` before `provisioned` and `last_used_at`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_sso_links", managed = false)]
#[allow(dead_code)]
pub struct LegacyLink {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16)]
    pub provider_source: String,
    pub provider_id: i64,
    #[rustango(max_length = 300)]
    pub issuer: String,
    #[rustango(max_length = 255)]
    pub subject: String,
    #[rustango(max_length = 64)]
    pub key_sha256: String,
    pub user_id: i64,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::drop_table;
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-policy-tri-key");
    drop_table(pool, "rustango_sso_links").await;
    rustango::testkit::matrix::fresh_table::<SsoProvider>(pool).await;
    rustango::sso::link::ensure_table(pool)
        .await
        .expect("links table");
}

fn person(sub: &str, email: &str, raw: serde_json::Value) -> Identity {
    Identity::from_profile(NormalizedUser {
        provider: "oidc".into(),
        provider_user_id: sub.into(),
        email: Some(email.into()),
        email_verified: true,
        name: None,
        avatar_url: None,
        raw,
    })
}

/// One fake account, returned for any id or email.
struct One(Option<Account>);

impl AccountLookup for One {
    async fn by_id(&self, _id: i64) -> Result<Option<Account>, String> {
        Ok(self.0)
    }
    async fn by_email(&self, _email: &str) -> Result<EmailLookup, String> {
        Ok(self.0.map_or(EmailLookup::Missing, EmailLookup::Found))
    }
}

async fn provider(pool: &Pool, slug: &str, allow_email_link: bool) {
    SsoProvider {
        id: Auto::default(),
        slug: slug.into(),
        label: slug.into(),
        kind: "oidc".into(),
        issuer_url: Some("https://idp.example".into()),
        client_id: "cid".into(),
        client_secret: Cast::new("s3cret".into()),
        enabled: true,
        sort_order: 0,
        scopes: None,
        allow_email_link,
        allowed_email_domains: None,
        blocked_email_domains: None,
        allowed_idp_tenants: None,
        allowed_hosted_domains: None,
        required_claims: serde_json::json!({}),
        require_verified_email: true,
        signup_mode: None,
        default_roles: None,
        role_map: serde_json::json!({}),
        role_claims: None,
        sync_roles_on_login: false,
        username_from: "email_local".into(),
        username_clash: "suffix".into(),
        sync_profile: false,
        allow_email_link_staff: false,
        sso_only: false,
        auto_deactivate_after_days: None,
        session_ttl_secs: None,
        superuser_session_ttl_secs: None,
        created_at: Auto::default(),
        updated_at: Auto::default(),
    }
    .insert_pool(pool)
    .await
    .expect("insert provider");
}

/// Write one column straight through the ORM, past the admin's parsers.
async fn set(pool: &Pool, slug: &str, column: &'static str, value: SqlValue) {
    SsoProvider::objects()
        .filter("slug", slug.to_owned())
        .update()
        .set(column, value)
        .execute_pool(pool)
        .await
        .expect("update provider");
}

async fn decide(
    pool: &Pool,
    slug: &str,
    id: &Identity,
    accounts: &impl AccountLookup,
) -> Result<i64, Denied> {
    let p = resolve_by_slug(pool, slug, "https://x/cb".into())
        .await
        .unwrap()
        .unwrap();
    let key = p.key(LinkSource::Tenant);
    let policy = p.effective_policy();
    admit(
        AdmitRequest::new(pool, Surface::TenantAdmin, &key, &policy, id),
        accounts,
    )
    .await
    .map(|a| a.user_id)
}

/// The legacy token for a link refusal, as [`Refusal::rule`] spells it.
fn legacy_rule(e: &LinkRefusal) -> &'static str {
    match e {
        LinkRefusal::NoSubject => "no_subject",
        LinkRefusal::Unverified => "email_unverified",
        LinkRefusal::NoAccount(_) => "signup_closed",
        LinkRefusal::EmailLinkDisabled => "email_link_disabled",
        LinkRefusal::Privileged => "privileged",
        LinkRefusal::EmailCollides => "email_collides",
        LinkRefusal::Inactive => "inactive",
        _ => "other",
    }
}

/// With no stored rules, `admit` decides exactly what `sign_in` did.
async fn the_default_policy_keeps_link_outcomes(pool: &Pool) {
    provider(pool, "plain", true).await;
    provider(pool, "nolink", false).await;
    let p = resolve_by_slug(pool, "plain", "https://x/cb".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.policy, Ok(SignInPolicy::default()));
    let mut n = 0;
    for slug in ["plain", "nolink"] {
        for (privileged, active, verified, superuser) in [
            (false, true, true, false),
            (true, true, true, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, true),
        ] {
            n += 1;
            let acct = One(Some(
                Account::new(7, privileged, active).with_superuser(superuser),
            ));
            let mut profile = NormalizedUser {
                provider: "oidc".into(),
                provider_user_id: format!("old-{n}"),
                email: Some("Ann@Example.com".into()),
                email_verified: verified,
                name: None,
                avatar_url: None,
                raw: serde_json::json!({}),
            };
            let resolved = resolve_by_slug(pool, slug, "https://x/cb".into())
                .await
                .unwrap()
                .unwrap();
            let key = resolved.key(LinkSource::Tenant);
            let old = sign_in(pool, &key, resolved.allow_email_link, &profile, &acct)
                .await
                .map_err(|e| legacy_rule(&e));
            profile.provider_user_id = format!("new-{n}");
            let new = decide(pool, slug, &Identity::from_profile(profile), &acct)
                .await
                .map_err(|e| match e {
                    Denied::Refused(r) => r.rule(),
                    Denied::Storage(e) => panic!("storage: {e}"),
                    _ => "other",
                });
            assert_eq!(old, new, "{slug} case {n}");
        }
    }
}

/// A linked user is refused once the provider narrows its domains; claims
/// are read from the stored JSON; a sign-in stamps `last_used_at`.
async fn stored_rules_are_enforced(pool: &Pool) {
    provider(pool, "corp", false).await;
    let key = resolve_by_slug(pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant);
    create_link(pool, &key, "s-ann", 7).await.unwrap();
    let ann = One(Some(Account::new(7, false, true)));
    assert_eq!(
        decide(
            pool,
            "corp",
            &person("s-ann", "ann@example.com", serde_json::json!({})),
            &ann
        )
        .await,
        Ok(7)
    );
    let link = SsoLink::objects()
        .filter("subject", "s-ann")
        .fetch(pool)
        .await
        .unwrap()
        .remove(0);
    assert!(link.last_used_at.is_some(), "sign-in stamps last_used_at");

    set(
        pool,
        "corp",
        "allowed_email_domains",
        SqlValue::String("client.org, *.client.org".into()),
    )
    .await;
    set(
        pool,
        "corp",
        "blocked_email_domains",
        SqlValue::String("contractors.client.org".into()),
    )
    .await;
    set(
        pool,
        "corp",
        "required_claims",
        SqlValue::Json(serde_json::json!({"groups": ["g1"]})),
    )
    .await;
    let refused = |r| Err(Denied::Refused(r));
    assert_eq!(
        decide(
            pool,
            "corp",
            &person("s-ann", "ann@example.com", serde_json::json!({})),
            &ann
        )
        .await,
        refused(Refusal::DomainNotAllowed {
            domain: "example.com".into()
        })
    );
    assert_eq!(
        decide(
            pool,
            "corp",
            &person("s-ann", "ann@evilclient.org", serde_json::json!({})),
            &ann
        )
        .await,
        refused(Refusal::DomainNotAllowed {
            domain: "evilclient.org".into()
        })
    );
    assert_eq!(
        decide(
            pool,
            "corp",
            &person(
                "s-ann",
                "ann@contractors.client.org",
                serde_json::json!({"groups": ["g1"]})
            ),
            &ann
        )
        .await,
        refused(Refusal::DomainBlocked {
            rule: "contractors.client.org".into()
        })
    );
    assert_eq!(
        decide(
            pool,
            "corp",
            &person("s-ann", "ann@client.org", serde_json::json!({})),
            &ann
        )
        .await,
        refused(Refusal::Claim {
            name: "groups".into()
        })
    );
    assert_eq!(
        decide(
            pool,
            "corp",
            &person(
                "s-ann",
                "ann@eu.client.org",
                serde_json::json!({"groups": ["x", "g1"]})
            ),
            &ann
        )
        .await,
        Ok(7)
    );
}

/// A stored value the parsers would have refused refuses every sign-in.
async fn an_unparseable_rule_refuses(pool: &Pool) {
    provider(pool, "bad", false).await;
    let key = resolve_by_slug(pool, "bad", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant);
    create_link(pool, &key, "s-bob", 8).await.unwrap();
    set(
        pool,
        "bad",
        "allowed_email_domains",
        SqlValue::String("client.org, gmail".into()),
    )
    .await;
    let p = resolve_by_slug(pool, "bad", String::new())
        .await
        .unwrap()
        .unwrap();
    assert!(p.policy.is_err());
    assert_eq!(
        decide(
            pool,
            "bad",
            &person("s-bob", "bob@client.org", serde_json::json!({})),
            &One(Some(Account::new(8, false, true)))
        )
        .await,
        Err(Denied::Refused(Refusal::PolicyInvalid))
    );
}

/// Provider and link tables from before this release still sign a linked user in.
async fn pre_migration_tables_still_sign_in(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    fresh_table::<LegacyProvider>(pool).await;
    drop_table(pool, "rustango_sso_links").await;
    fresh_table::<LegacyLink>(pool).await;
    LegacyProvider {
        id: Auto::default(),
        slug: "old".into(),
        label: "Old".into(),
        kind: "oidc".into(),
        issuer_url: Some("https://idp.example".into()),
        client_id: "cid".into(),
        client_secret: Cast::new("s3cret".into()),
        enabled: true,
        sort_order: 0,
        scopes: None,
        allow_email_link: false,
        created_at: Auto::default(),
        updated_at: Auto::default(),
    }
    .insert_pool(pool)
    .await
    .unwrap();
    let p = resolve_by_slug(pool, "old", String::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        p.policy,
        Ok(SignInPolicy::default()),
        "absent columns: no rules"
    );
    let key = p.key(LinkSource::Tenant);
    LegacyLink {
        id: Auto::default(),
        provider_source: "tenant".into(),
        provider_id: p.id,
        issuer: key.issuer().to_owned(),
        subject: "s-old".into(),
        key_sha256: key_sha256(key.issuer(), "s-old"),
        user_id: 9,
        created_at: Auto::default(),
    }
    .insert_pool(pool)
    .await
    .unwrap();
    let old = One(Some(Account::new(9, false, true)));
    assert_eq!(
        decide(
            pool,
            "old",
            &person("s-old", "x@example.com", serde_json::json!({})),
            &old
        )
        .await,
        Ok(9)
    );
    // The legacy shapes must not outlive the test in the shared database.
    fresh_table::<SsoProvider>(pool).await;
    drop_table(pool, "rustango_sso_links").await;
    rustango::sso::link::ensure_table(pool).await.unwrap();
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        the_default_policy_keeps_link_outcomes,
        stored_rules_are_enforced,
        an_unparseable_rule_refuses,
        pre_migration_tables_still_sign_in,
    ],
);

/// Every SSO surface signs in through `sso::policy::admit`; only the
/// legacy wrapper in `sso/link.rs` may reach the old entry point.
#[test]
fn no_callback_bypasses_admit() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    let mut offenders = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with("sso/policy/") || rel == "sso/link.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        let imports_it = text.split("sso::link::{").skip(1).any(|rest| {
            rest.split('}')
                .next()
                .is_some_and(|names| names.split([',', ' ', '\n']).any(|n| n == "sign_in"))
        });
        if imports_it || text.contains("link::sign_in(") || text.contains("legacy_sign_in(") {
            offenders.push(rel);
        }
    }
    assert!(
        offenders.is_empty(),
        "SSO sign-in outside admit: {offenders:?}"
    );
}
