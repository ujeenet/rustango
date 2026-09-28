//! SSO link table and provider reads on every backend.

#![cfg(feature = "sso")]

use rustango::casts::{Cast, EncryptedString};
use rustango::oauth2::NormalizedUser;
use rustango::sql::{Auto, Pool};
use rustango::sso::link::{create_link, linked_user, sign_in, EmailMatch, LinkRefusal};
use rustango::sso::{list_enabled, resolve_by_slug, LinkSource, ProviderKey, SsoProvider};
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
    std::env::set_var("RUSTANGO_SECRET_KEY", "sso-link-tri-key");
    rustango::testkit::matrix::drop_table(pool, "rustango_sso_links").await;
    rustango::testkit::matrix::fresh_table::<SsoProvider>(pool).await;
    rustango::sso::link::ensure_table(pool)
        .await
        .expect("links table");
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

async fn key_matches_every_part(pool: &Pool) {
    provider(pool, "corp", false).await;
    let p = resolve_by_slug(pool, "corp", "https://x/cb".into())
        .await
        .unwrap()
        .unwrap();
    let tenant = p.key(LinkSource::Tenant);
    create_link(pool, &tenant, "sub-1", 41).await.unwrap();
    assert_eq!(linked_user(pool, &tenant, "sub-1").await.unwrap(), Some(41));
    assert_eq!(linked_user(pool, &tenant, "sub-2").await.unwrap(), None);
    for other in [
        p.key(LinkSource::Shared),
        p.key(LinkSource::Admin),
        ProviderKey::app("https://idp.example"),
    ] {
        assert_eq!(linked_user(pool, &other, "sub-1").await.unwrap(), None);
    }
    assert!(
        create_link(pool, &tenant, "sub-1", 42).await.is_err(),
        "(provider, subject) is unique"
    );
}

async fn sign_in_rules(pool: &Pool) {
    let key = ProviderKey::app("https://idp.example");
    let ann = |privileged| {
        move |_email: String| async move {
            Ok(Some(EmailMatch {
                user_id: 7,
                privileged,
            }))
        }
    };
    let p = profile("sub-ann", "Ann@Example.com");
    assert!(matches!(
        sign_in(pool, &key, false, &p, ann(false)).await,
        Err(LinkRefusal::EmailLinkDisabled)
    ));
    assert!(matches!(
        sign_in(pool, &key, true, &p, ann(true)).await,
        Err(LinkRefusal::Privileged)
    ));
    assert_eq!(linked_user(pool, &key, "sub-ann").await.unwrap(), None);
    assert_eq!(sign_in(pool, &key, true, &p, ann(false)).await.unwrap(), 7);
    // Linked: the email no longer matters, and no lookup is needed.
    let p = profile("sub-ann", "someone-else@example.com");
    let none = |_email: String| async { Ok(None) };
    assert_eq!(sign_in(pool, &key, false, &p, none).await.unwrap(), 7);
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
    let none = |_email: String| async { Ok(None) };
    assert_eq!(
        sign_in(pool, &key, true, &profile("sub-old", "x@example.com"), none)
            .await
            .unwrap(),
        9
    );
}

tri_dialect_test!(
    setup: setup,
    scenarios: [
        key_matches_every_part,
        sign_in_rules,
        resolve_reads_row_secret_and_flag,
        a_table_without_the_flag_still_serves_logins,
    ],
);
