//! SSO link table, provider reads and exact matching on every backend.

#![cfg(all(feature = "sso", feature = "tenancy"))]

use rustango::casts::{Cast, EncryptedString};
use rustango::oauth2::NormalizedUser;
use rustango::sql::{Auto, Pool};
use rustango::sso::link::{
    create_link, linked_user, sign_in, Account, AccountLookup, EmailLookup, LinkRefusal, SsoLink,
};
use rustango::sso::{list_enabled, resolve_by_slug, LinkSource, ProviderKey, SsoProvider};
use rustango::tenancy::member_auth::{find_or_provision_member, MemberSignIn};
use rustango::tenancy::User;
use rustango::{tri_dialect_test, Model};

/// `rustango_sso_providers` as deployed before `allow_email_link` existed.
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
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::drop_table;
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-link-tri-key");
    drop_table(pool, "rustango_sso_links").await;
    rustango::testkit::matrix::fresh_table::<SsoProvider>(pool).await;
    rustango::sso::link::ensure_table(pool)
        .await
        .expect("links table");
    // `rustango_users` and the permission tables are shared with other
    // suites (and FK targets): create what is missing, never drop them.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn profile(sub: &str, email: &str) -> NormalizedUser {
    NormalizedUser {
        provider: "oidc".into(),
        provider_user_id: sub.into(),
        email: Some(email.into()),
        email_verified: true,
        name: None,
        avatar_url: None,
        raw: serde_json::json!({}),
    }
}

fn app_key() -> ProviderKey {
    ProviderKey::app("https://idp.example").unwrap()
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

/// `by_id` always fails, like a pool timeout.
struct Broken;

impl AccountLookup for Broken {
    async fn by_id(&self, _id: i64) -> Result<Option<Account>, String> {
        Err("pool timed out".into())
    }
    async fn by_email(&self, _email: &str) -> Result<EmailLookup, String> {
        Err("pool timed out".into())
    }
}

/// A concurrent first login: links the identity while this one looks up the email.
struct Racing<'a> {
    pool: &'a Pool,
    key: ProviderKey,
    subject: &'static str,
}

impl AccountLookup for Racing<'_> {
    async fn by_id(&self, id: i64) -> Result<Option<Account>, String> {
        Ok(Some(Account::new(id, false, true)))
    }
    async fn by_email(&self, _email: &str) -> Result<EmailLookup, String> {
        create_link(self.pool, &self.key, self.subject, 7)
            .await
            .unwrap();
        Ok(EmailLookup::Found(Account::new(7, false, true)))
    }
}

async fn provider(pool: &Pool, slug: &str, allow_email_link: bool) {
    let mut p = SsoProvider {
        id: Auto::default(),
        slug: slug.into(),
        label: format!("Sign in with {slug}"),
        kind: "oidc".into(),
        issuer_url: Some("https://idp.example".into()),
        client_id: "cid".into(),
        client_secret: Cast::new("s3cret".into()),
        enabled: true,
        sort_order: 3,
        scopes: Some("openid email".into()),
        allow_email_link,
        created_at: Auto::default(),
        updated_at: Auto::default(),
    };
    p.insert_pool(pool).await.expect("insert provider");
}

async fn user(pool: &Pool, name: &str, email: &str) -> i64 {
    let mut u = User {
        username: name.into(),
        email: Some(email.into()),
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("insert user");
    u.id.get().copied().unwrap()
}

/// Slugs `check --deploy` reports for this tenant's own providers.
async fn tenant_stranded(pool: &Pool) -> Vec<String> {
    let found = rustango::testkit::sso_check::tenant_providers(pool)
        .await
        .unwrap();
    found.into_iter().map(|s| s.slug).collect()
}

/// Slugs `check --deploy` reports for the shared providers in this tenant.
#[cfg(feature = "admin-sso")]
async fn shared_stranded(registry: &Pool, tenant: &Pool) -> Vec<String> {
    use rustango::testkit::sso_check::{shared_providers, SharedProviders};
    let shared = SharedProviders::load(registry).await.unwrap();
    let found = shared_providers(&shared, tenant).await.unwrap();
    let mut slugs: Vec<String> = found.into_iter().map(|s| s.slug).collect();
    slugs.sort();
    slugs
}

async fn key_matches_every_part(pool: &Pool) {
    provider(pool, "corp", false).await;
    let p = resolve_by_slug(pool, "corp", "https://x/cb".into())
        .await
        .unwrap()
        .unwrap();
    let tenant = p.key(LinkSource::Tenant);
    create_link(pool, &tenant, "sub-1", 41).await.unwrap();
    let got = |k: ProviderKey, s: &'static str| async move {
        linked_user(pool, &k, s).await.unwrap().map(|l| l.user_id)
    };
    assert_eq!(got(tenant.clone(), "sub-1").await, Some(41));
    assert_eq!(got(tenant.clone(), "sub-2").await, None);
    for other in [
        p.key(LinkSource::Shared),
        p.key(LinkSource::Admin),
        app_key(),
    ] {
        assert_eq!(got(other, "sub-1").await, None);
    }
    assert!(
        create_link(pool, &tenant, "sub-1", 42).await.is_err(),
        "(provider, subject) is unique"
    );
}

async fn subject_match_is_exact(pool: &Pool) {
    let key = app_key();
    create_link(pool, &key, "Sub-A", 1).await.unwrap();
    // A case variant is a different subject, and may hold its own link.
    assert!(linked_user(pool, &key, "sub-a").await.unwrap().is_none());
    create_link(pool, &key, "sub-a", 2).await.unwrap();
    for (s, id) in [("Sub-A", 1), ("sub-a", 2)] {
        let got = linked_user(pool, &key, s).await.unwrap().map(|l| l.user_id);
        assert_eq!(got, Some(id), "{s}");
    }
}

async fn email_match_is_exact(pool: &Pool) {
    let key = app_key();
    // Unique per run: the shared user table keeps earlier runs' rows.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let jose = user(pool, &format!("jose{n}"), &format!("Jose{n}@Example.com")).await;
    // ASCII case is ignored, whatever the stored case.
    let p = profile("g-jose", &format!("jose{n}@example.com"));
    assert_eq!(
        find_or_provision_member(pool, &key, true, &p, false).await,
        Ok(MemberSignIn::Member(jose))
    );
    // An accent is not: never a link to `jose`, never a second account the
    // database would call the same.
    let p = profile("g-accent", &format!("josé{n}@example.com"));
    let got = find_or_provision_member(pool, &key, true, &p, true).await;
    if pool.dialect().name() == "mysql" {
        assert_eq!(got, Ok(MemberSignIn::NotLinked), "ai_ci collides");
    } else {
        assert!(
            matches!(got, Ok(MemberSignIn::Member(id)) if id != jose),
            "a distinct email here: {got:?}"
        );
    }
}

async fn a_failing_user_lookup_keeps_the_link(pool: &Pool) {
    let key = app_key();
    create_link(pool, &key, "sub-keep", 3).await.unwrap();
    assert!(matches!(
        sign_in(
            pool,
            &key,
            false,
            &profile("sub-keep", "k@example.com"),
            &Broken
        )
        .await,
        Err(LinkRefusal::Storage(_))
    ));
    assert!(linked_user(pool, &key, "sub-keep").await.unwrap().is_some());
}

async fn issuer_case_is_part_of_the_key(pool: &Pool) {
    let upper = ProviderKey::app("https://IDP.example").unwrap();
    let lower = ProviderKey::app("https://idp.example").unwrap();
    create_link(pool, &upper, "sub", 1).await.unwrap();
    create_link(pool, &lower, "sub", 2).await.unwrap();
    for (k, id) in [(&upper, 1), (&lower, 2)] {
        assert_eq!(
            linked_user(pool, k, "sub")
                .await
                .unwrap()
                .map(|l| l.user_id),
            Some(id)
        );
    }
}

async fn a_hand_made_link_gets_its_key(pool: &Pool) {
    use rustango::sql::FetcherPool as _;
    let key = app_key();
    let mut row = SsoLink {
        id: Auto::default(),
        provider_source: "app".into(),
        provider_id: 0,
        issuer: key.issuer().to_owned(),
        subject: "sub-hand".into(),
        key_sha256: "stale".into(),
        user_id: 8,
        created_at: Auto::default(),
    };
    row.insert_pool(pool).await.unwrap();
    let ok = One(Some(Account::new(8, false, true)));
    let p = profile("sub-hand", "h@example.com");
    assert_eq!(sign_in(pool, &key, false, &p, &ok).await.unwrap(), 8);
    let stored = SsoLink::objects().fetch(pool).await.unwrap();
    assert_eq!(
        stored[0].key_sha256,
        rustango::sso::link::key_sha256(key.issuer(), "sub-hand")
    );
}

async fn a_concurrent_first_login_reuses_the_link(pool: &Pool) {
    let key = app_key();
    let racing = Racing {
        pool,
        key: key.clone(),
        subject: "sub-race",
    };
    let p = profile("sub-race", "race@example.com");
    assert_eq!(sign_in(pool, &key, true, &p, &racing).await.unwrap(), 7);
}

async fn sign_in_rules(pool: &Pool) {
    let key = app_key();
    let ann = |privileged, active| One(Some(Account::new(7, privileged, active)));
    let p = profile("sub-ann", "Ann@Example.com");
    assert!(matches!(
        sign_in(pool, &key, false, &p, &ann(false, true)).await,
        Err(LinkRefusal::EmailLinkDisabled)
    ));
    assert!(matches!(
        sign_in(pool, &key, true, &p, &ann(true, true)).await,
        Err(LinkRefusal::Privileged)
    ));
    assert!(matches!(
        sign_in(pool, &key, true, &p, &ann(false, false)).await,
        Err(LinkRefusal::Inactive)
    ));
    let mut unverified = p.clone();
    unverified.email_verified = false;
    assert!(matches!(
        sign_in(pool, &key, true, &unverified, &ann(false, true)).await,
        Err(LinkRefusal::Unverified)
    ));
    assert!(matches!(
        sign_in(
            pool,
            &key,
            true,
            &profile("", "a@example.com"),
            &ann(false, true)
        )
        .await,
        Err(LinkRefusal::NoSubject)
    ));
    assert!(linked_user(pool, &key, "sub-ann").await.unwrap().is_none());
    assert_eq!(
        sign_in(pool, &key, true, &p, &ann(false, true))
            .await
            .unwrap(),
        7
    );
    // Linked: the email no longer matters.
    let p = profile("sub-ann", "someone-else@example.com");
    assert_eq!(
        sign_in(pool, &key, false, &p, &ann(false, true))
            .await
            .unwrap(),
        7
    );
    // A linked but inactive account is refused.
    assert!(matches!(
        sign_in(pool, &key, false, &p, &ann(false, false)).await,
        Err(LinkRefusal::Inactive)
    ));
}

async fn a_link_to_a_deleted_user_is_dropped(pool: &Pool) {
    let key = app_key();
    create_link(pool, &key, "sub-gone", 99).await.unwrap();
    let p = profile("sub-gone", "new@example.com");
    assert!(matches!(
        sign_in(pool, &key, false, &p, &One(None)).await,
        Err(LinkRefusal::NoAccount(_))
    ));
    assert!(linked_user(pool, &key, "sub-gone").await.unwrap().is_none());
}

async fn provider_changes_drop_links(pool: &Pool) {
    use rustango::sql::FetcherPool as _;
    let key_of = |slug: &'static str| async move {
        resolve_by_slug(pool, slug, "https://x/cb".into())
            .await
            .unwrap()
            .unwrap()
            .key(LinkSource::Tenant)
    };
    provider(pool, "one", false).await;
    provider(pool, "two", false).await; // same issuer, another id
    create_link(pool, &key_of("one").await, "sub-x", 5)
        .await
        .unwrap();
    assert!(linked_user(pool, &key_of("two").await, "sub-x")
        .await
        .unwrap()
        .is_none());
    // Repoint "one" at another issuer: its old link no longer applies.
    let mut row = SsoProvider::objects()
        .filter("slug", "one")
        .fetch(pool)
        .await
        .unwrap()
        .remove(0);
    row.issuer_url = Some("https://other.example".into());
    row.save_pool(pool).await.unwrap();
    assert!(linked_user(pool, &key_of("one").await, "sub-x")
        .await
        .unwrap()
        .is_none());
}

async fn resolve_reads_row_secret_and_flag(pool: &Pool) {
    provider(pool, "corp", true).await;
    provider(pool, "plain", false).await;
    let p = resolve_by_slug(pool, "corp", "https://x/cb".into())
        .await
        .unwrap()
        .unwrap();
    assert!(p.allow_email_link);
    assert_eq!(p.sso.client_secret, "s3cret");
    assert_eq!(p.sso.issuer_url.as_deref(), Some("https://idp.example"));
    assert_eq!(
        p.sso.scopes,
        Some(vec!["openid".to_owned(), "email".to_owned()])
    );
    let plain = resolve_by_slug(pool, "plain", "https://x/cb".into())
        .await
        .unwrap()
        .unwrap();
    assert!(!plain.allow_email_link);
    assert_eq!(list_enabled(pool, "/login").await.len(), 2);
}

async fn a_table_without_the_flag_still_serves_logins(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<LegacyProvider>(pool).await;
    let mut p = LegacyProvider {
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
        created_at: Auto::default(),
        updated_at: Auto::default(),
    };
    p.insert_pool(pool).await.unwrap();
    assert_eq!(list_enabled(pool, "/login").await.len(), 1);
    let p = resolve_by_slug(pool, "old", "https://x/cb".into())
        .await
        .unwrap()
        .unwrap();
    assert!(!p.allow_email_link, "an unreadable flag is off");
    assert_eq!(p.sso.client_secret, "s3cret");
    // `check --deploy` reads the missing flag as off too, and names the provider.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    user(pool, &format!("old{n}"), &format!("old{n}@example.com")).await;
    assert_eq!(tenant_stranded(pool).await, ["old"]);
    let key = p.key(LinkSource::Tenant);
    create_link(pool, &key, "sub-old", 9).await.unwrap();
    let ok = One(Some(Account::new(9, false, true)));
    assert_eq!(
        sign_in(pool, &key, true, &profile("sub-old", "x@example.com"), &ok)
            .await
            .unwrap(),
        9
    );
}

/// A database without the provider table holds no providers, at sign-in and in
/// `check --deploy` alike, so a tenant falls through to the shared set (#2366).
async fn a_missing_provider_table_holds_no_providers(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, "rustango_sso_providers").await;
    let found = resolve_by_slug(pool, "corp", "https://x/cb".into()).await;
    assert!(matches!(found, Ok(None)), "{found:?}");
    assert!(list_enabled(pool, "/login").await.is_empty());
    assert!(tenant_stranded(pool).await.is_empty());
    rustango::testkit::matrix::fresh_table::<SsoProvider>(pool).await;
}

/// The `check --deploy` scan: linking off, no link rows, existing users (#2359).
async fn check_finds_providers_that_refuse_every_user(pool: &Pool) {
    use rustango::sql::FetcherPool as _;
    // Unique per run: the shared user tables keep earlier runs' rows.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    user(pool, &format!("chk{n}"), &format!("chk{n}@example.com")).await;
    provider(pool, "corp", false).await;
    provider(pool, "open", true).await;
    provider(pool, "off", false).await;
    let mut off = SsoProvider::objects()
        .filter("slug", "off")
        .fetch(pool)
        .await
        .unwrap()
        .remove(0);
    off.enabled = false;
    off.save_pool(pool).await.unwrap();
    let corp = resolve_by_slug(pool, "corp", String::new())
        .await
        .unwrap()
        .unwrap();
    // A link of another source is not this provider's link.
    create_link(pool, &corp.key(LinkSource::Shared), "sub-1", 1)
        .await
        .unwrap();
    assert_eq!(tenant_stranded(pool).await, ["corp"]);
    create_link(pool, &corp.key(LinkSource::Tenant), "sub-1", 1)
        .await
        .unwrap();
    assert!(tenant_stranded(pool).await.is_empty());

    #[cfg(feature = "admin-sso")]
    {
        use rustango::testkit::sso_check as check;
        // The admin never links by email, so `open` counts too.
        let mut root =
            rustango::admin::AdminUser::new_with_password(&format!("chk{n}"), "pw-123456789", true)
                .unwrap();
        root.insert_pool(pool).await.unwrap();
        let mut got = check::admin_providers(pool).await.unwrap();
        got.sort();
        assert_eq!(got, ["corp", "open"]);
        let open = resolve_by_slug(pool, "open", String::new())
            .await
            .unwrap()
            .unwrap();
        create_link(pool, &open.key(LinkSource::Admin), "sub-1", 1)
            .await
            .unwrap();
        assert_eq!(check::admin_providers(pool).await.unwrap(), ["corp"]);

        use check::SharedSsoProvider;
        rustango::testkit::matrix::fresh_table::<SharedSsoProvider>(pool).await;
        for (slug, allow) in [
            ("team", false),
            ("corp", false),
            ("free", true),
            ("off", false),
        ] {
            SharedSsoProvider {
                id: Auto::default(),
                slug: slug.into(),
                label: slug.into(),
                kind: "oidc".into(),
                // Not the tenant rows' issuer, so their shared-source link above is no match.
                issuer_url: Some("https://shared.example".into()),
                client_id: "cid".into(),
                client_secret: Cast::new("s3cret".into()),
                enabled: true,
                sort_order: 0,
                scopes: None,
                allow_email_link: allow,
                created_at: Auto::default(),
                updated_at: Auto::default(),
            }
            .insert_pool(pool)
            .await
            .unwrap();
        }
        // `corp` is the tenant's own enabled row, so the shared one is never
        // used here; the tenant's `off` row is disabled, so the shared one is.
        assert_eq!(shared_stranded(pool, pool).await, ["off", "team"]);
        let team = SharedSsoProvider::objects()
            .filter("slug", "team")
            .fetch(pool)
            .await
            .unwrap()
            .remove(0);
        let link = |issuer: &str| SsoLink {
            id: Auto::default(),
            provider_source: "shared".into(),
            provider_id: team.id.get().copied().unwrap(),
            issuer: issuer.into(),
            subject: "sub-1".into(),
            key_sha256: rustango::sso::link::key_sha256(issuer, "sub-1"),
            user_id: 1,
            created_at: Auto::default(),
        };
        // Sign-in matches the issuer exactly, so a case variant is no link (MySQL `_ci`).
        link("oidc|https://SHARED.example")
            .insert_pool(pool)
            .await
            .unwrap();
        assert_eq!(shared_stranded(pool, pool).await, ["off", "team"]);
        link("oidc|https://shared.example")
            .insert_pool(pool)
            .await
            .unwrap();
        assert_eq!(shared_stranded(pool, pool).await, ["off"]);
        // Other suites on this database recreate it through the migrations.
        rustango::testkit::matrix::drop_table(pool, "rustango_shared_sso_providers").await;
    }
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        key_matches_every_part,
        subject_match_is_exact,
        email_match_is_exact,
        a_failing_user_lookup_keeps_the_link,
        issuer_case_is_part_of_the_key,
        a_hand_made_link_gets_its_key,
        a_concurrent_first_login_reuses_the_link,
        sign_in_rules,
        a_link_to_a_deleted_user_is_dropped,
        provider_changes_drop_links,
        resolve_reads_row_secret_and_flag,
        a_table_without_the_flag_still_serves_logins,
        a_missing_provider_table_holds_no_providers,
        check_finds_providers_that_refuse_every_user,
    ],
);
