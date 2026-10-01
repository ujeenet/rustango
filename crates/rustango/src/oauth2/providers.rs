//! Built-in provider presets — preconfigured [`OAuth2Provider`]s for
//! the providers we get asked about most.
//!
//! Each function takes only the per-app credentials (`client_id`,
//! `client_secret`, `redirect_uri`) and returns a fully wired provider
//! with the right endpoints, scopes, and a userinfo mapper that handles
//! the provider's response shape.
//!
//! Don't see your provider? Either:
//! - Use [`OAuth2Provider::from_discovery`] for any OIDC provider, or
//! - Build one with [`OAuth2Provider::new`] + [`OAuth2Provider::with_user_mapper`].
//!
//! [`OAuth2Provider`]: super::OAuth2Provider
//! [`OAuth2Provider::from_discovery`]: super::OAuth2Provider::from_discovery
//! [`OAuth2Provider::new`]: super::OAuth2Provider::new
//! [`OAuth2Provider::with_user_mapper`]: super::OAuth2Provider::with_user_mapper

use std::sync::Arc;

use super::{NormalizedUser, OAuth2Provider, OAuthError, TokenResponse};

// --------------------------------------------------------------------- google
// OIDC. Userinfo follows OIDC spec — default mapper handles it. Picks up
// `prompt=consent` + `access_type=offline` so refresh tokens come back the
// first time the user grants consent.

#[must_use]
pub fn google(
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    OAuth2Provider::new(
        "google",
        client_id,
        client_secret,
        redirect_uri,
        "https://accounts.google.com/o/oauth2/v2/auth",
        "https://oauth2.googleapis.com/token",
    )
    .with_userinfo_url("https://openidconnect.googleapis.com/v1/userinfo")
    .with_scopes(["openid", "email", "profile"])
    .with_extra_auth_params([("access_type", "offline"), ("prompt", "consent")])
}

// --------------------------------------------------------------------- microsoft
// Azure AD / Microsoft Identity Platform v2.0. Multi-tenant by default
// (`/common/`) — pass tenant id instead if you need single-tenant.

#[must_use]
pub fn microsoft(
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    microsoft_for_tenant("common", client_id, client_secret, redirect_uri)
}

#[must_use]
pub fn microsoft_for_tenant(
    azure_tenant: impl AsRef<str>,
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    let t = azure_tenant.as_ref();
    OAuth2Provider::new(
        "microsoft",
        client_id,
        client_secret,
        redirect_uri,
        format!("https://login.microsoftonline.com/{t}/oauth2/v2.0/authorize"),
        format!("https://login.microsoftonline.com/{t}/oauth2/v2.0/token"),
    )
    .with_userinfo_url("https://graph.microsoft.com/oidc/userinfo")
    .with_scopes(["openid", "email", "profile", "offline_access"])
}

// --------------------------------------------------------------------- apple
// Sign in with Apple. NOTE: `client_secret` here is the JWT you generate from
// your Apple key — Apple does not issue a long-lived secret. Apple does NOT
// expose a /userinfo endpoint; identity comes from the id_token. Persisting
// support for that here would mean adding JWT + JWKS verification to the dep
// tree. For now, document the limitation and skip the preset — most users
// reach for Apple last and can build with `OAuth2Provider::new` + a custom
// mapper that decodes the id_token claims (no signature check needed if you
// trust your TLS-protected token endpoint).

// --------------------------------------------------------------------- github
// Pure OAuth2 — no /userinfo, uses /user. `/user` carries no verified flag,
// so the flow also fetches `/user/emails` into `raw["emails"]` (#1842).

#[must_use]
pub fn github(
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    OAuth2Provider::new(
        "github",
        client_id,
        client_secret,
        redirect_uri,
        "https://github.com/login/oauth/authorize",
        "https://github.com/login/oauth/access_token",
    )
    .with_userinfo_url("https://api.github.com/user")
    .with_emails_url("https://api.github.com/user/emails")
    .with_scopes(["read:user", "user:email"])
    .with_user_mapper(Arc::new(github_mapper))
}

fn github_mapper(
    provider: &str,
    raw: serde_json::Value,
    _tokens: &TokenResponse,
) -> Result<NormalizedUser, OAuthError> {
    let id = raw
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or(OAuthError::MissingField("id"))?
        .to_string();
    // Only `/user/emails` says whether an address is verified.
    let emails = raw.get("emails").and_then(serde_json::Value::as_array);
    let verified =
        |e: &serde_json::Value| e.get("verified") == Some(&serde_json::Value::Bool(true));
    let primary = emails
        .and_then(|l| {
            l.iter()
                .find(|e| verified(e) && e.get("primary") == Some(&serde_json::Value::Bool(true)))
        })
        .and_then(|e| e.get("email"))
        .and_then(|v| v.as_str());
    let email = primary
        .or_else(|| raw.get("email").and_then(|v| v.as_str()))
        .map(str::to_owned);
    let email_verified = email.as_deref().is_some_and(|addr| {
        emails.is_some_and(|l| {
            l.iter()
                .any(|e| verified(e) && e.get("email").and_then(|v| v.as_str()) == Some(addr))
        })
    });
    let name = raw
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| raw.get("login").and_then(|v| v.as_str()))
        .map(str::to_owned);
    let avatar_url = raw
        .get("avatar_url")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Ok(NormalizedUser {
        provider: provider.to_owned(),
        provider_user_id: id,
        email_verified,
        email,
        name,
        avatar_url,
        raw,
    })
}

// --------------------------------------------------------------------- discord
// OAuth2. /users/@me is the userinfo equivalent.

#[must_use]
pub fn discord(
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    OAuth2Provider::new(
        "discord",
        client_id,
        client_secret,
        redirect_uri,
        "https://discord.com/api/oauth2/authorize",
        "https://discord.com/api/oauth2/token",
    )
    .with_userinfo_url("https://discord.com/api/users/@me")
    .with_scopes(["identify", "email"])
    .with_user_mapper(Arc::new(discord_mapper))
}

fn discord_mapper(
    provider: &str,
    raw: serde_json::Value,
    _tokens: &TokenResponse,
) -> Result<NormalizedUser, OAuthError> {
    let id = raw
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(OAuthError::MissingField("id"))?
        .to_owned();
    let email = raw.get("email").and_then(|v| v.as_str()).map(str::to_owned);
    let email_verified = raw
        .get("verified")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let name = raw
        .get("global_name")
        .and_then(|v| v.as_str())
        .or_else(|| raw.get("username").and_then(|v| v.as_str()))
        .map(str::to_owned);
    let avatar_url = raw
        .get("avatar")
        .and_then(|v| v.as_str())
        .map(|hash| format!("https://cdn.discordapp.com/avatars/{id}/{hash}.png"));
    Ok(NormalizedUser {
        provider: provider.to_owned(),
        provider_user_id: id,
        email,
        email_verified,
        name,
        avatar_url,
        raw,
    })
}

// --------------------------------------------------------------------- gitlab

#[must_use]
pub fn gitlab(
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    OAuth2Provider::new(
        "gitlab",
        client_id,
        client_secret,
        redirect_uri,
        "https://gitlab.com/oauth/authorize",
        "https://gitlab.com/oauth/token",
    )
    .with_userinfo_url("https://gitlab.com/oauth/userinfo")
    .with_scopes(["openid", "email", "profile"])
}

// --------------------------------------------------------------------- slack
// Sign in with Slack. v2 OIDC.

#[must_use]
pub fn slack(
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    OAuth2Provider::new(
        "slack",
        client_id,
        client_secret,
        redirect_uri,
        "https://slack.com/openid/connect/authorize",
        "https://slack.com/api/openid.connect.token",
    )
    .with_userinfo_url("https://slack.com/api/openid.connect.userInfo")
    .with_scopes(["openid", "email", "profile"])
}

// --------------------------------------------------------------------- facebook
// OAuth2. /me with explicit fields query.

#[must_use]
pub fn facebook(
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    OAuth2Provider::new(
        "facebook",
        client_id,
        client_secret,
        redirect_uri,
        "https://www.facebook.com/v18.0/dialog/oauth",
        "https://graph.facebook.com/v18.0/oauth/access_token",
    )
    .with_userinfo_url("https://graph.facebook.com/me?fields=id,name,email,picture")
    .with_scopes(["email", "public_profile"])
    .with_user_mapper(Arc::new(facebook_mapper))
    // Facebook doesn't support PKCE on web flows.
    .with_pkce(false)
}

fn facebook_mapper(
    provider: &str,
    raw: serde_json::Value,
    _tokens: &TokenResponse,
) -> Result<NormalizedUser, OAuthError> {
    let id = raw
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(OAuthError::MissingField("id"))?
        .to_owned();
    let email = raw.get("email").and_then(|v| v.as_str()).map(str::to_owned);
    let name = raw.get("name").and_then(|v| v.as_str()).map(str::to_owned);
    let avatar_url = raw
        .pointer("/picture/data/url")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Ok(NormalizedUser {
        provider: provider.to_owned(),
        provider_user_id: id,
        // The Graph API has no verified flag, so never vouch for it (#1842).
        email_verified: false,
        email,
        name,
        avatar_url,
        raw,
    })
}

// --------------------------------------------------------------------- keycloak / custom OIDC
// For self-hosted OIDC servers (Keycloak, Auth0, Okta, Authentik, Zitadel...)
// prefer `OAuth2Provider::from_discovery` — it pulls everything from the
// `.well-known/openid-configuration` endpoint at startup.
//
// Convenience: build a Keycloak provider when you know your realm URL but
// haven't wired discovery in.

#[must_use]
pub fn keycloak(
    realm_url: impl AsRef<str>,
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
    redirect_uri: impl Into<String>,
) -> OAuth2Provider {
    let realm = realm_url.as_ref().trim_end_matches('/');
    OAuth2Provider::new(
        "keycloak",
        client_id,
        client_secret,
        redirect_uri,
        format!("{realm}/protocol/openid-connect/auth"),
        format!("{realm}/protocol/openid-connect/token"),
    )
    .with_userinfo_url(format!("{realm}/protocol/openid-connect/userinfo"))
    .with_scopes(["openid", "email", "profile"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_preset_endpoints() {
        let p = google("cid", "csec", "https://app.test/cb");
        assert_eq!(p.name, "google");
        assert!(p.auth_url.starts_with("https://accounts.google.com/"));
        assert!(p.token_url.starts_with("https://oauth2.googleapis.com/"));
        assert_eq!(
            p.userinfo_url.as_deref(),
            Some("https://openidconnect.googleapis.com/v1/userinfo")
        );
        assert!(p.use_pkce);
        assert!(p.extra_auth_params.iter().any(|(k, _)| k == "access_type"));
    }

    #[test]
    fn microsoft_default_uses_common_tenant() {
        let p = microsoft("cid", "csec", "https://app.test/cb");
        assert!(p.auth_url.contains("/common/"));
    }

    #[test]
    fn microsoft_for_tenant_substitutes_id() {
        let p = microsoft_for_tenant(
            "11111111-2222-3333-4444-555555555555",
            "cid",
            "csec",
            "https://app.test/cb",
        );
        assert!(p.auth_url.contains("11111111-2222-3333-4444-555555555555"));
    }

    #[test]
    fn github_mapper_extracts_user() {
        let raw = serde_json::json!({
            "id": 583231_i64,
            "login": "octocat",
            "name": "The Octocat",
            "email": "octocat@github.com",
            "avatar_url": "https://avatars.githubusercontent.com/u/583231?v=4",
        });
        let tokens = TokenResponse {
            access_token: "x".into(),
            refresh_token: None,
            expires_in: None,
            token_type: None,
            id_token: None,
            scope: None,
        };
        let u = github_mapper("github", raw, &tokens).unwrap();
        assert_eq!(u.provider_user_id, "583231");
        assert_eq!(u.email.as_deref(), Some("octocat@github.com"));
        // /user alone says nothing about verification (#1842).
        assert!(!u.email_verified);
        assert_eq!(u.name.as_deref(), Some("The Octocat"));
    }

    fn no_tokens() -> TokenResponse {
        TokenResponse {
            access_token: "x".into(),
            refresh_token: None,
            expires_in: None,
            token_type: None,
            id_token: None,
            scope: None,
        }
    }

    #[test]
    fn github_mapper_reads_verified_from_user_emails() {
        let raw = serde_json::json!({
            "id": 1_i64,
            "email": "public@example.com",
            "emails": [
                {"email": "public@example.com", "primary": false, "verified": false},
                {"email": "primary@example.com", "primary": true, "verified": true},
            ],
        });
        let u = github_mapper("github", raw, &no_tokens()).unwrap();
        assert_eq!(u.email.as_deref(), Some("primary@example.com"));
        assert!(u.email_verified);

        // An unverified primary is not upgraded by anything else.
        let raw = serde_json::json!({
            "id": 1_i64,
            "email": "victim@example.com",
            "emails": [{"email": "victim@example.com", "primary": true, "verified": false}],
        });
        let u = github_mapper("github", raw, &no_tokens()).unwrap();
        assert_eq!(u.email.as_deref(), Some("victim@example.com"));
        assert!(!u.email_verified);
    }

    #[test]
    fn facebook_email_is_not_assumed_verified() {
        let raw = serde_json::json!({"id": "1", "email": "alice@fb.com"});
        let u = facebook_mapper("facebook", raw, &no_tokens()).unwrap();
        assert!(!u.email_verified);
    }

    #[test]
    fn github_preset_fetches_user_emails() {
        let p = github("cid", "csec", "https://app.test/cb");
        assert_eq!(
            p.emails_url.as_deref(),
            Some("https://api.github.com/user/emails")
        );
    }

    #[test]
    fn github_mapper_falls_back_to_login_for_name() {
        let raw = serde_json::json!({"id": 1_i64, "login": "alice"});
        let tokens = TokenResponse {
            access_token: "x".into(),
            refresh_token: None,
            expires_in: None,
            token_type: None,
            id_token: None,
            scope: None,
        };
        let u = github_mapper("github", raw, &tokens).unwrap();
        assert_eq!(u.name.as_deref(), Some("alice"));
        assert_eq!(u.email, None);
        assert!(!u.email_verified);
    }

    #[test]
    fn discord_mapper_builds_avatar_url() {
        let raw = serde_json::json!({
            "id": "80351110224678912",
            "username": "Nelly",
            "global_name": "Nelly!",
            "avatar": "8342729096ea3675442027381ff50dfe",
            "email": "nelly@discord.com",
            "verified": true,
        });
        let tokens = TokenResponse {
            access_token: "x".into(),
            refresh_token: None,
            expires_in: None,
            token_type: None,
            id_token: None,
            scope: None,
        };
        let u = discord_mapper("discord", raw, &tokens).unwrap();
        assert_eq!(u.provider_user_id, "80351110224678912");
        assert_eq!(u.name.as_deref(), Some("Nelly!"));
        assert!(u.email_verified);
        assert_eq!(
            u.avatar_url.as_deref(),
            Some("https://cdn.discordapp.com/avatars/80351110224678912/8342729096ea3675442027381ff50dfe.png")
        );
    }

    #[test]
    fn discord_mapper_falls_back_to_username_when_no_global_name() {
        let raw = serde_json::json!({
            "id": "1",
            "username": "alice",
        });
        let tokens = TokenResponse {
            access_token: "x".into(),
            refresh_token: None,
            expires_in: None,
            token_type: None,
            id_token: None,
            scope: None,
        };
        let u = discord_mapper("discord", raw, &tokens).unwrap();
        assert_eq!(u.name.as_deref(), Some("alice"));
        assert!(u.avatar_url.is_none());
    }

    #[test]
    fn facebook_disables_pkce() {
        let p = facebook("cid", "csec", "https://app.test/cb");
        assert!(!p.use_pkce);
    }

    #[test]
    fn facebook_mapper_pulls_nested_avatar() {
        let raw = serde_json::json!({
            "id": "10001",
            "name": "Alice",
            "email": "alice@fb.com",
            "picture": {"data": {"url": "https://fb.cdn/a.jpg"}},
        });
        let tokens = TokenResponse {
            access_token: "x".into(),
            refresh_token: None,
            expires_in: None,
            token_type: None,
            id_token: None,
            scope: None,
        };
        let u = facebook_mapper("facebook", raw, &tokens).unwrap();
        assert_eq!(u.avatar_url.as_deref(), Some("https://fb.cdn/a.jpg"));
    }

    #[test]
    fn keycloak_builds_realm_endpoints() {
        let p = keycloak(
            "https://kc.example.com/realms/demo/",
            "cid",
            "csec",
            "https://app.test/cb",
        );
        assert!(p
            .auth_url
            .ends_with("/realms/demo/protocol/openid-connect/auth"));
        assert!(p
            .token_url
            .ends_with("/realms/demo/protocol/openid-connect/token"));
        assert!(p
            .userinfo_url
            .as_deref()
            .unwrap()
            .ends_with("/realms/demo/protocol/openid-connect/userinfo"));
    }
}
