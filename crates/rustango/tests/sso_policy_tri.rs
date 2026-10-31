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
    admit, AdmitRequest, Admitted, Denied, Identity, Refusal, SharedOverride, SignInPolicy,
    SignupMode, SsoInvite, SsoSharedOverride, SsoTenantPolicy, Surface, TenantSettings,
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

/// With no stored rules, `sign_in` and `admit` keep the link outcomes:
/// only a plain, active, verified account links, and only with opt-in.
async fn the_default_policy_keeps_link_outcomes(pool: &Pool) {
    provider(pool, "plain", true).await;
    provider(pool, "nolink", false).await;
    let p = resolve_by_slug(pool, "plain", "https://x/cb".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.policy, Ok(SignInPolicy::default()));
    let mut n = 0;
    for (slug, want) in [
        (
            "plain",
            [
                Ok(7),
                Err("privileged"),
                Err("inactive"),
                Err("email_unverified"),
                Err("privileged"),
            ],
        ),
        (
            "nolink",
            [
                Err("email_link_disabled"),
                Err("email_link_disabled"),
                Err("email_link_disabled"),
                Err("email_unverified"),
                Err("email_link_disabled"),
            ],
        ),
    ] {
        for ((privileged, active, verified, superuser), want) in [
            (false, true, true, false),
            (true, true, true, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, true),
        ]
        .into_iter()
        .zip(want)
        {
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
            assert_eq!(old, want, "sign_in: {slug} case {n}");
            assert_eq!(new, want, "admit: {slug} case {n}");
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

/// Run `body` on its own task, then `restore` even when it panicked, so a
/// failing scenario never leaves a framework table changed for later suites.
async fn restoring<B, R>(pool: &Pool, body: impl FnOnce(Pool) -> B, restore: impl FnOnce(Pool) -> R)
where
    B: std::future::Future<Output = ()> + Send + 'static,
    R: std::future::Future<Output = ()>,
{
    let res = tokio::spawn(body(pool.clone())).await;
    restore(pool.clone()).await;
    if let Err(e) = res {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// The current provider and link tables, after a scenario swapped in old shapes.
async fn current_sso_tables(pool: Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    fresh_table::<SsoProvider>(&pool).await;
    drop_table(&pool, "rustango_sso_links").await;
    rustango::sso::link::ensure_table(&pool).await.unwrap();
}

/// A scenario that panics still gets its tables back.
async fn a_failed_scenario_restores_its_tables(pool: &Pool) {
    use rustango::testkit::matrix::fresh_table;
    let inner = pool.clone();
    let run = tokio::spawn(async move {
        restoring(
            &inner,
            |pool| async move {
                fresh_table::<LegacyProvider>(&pool).await;
                panic!("scenario failed");
            },
            current_sso_tables,
        )
        .await;
    })
    .await;
    assert!(run.unwrap_err().is_panic(), "the panic reaches the test");
    // The current shape is back: a full provider row inserts.
    provider(pool, "back", false).await;
}

/// Provider and link tables from before this release still sign a linked user in.
async fn pre_migration_tables_still_sign_in(pool: &Pool) {
    restoring(pool, pre_migration_body, current_sso_tables).await;
}

async fn pre_migration_body(pool: Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    let pool = &pool;
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
}

/// Some policy columns but not all: a missing one may hold a rule, so
/// every sign-in through the row refuses until `migrate` runs.
async fn a_partly_migrated_table_refuses(pool: &Pool) {
    restoring(pool, partly_migrated_body, current_sso_tables).await;
}

async fn partly_migrated_body(pool: Pool) {
    use rustango::testkit::matrix::fresh_table;
    let pool = &pool;
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

/// The admin re-link action applies the sign-in's staff rule to every
/// link before it changes any, and then re-links them all.
async fn the_relink_action_checks_every_link_first(pool: &Pool) {
    use rustango::testkit::sso_admin::relink;
    let t = tok();
    provider(pool, "ra", false).await;
    let key = resolve_by_slug(pool, "ra", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant);
    let ann = plain_user(
        pool,
        &format!("ra-{t}"),
        &format!("ra-{t}@client.org"),
        false,
    )
    .await;
    let bob = plain_user(
        pool,
        &format!("rb-{t}"),
        &format!("rb-{t}@client.org"),
        false,
    )
    .await;
    let editor = role(pool, &format!("RA-{t}")).await;
    rustango::tenancy::permissions::assign_role_pool(bob, editor, pool)
        .await
        .unwrap();
    for (sub, uid) in [(format!("ra-{t}"), ann), (format!("rb-{t}"), bob)] {
        create_link(pool, &key, &sub, uid).await.unwrap();
    }
    let link_of = |uid: i64| async move {
        SsoLink::objects()
            .filter("user_id", uid)
            .fetch(pool)
            .await
            .unwrap()
    };
    let invites_for = |uid: i64| async move {
        SsoInvite::objects()
            .filter("link_user_id", uid)
            .fetch(pool)
            .await
            .unwrap()
            .len()
    };
    let pk = |l: &SsoLink| SqlValue::I64(*l.id.get().unwrap());
    let (a, b) = (pk(&link_of(ann).await[0]), pk(&link_of(bob).await[0]));
    // Bob holds a role and the provider does not link staff: nothing changes.
    let err = relink(pool, &[a.clone(), b.clone()]).await.unwrap_err();
    assert!(err.contains("role"), "{err}");
    assert_eq!(link_of(ann).await.len(), 1, "ann's link was kept");
    assert_eq!(invites_for(ann).await, 0);
    relink(pool, &[a]).await.unwrap();
    assert!(link_of(ann).await.is_empty());
    assert_eq!(invites_for(ann).await, 1);
    set(pool, "ra", "allow_email_link_staff", SqlValue::Bool(true)).await;
    relink(pool, &[b]).await.unwrap();
    assert!(link_of(bob).await.is_empty());
    assert_eq!(invites_for(bob).await, 1);
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
    // A mapped role with no row is audited, but not on every sign-in.
    rustango::audit::ensure_table_pool(pool).await.unwrap();
    set(
        pool,
        "map",
        "role_map",
        SqlValue::Json(serde_json::json!({"g-ghost": format!("Ghost-{t}")})),
    )
    .await;
    for _ in 0..8 {
        tenant(pool, "map", &with(serde_json::json!(["g-ghost"])))
            .await
            .unwrap();
    }
    let unknown = rustango::audit::AuditLog::objects()
        .filter("entity_table", "rustango_user_roles")
        .filter("entity_pk", uid.to_string())
        .fetch(pool)
        .await
        .unwrap()
        .len();
    assert!((1..=5).contains(&unknown), "{unknown} unknown-role rows");
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

/// Only a row named exactly `default` holds the tenant settings, on every
/// backend (MySQL's `=` alone would match `Default`).
async fn the_settings_row_name_is_exact(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<SsoTenantPolicy>(pool).await;
    SsoTenantPolicy {
        id: Auto::default(),
        name: "Default".into(),
        access_denied_message: None,
        password_login: "off".into(),
        sso_only: true,
        default_provider: None,
        signups_per_hour: None,
    }
    .insert_pool(pool)
    .await
    .unwrap();
    assert_eq!(
        TenantSettings::load(pool).await.unwrap(),
        TenantSettings::default()
    );
    rustango::testkit::matrix::drop_table(pool, "rustango_sso_tenant_policy").await;
}

/// The member flow with no mode set follows `auto_provision`, as before.
#[allow(deprecated)]
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

/// A tenant settings row, made fresh.
async fn tenant_settings(pool: &Pool, password_login: &str, sso_only: bool, per_hour: Option<i32>) {
    rustango::testkit::matrix::fresh_table::<SsoTenantPolicy>(pool).await;
    SsoTenantPolicy {
        id: Auto::default(),
        name: "default".into(),
        access_denied_message: None,
        password_login: password_login.into(),
        sso_only,
        default_provider: None,
        signups_per_hour: per_hour,
    }
    .insert_pool(pool)
    .await
    .unwrap();
}

async fn may_use_password(pool: &Pool, id: i64) -> bool {
    rustango::sso::lifecycle::password_login_allowed(pool, None, &user_by_id(pool, id).await).await
}

/// `sso_only` on a provider or the tenant keeps its users off passwords;
/// a superuser follows only `password_login`.
async fn password_rules_follow_sso_only(pool: &Pool) {
    restoring(pool, password_rules_body, |pool| async move {
        rustango::testkit::matrix::drop_table(&pool, "rustango_sso_tenant_policy").await;
    })
    .await;
}

async fn password_rules_body(pool: Pool) {
    let pool = &pool;
    let t = tok();
    rustango::testkit::matrix::drop_table(pool, "rustango_sso_tenant_policy").await;
    provider(pool, "strict", false).await;
    let key = resolve_by_slug(pool, "strict", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant);
    let ann = plain_user(pool, &format!("ann-{t}"), &format!("ann-{t}@x.org"), false).await;
    let boss = plain_user(pool, &format!("boss-{t}"), &format!("boss-{t}@x.org"), true).await;
    let bob = plain_user(pool, &format!("bob-{t}"), &format!("bob-{t}@x.org"), false).await;
    create_link(pool, &key, &format!("a-{t}"), ann)
        .await
        .unwrap();
    create_link(pool, &key, &format!("b-{t}"), boss)
        .await
        .unwrap();
    // No settings table, no rule: everyone.
    for id in [ann, boss, bob] {
        assert!(may_use_password(pool, id).await);
    }
    set(pool, "strict", "sso_only", SqlValue::Bool(true)).await;
    assert!(
        !may_use_password(pool, ann).await,
        "linked through an sso_only provider"
    );
    assert!(
        may_use_password(pool, boss).await,
        "a superuser is never locked out"
    );
    assert!(may_use_password(pool, bob).await, "not linked");
    // Disabling the provider keeps its users off passwords; so does deleting it.
    set(pool, "strict", "enabled", SqlValue::Bool(false)).await;
    assert!(!may_use_password(pool, ann).await, "disabled");
    set(pool, "strict", "enabled", SqlValue::Bool(true)).await;
    provider(pool, "gone", false).await;
    let gone_key = resolve_by_slug(pool, "gone", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant);
    let cy = plain_user(pool, &format!("cy-{t}"), &format!("cy-{t}@x.org"), false).await;
    create_link(pool, &gone_key, &format!("c-{t}"), cy)
        .await
        .unwrap();
    assert!(may_use_password(pool, cy).await);
    let q = SsoProvider::objects()
        .filter("slug", "gone")
        .compile_delete()
        .unwrap();
    rustango::sql::delete_pool(pool, &q).await.unwrap();
    assert!(!may_use_password(pool, cy).await, "deleted");
    // Tenant-wide.
    tenant_settings(pool, "on", true, None).await;
    assert!(!may_use_password(pool, bob).await);
    assert!(may_use_password(pool, boss).await);
    tenant_settings(pool, "superusers_only", false, None).await;
    assert!(!may_use_password(pool, bob).await);
    assert!(may_use_password(pool, boss).await);
    tenant_settings(pool, "off", false, None).await;
    assert!(!may_use_password(pool, boss).await);
    // Unreadable settings: only a superuser, the break-glass way in.
    tenant_settings(pool, "on", false, None).await;
    SsoTenantPolicy::objects()
        .filter("name", "default")
        .update()
        .set("signups_per_hour", -5)
        .execute_pool(pool)
        .await
        .unwrap();
    assert!(may_use_password(pool, boss).await, "superuser break-glass");
    assert!(!may_use_password(pool, bob).await, "everyone else refused");
}

/// The SSO audit rows for subject `sub` of provider `slug`, by event.
async fn sso_events(pool: &Pool, slug: &str, sub: &str) -> Vec<serde_json::Value> {
    rustango::audit::fetch_for_entity_pool(pool, "rustango_sso_links", &format!("{slug}:{sub}"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.changes)
        .collect()
}

/// Sign-ups, email links and role changes leave an audit row; the tenant's
/// hourly sign-up limit refuses past it.
async fn successes_are_audited_and_signups_capped(pool: &Pool) {
    rustango::audit::ensure_table_pool(pool).await.unwrap();
    let t = tok();
    let viewer = format!("Viewer-{t}");
    role(pool, &viewer).await;
    provider(pool, "aud", true).await;
    set(pool, "aud", "signup_mode", SqlValue::String("open".into())).await;
    set(
        pool,
        "aud",
        "role_map",
        SqlValue::Json(serde_json::json!({ "g1": viewer })),
    )
    .await;
    set(pool, "aud", "sync_roles_on_login", SqlValue::Bool(true)).await;
    let made = tenant(
        pool,
        "aud",
        &member(
            &format!("n-{t}"),
            &format!("new-{t}@x.org"),
            serde_json::json!({}),
        ),
    )
    .await
    .unwrap();
    let events = sso_events(pool, "aud", &format!("n-{t}")).await;
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "sso_signup" && e["user_id"] == made.user_id),
        "{events:?}"
    );
    // The role map adds a role on the next sign-in: a diff row.
    tenant(
        pool,
        "aud",
        &member(
            &format!("n-{t}"),
            &format!("new-{t}@x.org"),
            serde_json::json!({"groups": ["g1"]}),
        ),
    )
    .await
    .unwrap();
    let events = sso_events(pool, "aud", &format!("n-{t}")).await;
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "sso_roles_synced" && e["added"] == serde_json::json!([viewer])),
        "{events:?}"
    );
    // An email link.
    let old = plain_user(pool, &format!("old-{t}"), &format!("old-{t}@x.org"), false).await;
    tenant(
        pool,
        "aud",
        &member(
            &format!("o-{t}"),
            &format!("old-{t}@x.org"),
            serde_json::json!({}),
        ),
    )
    .await
    .unwrap();
    let events = sso_events(pool, "aud", &format!("o-{t}")).await;
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "sso_linked" && e["user_id"] == old),
        "{events:?}"
    );

    // The hourly cap, counted from this hour's sign-ups.
    let p = resolve_by_slug(pool, "aud", String::new())
        .await
        .unwrap()
        .unwrap();
    let key = p.key(LinkSource::Tenant);
    let mut capped = TenantSettings::default();
    capped.signups_per_hour = Some(1);
    let policy = p.effective_policy().tenant(&capped);
    let late = member(
        &format!("l-{t}"),
        &format!("late-{t}@x.org"),
        serde_json::json!({}),
    );
    assert_eq!(
        admit(
            AdmitRequest::new(pool, Surface::TenantAdmin, &key, &policy, &late).provider("aud"),
            &TenantAccounts(pool),
        )
        .await,
        refused(Refusal::SignupRate)
    );
    let mut unlimited = TenantSettings::default();
    unlimited.signups_per_hour = Some(0);
    let open = p.effective_policy().tenant(&unlimited);
    assert!(
        admit(
            AdmitRequest::new(pool, Surface::TenantAdmin, &key, &open, &late).provider("aud"),
            &TenantAccounts(pool),
        )
        .await
        .unwrap()
        .created
    );
}

/// A shared provider grants only the roles the tenant approved, and none
/// without an override.
async fn shared_roles_need_tenant_approval(pool: &Pool) {
    let t = tok();
    let (viewer, admin) = (format!("Viewer-{t}"), format!("Admin-{t}"));
    role(pool, &viewer).await;
    role(pool, &admin).await;
    provider(pool, "sh", false).await;
    set(pool, "sh", "signup_mode", SqlValue::String("open".into())).await;
    set(
        pool,
        "sh",
        "default_roles",
        SqlValue::String(format!("{viewer}, {admin}")),
    )
    .await;
    let p = resolve_by_slug(pool, "sh", String::new())
        .await
        .unwrap()
        .unwrap();
    let key = p.key(LinkSource::Tenant);
    let mut ov = SharedOverride::default();
    ov.allowed_roles = viewer.parse().unwrap();
    for (n, approved, want) in [(0, None, vec![]), (1, Some(ov), vec![viewer.clone()])] {
        let policy = p.effective_policy().shared_override(Ok(approved));
        let who = member(
            &format!("r{n}-{t}"),
            &format!("r{n}-{t}@x.org"),
            serde_json::json!({}),
        );
        let a = admit(
            AdmitRequest::new(pool, Surface::TenantAdmin, &key, &policy, &who).provider("sh"),
            &TenantAccounts(pool),
        )
        .await
        .unwrap();
        assert_eq!(roles_of(pool, a.user_id).await, want, "case {n}");
    }
    // A stored override row parses into the same rule.
    rustango::testkit::matrix::fresh_table::<SsoSharedOverride>(pool).await;
    let row = |slug: &str| SsoSharedOverride {
        id: Auto::default(),
        shared_slug: slug.into(),
        shared_provider_id: None,
        shared_issuer: None,
        allowed_email_domains: None,
        blocked_email_domains: Some("x.org".into()),
        allowed_idp_tenants: None,
        allowed_hosted_domains: None,
        required_claims: serde_json::json!({}),
        signup_mode: Some("closed".into()),
        allow_email_link: Some(false),
        allow_email_link_staff: None,
        sso_only: Some(true),
        session_ttl_secs: Some(60),
        superuser_session_ttl_secs: None,
        allowed_roles: Some(viewer.clone()),
    };
    row("sh").insert_pool(pool).await.unwrap();
    // The shared row as a sign-in sees it.
    let shared = p.key(LinkSource::Shared);
    let loaded = SharedOverride::load(pool, &shared, "sh")
        .await
        .unwrap()
        .unwrap();
    assert!(loaded.lifecycle.sso_only);
    assert!(loaded.email_link_off && !loaded.email_link_staff_off);
    assert_eq!(loaded.signup_mode, Some(SignupMode::Closed));
    assert_eq!(loaded.allowed_roles.names(), [viewer.clone()]);
    let policy = p.effective_policy().shared_override(Ok(Some(loaded)));
    assert_eq!(policy.session_ttl(3600, false), 60);
    assert_eq!(
        policy
            .check(&member("z", "z@x.org", serde_json::json!({})))
            .unwrap_err()
            .rule(),
        "domain_blocked"
    );
    // First use bound it to the row: a rename keeps it.
    assert!(SharedOverride::load(pool, &shared, "renamed")
        .await
        .unwrap()
        .is_some());
    // A new row under the old slug does not inherit it.
    provider(pool, "sh-new", false).await;
    let newer = resolve_by_slug(pool, "sh-new", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Shared);
    assert_ne!(newer, shared);
    assert!(SharedOverride::load(pool, &newer, "sh")
        .await
        .unwrap()
        .is_none());
    // The same row repointed at another IdP refuses until made again.
    set(
        pool,
        "sh",
        "issuer_url",
        SqlValue::String("https://other-idp.example".into()),
    )
    .await;
    let repointed = resolve_by_slug(pool, "sh", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Shared);
    assert!(SharedOverride::load(pool, &repointed, "sh").await.is_err());
    // Two unbound overrides of one slug are ambiguous.
    row("sh-new").insert_pool(pool).await.unwrap();
    row("sh-new").insert_pool(pool).await.unwrap();
    assert!(SharedOverride::load(pool, &newer, "sh-new").await.is_err());
    rustango::testkit::matrix::drop_table(pool, "rustango_sso_shared_overrides").await;
}

/// Idle SSO-made users are switched off; superusers, hand-linked users and
/// recent ones are left alone.
async fn idle_users_are_deactivated(pool: &Pool) {
    rustango::audit::ensure_table_pool(pool).await.unwrap();
    let t = tok();
    provider(pool, "idle", false).await;
    set(pool, "idle", "auto_deactivate_after_days", SqlValue::I32(2)).await;
    set(pool, "idle", "sso_only", SqlValue::Bool(true)).await;
    // Same idle rule, but its users may still use a password.
    provider(pool, "soft", false).await;
    set(pool, "soft", "auto_deactivate_after_days", SqlValue::I32(2)).await;
    let soft_key = resolve_by_slug(pool, "soft", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant);
    let key = resolve_by_slug(pool, "idle", String::new())
        .await
        .unwrap()
        .unwrap()
        .key(LinkSource::Tenant);
    let old = chrono::Utc::now() - chrono::TimeDelta::days(5);
    let mut ids = Vec::new();
    for (name, superuser, provisioned, last) in [
        ("gone", false, true, Some(old)),
        ("boss", true, true, Some(old)),
        ("hand", false, false, Some(old)),
        ("fresh", false, true, Some(chrono::Utc::now())),
    ] {
        let id = plain_user(
            pool,
            &format!("{name}-{t}"),
            &format!("{name}-{t}@x.org"),
            superuser,
        )
        .await;
        create_link(pool, &key, &format!("{name}-{t}"), id)
            .await
            .unwrap();
        SsoLink::objects()
            .filter("subject", format!("{name}-{t}"))
            .update()
            .set("provisioned", provisioned)
            .set("last_used_at", last)
            .execute_pool(pool)
            .await
            .unwrap();
        ids.push(id);
    }
    let pw = plain_user(pool, &format!("pw-{t}"), &format!("pw-{t}@x.org"), false).await;
    create_link(pool, &soft_key, &format!("pw-{t}"), pw)
        .await
        .unwrap();
    SsoLink::objects()
        .filter("subject", format!("pw-{t}"))
        .update()
        .set("provisioned", true)
        .set("last_used_at", Some(old))
        .execute_pool(pool)
        .await
        .unwrap();
    let done = rustango::sso::lifecycle::deactivate_idle(pool, None)
        .await
        .unwrap();
    assert!(user_by_id(pool, pw).await.active, "a password user stays");
    assert_eq!((done.deactivated, done.failed), (vec![ids[0]], 0));
    let gone = user_by_id(pool, ids[0]).await;
    assert!(!gone.active && gone.sessions_revoked_at.is_some());
    for id in &ids[1..] {
        assert!(user_by_id(pool, *id).await.active);
    }
    // Run again: nothing more.
    assert!(rustango::sso::lifecycle::deactivate_idle(pool, None)
        .await
        .unwrap()
        .deactivated
        .is_empty());
}

/// An email link locks the account's password and ends its sessions, once,
/// and says so in the audit row; a re-link does the same.
async fn an_email_link_locks_the_password(pool: &Pool) {
    rustango::audit::ensure_table_pool(pool).await.unwrap();
    let t = tok();
    provider(pool, "lock", true).await;
    let email = format!("pat-{t}@client.org");
    let id = plain_user(pool, &format!("pat-{t}"), &email, false).await;
    let hash = rustango::tenancy::password::hash("squatter-pass").unwrap();
    User::objects()
        .filter("id", id)
        .update()
        .set("password_hash", hash.clone())
        .execute_pool(pool)
        .await
        .unwrap();
    let mut key = rustango::tenancy::auth_backends::ApiKey {
        id: Auto::default(),
        user_id: id,
        key_prefix: t.chars().take(8).collect(),
        key_hash: "x".into(),
        label: "squatter".into(),
        expires_at: None,
        created_at: Auto::default(),
    };
    key.insert_pool(pool).await.unwrap();
    // A later cut-off (a logout racing the link) is never moved earlier.
    let later = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() + 3600, 0).unwrap();
    User::objects()
        .filter("id", id)
        .update()
        .set("sessions_revoked_at", Some(later))
        .execute_pool(pool)
        .await
        .unwrap();
    let who = member(&format!("p-{t}"), &email, serde_json::json!({}));
    assert_eq!(tenant(pool, "lock", &who).await.unwrap().user_id, id);
    let after = user_by_id(pool, id).await;
    assert_eq!(after.sessions_revoked_at, Some(later));
    assert!(rustango::tenancy::auth_backends::ApiKey::objects()
        .filter("user_id", id)
        .fetch(pool)
        .await
        .unwrap()
        .is_empty());
    assert_ne!(after.password_hash, hash);
    assert!(!rustango::tenancy::password::verify("squatter-pass", &after.password_hash).unwrap());
    assert!(after.sessions_revoked_at.is_some());
    let events = sso_events(pool, "lock", &format!("p-{t}")).await;
    assert!(
        events.iter().any(|e| e["event"] == "sso_linked"
            && e["password_locked"] == true
            && e["revoked"]["api_keys"] == 1),
        "{events:?}"
    );
    // Signing in by the link again changes nothing.
    tenant(pool, "lock", &who).await.unwrap();
    assert_eq!(
        user_by_id(pool, id).await.password_hash,
        after.password_hash
    );

    // A re-link invite locks too.
    provider(pool, "lock2", false).await;
    let email2 = format!("rex-{t}@client.org");
    let rex = plain_user(pool, &format!("rex-{t}"), &email2, false).await;
    relink_invite(pool, "lock2", &email2, rex).await;
    let before = user_by_id(pool, rex).await.password_hash;
    let who2 = member(&format!("r-{t}"), &email2, serde_json::json!({}));
    assert_eq!(tenant(pool, "lock2", &who2).await.unwrap().user_id, rex);
    let rex_after = user_by_id(pool, rex).await;
    assert_ne!(rex_after.password_hash, before);
    assert!(rex_after.sessions_revoked_at.is_some());
}

/// Any role makes an account privileged, even one with no permission.
async fn a_role_without_permissions_is_privileged(pool: &Pool) {
    let t = tok();
    let empty = role(pool, &format!("Empty-{t}")).await;
    provider(pool, "rl", true).await;
    let email = format!("rho-{t}@client.org");
    let id = plain_user(pool, &format!("rho-{t}"), &email, false).await;
    rustango::tenancy::permissions::assign_role_pool(id, empty, pool)
        .await
        .unwrap();
    let who = member(&format!("rho-{t}"), &email, serde_json::json!({}));
    assert_eq!(tenant(pool, "rl", &who).await, refused(Refusal::Privileged));
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        an_email_link_locks_the_password,
        a_role_without_permissions_is_privileged,
        a_partly_migrated_table_refuses,
        a_failed_scenario_restores_its_tables,
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
        the_relink_action_checks_every_link_first,
        role_sync_touches_only_mapped_roles,
        username_clash_suffix_or_refuse,
        member_auto_provision_is_unchanged,
        password_rules_follow_sso_only,
        the_settings_row_name_is_exact,
        successes_are_audited_and_signups_capped,
        shared_roles_need_tenant_approval,
        idle_users_are_deactivated,
    ],
);

/// The paths a `use` item imports, e.g. `crate::sso::link::sign_in`.
fn use_paths(item: &str) -> Vec<String> {
    type Toks = std::iter::Peekable<std::vec::IntoIter<String>>;
    fn tree(prefix: &str, toks: &mut Toks, out: &mut Vec<String>) {
        let mut path = prefix.to_owned();
        while let Some(t) = toks.next() {
            if t == "{" {
                loop {
                    tree(&path, toks, out);
                    match toks.next().as_deref() {
                        Some(",") if toks.peek().is_some_and(|t| t == "}") => {
                            toks.next();
                            return;
                        }
                        Some(",") => {}
                        _ => return,
                    }
                }
            }
            if t == "as" {
                toks.next();
            } else if t != "," && t != "}" {
                path = format!("{path}::{t}");
            }
            if matches!(toks.peek().map(String::as_str), None | Some("," | "}")) {
                break;
            }
        }
        out.push(path);
    }
    let mut toks = Vec::new();
    let mut word = String::new();
    for c in item.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            toks.push(std::mem::take(&mut word));
        }
        if matches!(c, '{' | '}' | ',') {
            toks.push(c.to_string());
        }
    }
    if !word.is_empty() {
        toks.push(word);
    }
    let mut out = Vec::new();
    tree("", &mut toks.into_iter().peekable(), &mut out);
    out
}

/// Whether `text` calls the old sign-in entry point by path or imports it.
fn reaches_legacy_sign_in(text: &str) -> bool {
    if text.contains("link::sign_in(") || text.contains("legacy_sign_in(") {
        return true;
    }
    text.split("use ").skip(1).any(|rest| {
        let item = rest.split(';').next().unwrap_or("");
        use_paths(item)
            .iter()
            .any(|p| p.ends_with("::link::sign_in") || p.ends_with("::legacy_sign_in"))
    })
}

/// Every SSO surface signs in through `sso::policy::admit`; only the
/// legacy wrapper in `sso/link.rs` may reach the old entry point.
#[test]
fn no_callback_bypasses_admit() {
    for caught in [
        "use crate::sso::link::sign_in;",
        "use super::link::{sign_in};",
        "use super::link::{self, sign_in as s};",
        "use crate::sso::{\n    link::{ProviderKey, sign_in},\n    policy,\n};",
        "let r = link::sign_in(pool, &key);",
    ] {
        assert!(reaches_legacy_sign_in(caught), "missed: {caught}");
    }
    for fine in [
        "use crate::sso::link::{sign_in_url, SsoLink};",
        "use crate::sso::{link::ProviderKey, policy::admit};",
        "fn sign_in() {}",
    ] {
        assert!(!reaches_legacy_sign_in(fine), "false alarm: {fine}");
    }
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
        if reaches_legacy_sign_in(&text) {
            offenders.push(rel);
        }
    }
    assert!(
        offenders.is_empty(),
        "SSO sign-in outside admit: {offenders:?}"
    );
}
