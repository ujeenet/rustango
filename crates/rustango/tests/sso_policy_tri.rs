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
    admit, AdmitRequest, Admitted, Denied, Identity, Refusal, SignInPolicy, SignupMode, SsoInvite,
    Surface,
};
use rustango::sso::{resolve_by_slug, LinkSource, SsoProvider};
use rustango::tenancy::member_auth::TenantAccounts;
use rustango::tenancy::User;
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

/// `rustango_sso_providers` with only one of the policy columns.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_sso_providers", managed = false)]
#[allow(dead_code)]
pub struct PartialProvider {
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
    #[rustango(max_length = 2000)]
    pub allowed_email_domains: Option<String>,
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
    // Users and roles are shared with other suites: create what is missing.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
    rustango::testkit::matrix::fresh_table::<SsoInvite>(pool).await;
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

/// Some policy columns but not all: a missing one may hold a rule, so
/// every sign-in through the row refuses until `migrate` runs.
async fn a_partly_migrated_table_refuses(pool: &Pool) {
    use rustango::testkit::matrix::fresh_table;
    fresh_table::<PartialProvider>(pool).await;
    PartialProvider {
        id: Auto::default(),
        slug: "half".into(),
        label: "Half".into(),
        kind: "oidc".into(),
        issuer_url: Some("https://idp.example".into()),
        client_id: "cid".into(),
        client_secret: Cast::new("s3cret".into()),
        enabled: true,
        sort_order: 0,
        scopes: None,
        allow_email_link: false,
        allowed_email_domains: Some("client.org".into()),
        created_at: Auto::default(),
        updated_at: Auto::default(),
    }
    .insert_pool(pool)
    .await
    .unwrap();
    let p = resolve_by_slug(pool, "half", String::new())
        .await
        .unwrap()
        .unwrap();
    assert!(p.policy.is_err(), "{:?}", p.policy);
    let key = p.key(LinkSource::Tenant);
    create_link(pool, &key, "s-half", 4).await.unwrap();
    let res = decide(
        pool,
        "half",
        &person("s-half", "a@client.org", serde_json::json!({})),
        &One(Some(Account::new(4, false, true))),
    )
    .await;
    fresh_table::<SsoProvider>(pool).await;
    assert_eq!(res, Err(Denied::Refused(Refusal::PolicyInvalid)));
}

/// A token unique to this run, so rows left in a shared database never meet.
fn tok() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{nanos:x}{}", N.fetch_add(1, Ordering::Relaxed))
}

fn member(sub: &str, email: &str, raw: serde_json::Value) -> Identity {
    Identity::from_profile(NormalizedUser {
        provider: "oidc".into(),
        provider_user_id: sub.into(),
        email: Some(email.into()),
        email_verified: true,
        name: Some("Ann Example".into()),
        avatar_url: None,
        raw,
    })
}

/// `admit` on the tenant console against the real user table.
async fn tenant(pool: &Pool, slug: &str, id: &Identity) -> Result<Admitted, Denied> {
    let p = resolve_by_slug(pool, slug, String::new())
        .await
        .unwrap()
        .unwrap();
    let key = p.key(LinkSource::Tenant);
    let policy = p.effective_policy();
    admit(
        AdmitRequest::new(pool, Surface::TenantAdmin, &key, &policy, id).provider(slug),
        &TenantAccounts(pool),
    )
    .await
}

fn refused(r: Refusal) -> Result<Admitted, Denied> {
    Err(Denied::Refused(r))
}

async fn invite(pool: &Pool, target: &str, roles: Option<&str>, max: Option<i32>) -> SsoInvite {
    let mut i = SsoInvite {
        id: Auto::default(),
        provider_slug: None,
        target: target.into(),
        roles: roles.map(str::to_owned),
        expires_at: None,
        max_uses: max,
        uses: 0,
        used_at: None,
        used_by: None,
        invited_by: None,
        link_user_id: None,
        link_key: None,
        created_at: Auto::default(),
    };
    i.insert_pool(pool).await.unwrap();
    i
}

/// The link-key fingerprint of tenant provider `slug`.
async fn key_of(pool: &Pool, slug: &str) -> String {
    resolve_by_slug(pool, slug, String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant)
        .fingerprint()
}

/// A one-use re-link invite for `user_id` through provider `slug`.
async fn relink_invite(pool: &Pool, slug: &str, email: &str, user_id: i64) -> SsoInvite {
    let mut inv = invite(pool, email, None, Some(1)).await;
    inv.link_user_id = Some(user_id);
    inv.link_key = Some(key_of(pool, slug).await);
    inv.save_pool(pool).await.unwrap();
    inv
}

async fn role(pool: &Pool, name: &str) -> i64 {
    rustango::tenancy::permissions::get_or_create_role_pool(name, "", pool)
        .await
        .unwrap()
}

async fn roles_of(pool: &Pool, user_id: i64) -> Vec<String> {
    let mut names: Vec<String> = rustango::tenancy::permissions::user_roles_pool(user_id, pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    names.sort();
    names
}

async fn user_by_id(pool: &Pool, id: i64) -> User {
    User::objects()
        .filter("id", id)
        .fetch(pool)
        .await
        .unwrap()
        .remove(0)
}

async fn plain_user(pool: &Pool, name: &str, email: &str, superuser: bool) -> i64 {
    let mut u = User {
        username: name.into(),
        email: Some(email.into()),
        is_superuser: superuser,
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.unwrap();
    u.id.get().copied().unwrap()
}

/// Open sign-up with a domain rule: a new user lands with the default role,
/// never a superuser; another domain is refused.
async fn open_signup_with_a_domain_rule(pool: &Pool) {
    let t = tok();
    let viewer = format!("Viewer-{t}");
    role(pool, &viewer).await;
    provider(pool, "open", false).await;
    set(
        pool,
        "open",
        "allowed_email_domains",
        SqlValue::String("client.org".into()),
    )
    .await;
    set(pool, "open", "signup_mode", SqlValue::String("open".into())).await;
    set(
        pool,
        "open",
        "default_roles",
        SqlValue::String(format!("{viewer}, Ghost-{t}")),
    )
    .await;
    let email = format!("ann-{t}@client.org");
    let first = tenant(
        pool,
        "open",
        &member(&format!("s-{t}"), &email, serde_json::json!({})),
    )
    .await
    .unwrap();
    assert!(first.created);
    let u = user_by_id(pool, first.user_id).await;
    assert!(!u.is_superuser && u.active);
    assert_eq!(u.username, format!("ann-{t}"));
    assert_eq!(u.email.as_deref(), Some(email.as_str()));
    assert_eq!(u.data["display_name"], "Ann Example");
    assert_eq!(roles_of(pool, first.user_id).await, [viewer]);
    let link = SsoLink::objects()
        .filter("subject", format!("s-{t}"))
        .fetch(pool)
        .await
        .unwrap()
        .remove(0);
    assert!(link.provisioned);
    let again = tenant(
        pool,
        "open",
        &member(&format!("s-{t}"), &email, serde_json::json!({})),
    )
    .await
    .unwrap();
    assert_eq!((again.user_id, again.created), (first.user_id, false));
    assert_eq!(
        tenant(
            pool,
            "open",
            &member(
                &format!("g-{t}"),
                &format!("eve-{t}@gmail.com"),
                serde_json::json!({})
            )
        )
        .await,
        refused(Refusal::DomainNotAllowed {
            domain: "gmail.com".into()
        })
    );
}

/// Invite only: an email invite signs up with its roles (over the defaults)
/// until used up or expired; a domain invite does not count.
async fn invite_only_signup(pool: &Pool) {
    let t = tok();
    let (viewer, editor) = (format!("Viewer-{t}"), format!("Editor-{t}"));
    role(pool, &viewer).await;
    role(pool, &editor).await;
    provider(pool, "inv", false).await;
    set(
        pool,
        "inv",
        "signup_mode",
        SqlValue::String("invite_only".into()),
    )
    .await;
    set(
        pool,
        "inv",
        "default_roles",
        SqlValue::String(viewer.clone()),
    )
    .await;
    let bob = format!("bob-{t}@client.org");
    let inv = invite(pool, &bob, Some(&editor), Some(1)).await;
    invite(pool, "@client.org", None, None).await;
    let made = tenant(
        pool,
        "inv",
        &member(&format!("b-{t}"), &bob, serde_json::json!({})),
    )
    .await
    .unwrap();
    assert!(made.created);
    assert_eq!(roles_of(pool, made.user_id).await, [editor]);
    let used = SsoInvite::objects()
        .filter("id", *inv.id.get().unwrap())
        .fetch(pool)
        .await
        .unwrap()
        .remove(0);
    assert_eq!((used.uses, used.used_by), (1, Some(made.user_id)));
    assert!(used.used_at.is_some());
    assert_eq!(
        tenant(
            pool,
            "inv",
            &member(
                &format!("c-{t}"),
                &format!("carol-{t}@client.org"),
                serde_json::json!({})
            )
        )
        .await,
        refused(Refusal::NotInvited),
        "a domain invite does not admit under invite_only"
    );
    let erin = format!("erin-{t}@client.org");
    let mut old = invite(pool, &erin, None, None).await;
    old.expires_at = Some(chrono::Utc::now() - chrono::TimeDelta::hours(1));
    old.save_pool(pool).await.unwrap();
    assert_eq!(
        tenant(
            pool,
            "inv",
            &member(&format!("e-{t}"), &erin, serde_json::json!({}))
        )
        .await,
        refused(Refusal::InviteExpired)
    );
    let fred = format!("fred-{t}@client.org");
    let mut gone = invite(pool, &fred, None, Some(1)).await;
    gone.uses = 1;
    gone.save_pool(pool).await.unwrap();
    assert_eq!(
        tenant(
            pool,
            "inv",
            &member(&format!("f-{t}"), &fred, serde_json::json!({}))
        )
        .await,
        refused(Refusal::InviteUsedUp)
    );
}

/// Allowlist: a domain invite admits the domain and only it.
async fn allowlist_by_domain(pool: &Pool) {
    let t = tok();
    provider(pool, "allow", false).await;
    set(
        pool,
        "allow",
        "signup_mode",
        SqlValue::String("allowlist".into()),
    )
    .await;
    invite(pool, &format!("*.c{t}.org"), None, None).await;
    let ok = tenant(
        pool,
        "allow",
        &member(
            &format!("a-{t}"),
            &format!("ann@eu.c{t}.org"),
            serde_json::json!({}),
        ),
    )
    .await
    .unwrap();
    assert!(ok.created);
    assert_eq!(
        tenant(
            pool,
            "allow",
            &member(
                &format!("x-{t}"),
                &format!("ann@c{t}.org"),
                serde_json::json!({})
            )
        )
        .await,
        refused(Refusal::NotInvited),
        "`*.` covers subdomains only"
    );
}

/// Closed, and no mode on the tenant console, refuse an unknown email.
async fn closed_refuses_signup(pool: &Pool) {
    let t = tok();
    provider(pool, "shut", false).await;
    let who = member(
        &format!("s-{t}"),
        &format!("z-{t}@client.org"),
        serde_json::json!({}),
    );
    assert_eq!(
        tenant(pool, "shut", &who).await,
        refused(Refusal::SignupClosed)
    );
    set(
        pool,
        "shut",
        "signup_mode",
        SqlValue::String("closed".into()),
    )
    .await;
    invite(pool, &format!("z-{t}@client.org"), None, None).await;
    assert_eq!(
        tenant(pool, "shut", &who).await,
        refused(Refusal::SignupClosed)
    );
}

/// Several sign-ups race for one use of an invite: exactly one user is made.
async fn one_use_admits_one_racer(pool: &Pool) {
    let t = tok();
    provider(pool, "race", false).await;
    set(
        pool,
        "race",
        "signup_mode",
        SqlValue::String("allowlist".into()),
    )
    .await;
    let domain = format!("r{t}.org");
    invite(pool, &format!("@{domain}"), None, Some(1)).await;
    let ids: Vec<Identity> = (0..6)
        .map(|n| {
            member(
                &format!("r{n}-{t}"),
                &format!("u{n}@{domain}"),
                serde_json::json!({}),
            )
        })
        .collect();
    let runs = futures_util::future::join_all(ids.iter().map(|id| tenant(pool, "race", id))).await;
    let wins = runs.iter().filter(|r| r.is_ok()).count();
    let made = User::objects()
        .filter("email__endswith", format!("@{domain}"))
        .fetch(pool)
        .await
        .unwrap()
        .len();
    assert_eq!((wins, made), (1, 1), "{runs:?}");

    // An unlimited invite admits every racer.
    let open = format!("o{t}.org");
    invite(pool, &format!("@{open}"), None, None).await;
    let ids: Vec<Identity> = (0..2)
        .map(|n| {
            member(
                &format!("o{n}-{t}"),
                &format!("u{n}@{open}"),
                serde_json::json!({}),
            )
        })
        .collect();
    let runs = futures_util::future::join_all(ids.iter().map(|id| tenant(pool, "race", id))).await;
    assert!(runs.iter().all(Result::is_ok), "{runs:?}");
}

/// A superuser is never linked by email, whatever the provider or an invite says;
/// a role holder links only with the staff opt-in.
async fn superusers_are_never_email_linked(pool: &Pool) {
    let t = tok();
    let editor = format!("Editor-{t}");
    let editor_id = role(pool, &editor).await;
    rustango::tenancy::permissions::grant_role_perm_pool(editor_id, "post.change", pool)
        .await
        .unwrap();
    provider(pool, "staff", true).await;
    set(
        pool,
        "staff",
        "allow_email_link_staff",
        SqlValue::Bool(true),
    )
    .await;
    set(
        pool,
        "staff",
        "signup_mode",
        SqlValue::String("open".into()),
    )
    .await;
    let root_email = format!("root-{t}@client.org");
    let root = plain_user(pool, &format!("root-{t}"), &root_email, true).await;
    let who = member(&format!("su-{t}"), &root_email, serde_json::json!({}));
    assert_eq!(
        tenant(pool, "staff", &who).await,
        refused(Refusal::Privileged)
    );
    relink_invite(pool, "staff", &root_email, root).await;
    assert_eq!(
        tenant(pool, "staff", &who).await,
        refused(Refusal::Privileged)
    );
    assert!(SsoLink::objects()
        .filter("user_id", root)
        .fetch(pool)
        .await
        .unwrap()
        .is_empty());

    let ed_email = format!("ed-{t}@client.org");
    let ed = plain_user(pool, &format!("ed-{t}"), &ed_email, false).await;
    rustango::tenancy::permissions::assign_role_pool(ed, editor_id, pool)
        .await
        .unwrap();
    let ed_who = member(&format!("ed-{t}"), &ed_email, serde_json::json!({}));
    set(
        pool,
        "staff",
        "allow_email_link_staff",
        SqlValue::Bool(false),
    )
    .await;
    assert_eq!(
        tenant(pool, "staff", &ed_who).await,
        refused(Refusal::Privileged)
    );
    set(
        pool,
        "staff",
        "allow_email_link_staff",
        SqlValue::Bool(true),
    )
    .await;
    assert_eq!(tenant(pool, "staff", &ed_who).await.unwrap().user_id, ed);
}

/// A re-link invite links its (non-superuser) user once, by verified email.
async fn a_relink_invite_links_its_user_once(pool: &Pool) {
    let t = tok();
    provider(pool, "re", false).await;
    provider(pool, "re2", false).await;
    let email = format!("rl-{t}@client.org");
    let uid = plain_user(pool, &format!("rl-{t}"), &email, false).await;
    relink_invite(pool, "re", &email, uid).await;
    let linked_to_uid = || async {
        SsoLink::objects()
            .filter("user_id", uid)
            .fetch(pool)
            .await
            .unwrap()
            .len()
    };
    // Another provider of the tenant cannot use it.
    assert_eq!(
        tenant(
            pool,
            "re2",
            &member(&format!("x-{t}"), &email, serde_json::json!({}))
        )
        .await,
        refused(Refusal::EmailLinkDisabled)
    );
    assert_eq!(linked_to_uid().await, 0);
    let got = tenant(
        pool,
        "re",
        &member(&format!("new-{t}"), &email, serde_json::json!({})),
    )
    .await
    .unwrap();
    assert_eq!((got.user_id, got.created), (uid, false));
    assert_eq!(
        tenant(
            pool,
            "re",
            &member(&format!("other-{t}"), &email, serde_json::json!({}))
        )
        .await,
        refused(Refusal::EmailLinkDisabled),
        "the invite is used up"
    );

    // A role holder needs the staff opt-in, as for an email link.
    let editor = role(pool, &format!("RE-{t}")).await;
    rustango::tenancy::permissions::grant_role_perm_pool(editor, "post.change", pool)
        .await
        .unwrap();
    let staff_email = format!("st-{t}@client.org");
    let staff = plain_user(pool, &format!("st-{t}"), &staff_email, false).await;
    rustango::tenancy::permissions::assign_role_pool(staff, editor, pool)
        .await
        .unwrap();
    relink_invite(pool, "re", &staff_email, staff).await;
    let who = member(&format!("st-{t}"), &staff_email, serde_json::json!({}));
    assert_eq!(tenant(pool, "re", &who).await, refused(Refusal::Privileged));
    set(pool, "re", "allow_email_link_staff", SqlValue::Bool(true)).await;
    assert_eq!(tenant(pool, "re", &who).await.unwrap().user_id, staff);

    // An invite whose user is gone is ignored and deleted.
    let ghost = format!("gh-{t}@client.org");
    let inv = relink_invite(pool, "re", &ghost, i64::from(i32::MAX)).await;
    assert_eq!(
        tenant(
            pool,
            "re",
            &member(&format!("gh-{t}"), &ghost, serde_json::json!({}))
        )
        .await,
        refused(Refusal::SignupClosed)
    );
    assert!(SsoInvite::objects()
        .filter("id", *inv.id.get().unwrap())
        .fetch(pool)
        .await
        .unwrap()
        .is_empty());
}

/// The role map grants on sign-up and, with sync on, adds and removes only
/// the roles it names.
async fn role_sync_touches_only_mapped_roles(pool: &Pool) {
    let t = tok();
    let (viewer, editor, hand) = (format!("V-{t}"), format!("E-{t}"), format!("H-{t}"));
    role(pool, &viewer).await;
    role(pool, &editor).await;
    let hand_id = role(pool, &hand).await;
    provider(pool, "map", false).await;
    set(pool, "map", "signup_mode", SqlValue::String("open".into())).await;
    set(
        pool,
        "map",
        "default_roles",
        SqlValue::String(viewer.clone()),
    )
    .await;
    set(
        pool,
        "map",
        "role_map",
        SqlValue::Json(serde_json::json!({"g-ed": editor})),
    )
    .await;
    set(pool, "map", "sync_roles_on_login", SqlValue::Bool(true)).await;
    let email = format!("m-{t}@client.org");
    let sub = format!("m-{t}");
    let with =
        |groups: serde_json::Value| member(&sub, &email, serde_json::json!({"groups": groups}));
    let uid = tenant(pool, "map", &with(serde_json::json!(["g-ed"])))
        .await
        .unwrap()
        .user_id;
    let mut want = vec![editor.clone(), viewer.clone()];
    want.sort();
    assert_eq!(roles_of(pool, uid).await, want);
    rustango::tenancy::permissions::assign_role_pool(uid, hand_id, pool)
        .await
        .unwrap();
    tenant(pool, "map", &with(serde_json::json!([])))
        .await
        .unwrap();
    let mut want = vec![hand.clone(), viewer.clone()];
    want.sort();
    assert_eq!(roles_of(pool, uid).await, want, "only the mapped role went");
    tenant(pool, "map", &with(serde_json::json!(["g-ed"])))
        .await
        .unwrap();
    let mut want = vec![editor, hand, viewer];
    want.sort();
    assert_eq!(roles_of(pool, uid).await, want);
    // No groups claim at all: nothing to sync from, the roles stay.
    tenant(pool, "map", &member(&sub, &email, serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(
        roles_of(pool, uid).await,
        want,
        "a missing claim strips nothing"
    );
}

/// A taken username gets a suffix, or refuses.
async fn username_clash_suffix_or_refuse(pool: &Pool) {
    let t = tok();
    plain_user(
        pool,
        &format!("gina-{t}"),
        &format!("other-{t}@x.org"),
        false,
    )
    .await;
    provider(pool, "clash", false).await;
    set(
        pool,
        "clash",
        "signup_mode",
        SqlValue::String("open".into()),
    )
    .await;
    let email = format!("gina-{t}@client.org");
    set(
        pool,
        "clash",
        "username_clash",
        SqlValue::String("refuse".into()),
    )
    .await;
    assert_eq!(
        tenant(
            pool,
            "clash",
            &member(&format!("g1-{t}"), &email, serde_json::json!({}))
        )
        .await,
        refused(Refusal::UsernameTaken)
    );
    set(
        pool,
        "clash",
        "username_clash",
        SqlValue::String("suffix".into()),
    )
    .await;
    let uid = tenant(
        pool,
        "clash",
        &member(&format!("g1-{t}"), &email, serde_json::json!({})),
    )
    .await
    .unwrap()
    .user_id;
    let name = user_by_id(pool, uid).await.username;
    assert!(name.starts_with(&format!("gina-{t}-")), "{name}");

    // A username that is someone's email is taken, in any case.
    let theirs = format!("Mail-{t}@X.org");
    plain_user(pool, &format!("mail-{t}"), &theirs.to_lowercase(), false).await;
    set(
        pool,
        "clash",
        "username_from",
        SqlValue::String("preferred_username".into()),
    )
    .await;
    let uid = tenant(
        pool,
        "clash",
        &member(
            &format!("p1-{t}"),
            &format!("p1-{t}@client.org"),
            serde_json::json!({"preferred_username": theirs}),
        ),
    )
    .await
    .unwrap()
    .user_id;
    assert_ne!(user_by_id(pool, uid).await.username, theirs);
    // Characters a username may not hold refuse, with the generic answer.
    assert_eq!(
        tenant(
            pool,
            "clash",
            &member(
                &format!("p2-{t}"),
                &format!("p2-{t}@client.org"),
                serde_json::json!({"preferred_username": "Ann <b>"})
            ),
        )
        .await,
        refused(Refusal::UsernameTaken)
    );
}

/// The member flow with no mode set follows `auto_provision`, as before.
async fn member_auto_provision_is_unchanged(pool: &Pool) {
    use rustango::tenancy::member_auth::{find_or_provision_member, MemberSignIn};
    let t = tok();
    provider(pool, "mem", false).await;
    let key = resolve_by_slug(pool, "mem", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant);
    let profile = NormalizedUser {
        provider: "oidc".into(),
        provider_user_id: format!("mem-{t}"),
        email: Some(format!("mem-{t}@example.com")),
        email_verified: true,
        name: None,
        avatar_url: None,
        raw: serde_json::json!({}),
    };
    assert_eq!(
        find_or_provision_member(pool, &key, false, &profile, false)
            .await
            .unwrap(),
        MemberSignIn::NoAccount
    );
    let MemberSignIn::Member(id) = find_or_provision_member(pool, &key, false, &profile, true)
        .await
        .unwrap()
    else {
        panic!("auto_provision signs up");
    };
    let u = user_by_id(pool, id).await;
    assert_eq!(u.username, format!("mem-{t}"));
    assert!(!u.is_superuser);
    // Through `admit`, the surface default is the member flow's choice.
    let p = resolve_by_slug(pool, "mem", String::new())
        .await
        .unwrap()
        .unwrap();
    let who = member(
        &format!("mem2-{t}"),
        &format!("mem2-{t}@example.com"),
        serde_json::json!({}),
    );
    for (mode, ok) in [(SignupMode::Closed, false), (SignupMode::Open, true)] {
        let policy = p.effective_policy().default_signup(mode);
        let got = admit(
            AdmitRequest::new(pool, Surface::Member, &key, &policy, &who),
            &TenantAccounts(pool),
        )
        .await;
        assert_eq!(got.is_ok(), ok, "{mode:?}: {got:?}");
    }
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        a_partly_migrated_table_refuses,
        the_default_policy_keeps_link_outcomes,
        stored_rules_are_enforced,
        an_unparseable_rule_refuses,
        pre_migration_tables_still_sign_in,
        open_signup_with_a_domain_rule,
        invite_only_signup,
        allowlist_by_domain,
        closed_refuses_signup,
        one_use_admits_one_racer,
        superusers_are_never_email_linked,
        a_relink_invite_links_its_user_once,
        role_sync_touches_only_mapped_roles,
        username_clash_suffix_or_refuse,
        member_auto_provision_is_unchanged,
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
