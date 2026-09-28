//! SSO link table, provider reads and exact matching on every backend.

#![cfg(all(feature = "sso", feature = "tenancy"))]

use rustango::casts::{Cast, EncryptedString};
use rustango::oauth2::NormalizedUser;
use rustango::sql::{Auto, Pool};
use rustango::sso::link::{create_link, linked_user, sign_in, Account, AccountLookup, LinkRefusal};
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
    // Children first: the permission tables reference `rustango_users`.
    for t in [
        "rustango_user_permissions",
        "rustango_user_roles",
        "rustango_role_permissions",
        "rustango_roles",
        "rustango_permissions",
    ] {
        drop_table(pool, t).await;
    }
    rustango::testkit::matrix::fresh_table::<User>(pool).await;
    rustango::tenancy::permissions::ensure_tables_pool(pool)
        .await
        .expect("permission tables");
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
    async fn by_email(&self, _email: &str) -> Result<Option<Account>, String> {
        Ok(self.0)
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
    let jose = user(pool, "jose", "jose@example.com").await;
    let p = profile("g-accent", "josé@example.com");
    assert_eq!(
        find_or_provision_member(pool, &key, true, &p, false).await,
        Ok(MemberSignIn::NoAccount),
        "an accent-insensitive collation must not match another account"
    );
    // The IdP's email is lowercased before the exact match.
    let p = profile("g-jose", "Jose@Example.com");
    assert_eq!(
        find_or_provision_member(pool, &key, true, &p, false).await,
        Ok(MemberSignIn::Member(jose))
    );
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

tri_dialect_test!(
    setup: setup,
    scenarios: [
        key_matches_every_part,
        subject_match_is_exact,
        email_match_is_exact,
        sign_in_rules,
        a_link_to_a_deleted_user_is_dropped,
        provider_changes_drop_links,
        resolve_reads_row_secret_and_flag,
        a_table_without_the_flag_still_serves_logins,
    ],
);
