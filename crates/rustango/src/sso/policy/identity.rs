//! What the IdP said about a user: the userinfo profile plus the claims of
//! a verified id_token. Claims only an id_token may carry are dropped from
//! userinfo, so an unverified token leaves them absent.

use serde_json::{Map, Value};

use super::domain::{EmailAddr, PolicyError};
use super::jwks::{self, IdTokenError};
use super::Refusal;
use crate::oauth2::{IssuerRule, NormalizedUser, OAuth2Flow, OAuth2Provider, TokenResponse};

/// Claims trusted only from a verified id_token.
const ID_TOKEN_ONLY: &[&str] = &[
    "iss", "aud", "azp", "tid", "xms_edov", "nonce", "exp", "nbf", "iat",
];

/// The id_token's fate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdToken {
    /// Verified; its claims are merged in.
    Verified,
    /// The token endpoint sent none.
    Absent,
    /// Sent but not trusted.
    Invalid(IdTokenError),
}

/// A signed-in identity as the policy sees it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Identity {
    pub profile: NormalizedUser,
    claims: Map<String, Value>,
    email: Option<EmailAddr>,
    /// The IdP vouches for [`Self::email`] whatever the policy pins.
    email_verified: bool,
    /// A Microsoft directory: a pinned `tid` also vouches for the email.
    microsoft: bool,
    /// The provider signs id_tokens (JWKS): role claims come only from a verified one.
    jwks: bool,
    /// The claims a verified id_token carried.
    token_claims: Vec<String>,
    pub id_token: IdToken,
}

impl Identity {
    /// From a completed handshake: the profile, plus the id_token's claims
    /// once its signature, audience, lifetime, issuer and the `nonce` of
    /// `flow` check out.
    pub async fn from_handshake(
        provider: &OAuth2Provider,
        profile: NormalizedUser,
        tokens: &TokenResponse,
        flow: &OAuth2Flow,
    ) -> Self {
        let verified = match tokens.id_token.as_deref() {
            None => Err(None),
            Some(t) => jwks::verify(provider, t).await.map_err(Some),
        };
        let verified = verified.and_then(|t| check_nonce(t, flow.nonce.as_deref()));
        let microsoft = matches!(
            provider.id_token_keys(),
            Some((_, IssuerRule::MicrosoftTenant))
        );
        let mut identity = Self::assemble(profile, verified, microsoft);
        identity.jwks = provider.id_token_keys().is_some();
        if let IdToken::Invalid(e) = &identity.id_token {
            tracing::warn!(target: "rustango::sso", provider = %provider.name, "id_token not trusted: {e}");
        }
        identity
    }

    /// An identity from the profile alone, as before id_tokens were read.
    #[must_use]
    pub fn from_profile(profile: NormalizedUser) -> Self {
        Self::assemble(profile, Err(None), false)
    }

    fn assemble(
        profile: NormalizedUser,
        verified: Result<Map<String, Value>, Option<IdTokenError>>,
        microsoft: bool,
    ) -> Self {
        let mut claims = profile.raw.as_object().cloned().unwrap_or_default();
        for k in ID_TOKEN_ONLY {
            claims.remove(*k);
        }
        // A token for another subject vouches for nothing here.
        let verified = verified.and_then(|t| {
            if t.get("sub").and_then(Value::as_str) == Some(profile.provider_user_id.as_str()) {
                Ok(t)
            } else {
                Err(Some(IdTokenError::Malformed("sub differs from userinfo")))
            }
        });
        let (id_token, token) = match verified {
            Ok(t) => (IdToken::Verified, Some(t)),
            Err(None) => (IdToken::Absent, None),
            Err(Some(e)) => (IdToken::Invalid(e), None),
        };
        let token_email = token
            .as_ref()
            .and_then(|t| t.get("email"))
            .and_then(Value::as_str)
            .and_then(|e| e.parse::<EmailAddr>().ok());
        let profile_email = profile
            .email
            .as_deref()
            .and_then(|e| e.parse::<EmailAddr>().ok());
        let token_says = |k: &str| {
            token
                .as_ref()
                .and_then(|t| t.get(k))
                .and_then(Value::as_bool)
                == Some(true)
        };
        // The token's email when it has one: that is what its flags describe.
        let (email, email_verified) = match token_email {
            Some(e) => {
                let by_profile = profile.email_verified && profile_email.as_ref() == Some(&e);
                let by_token =
                    token_says("email_verified") || (microsoft && token_says("xms_edov"));
                (Some(e), by_profile || by_token)
            }
            None => (profile_email, profile.email_verified),
        };
        let token_claims = token
            .as_ref()
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(t) = token {
            claims.extend(t);
        }
        Self {
            profile,
            claims,
            email,
            email_verified,
            microsoft,
            jwks: false,
            token_claims,
            id_token,
        }
    }

    /// A signed id_token answered another sign-in: refuse, it was injected.
    #[must_use]
    pub fn replayed(&self) -> bool {
        self.id_token == IdToken::Invalid(IdTokenError::Nonce)
    }

    /// The IdP's subject.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.profile.provider_user_id
    }

    /// The email, verified or not.
    #[must_use]
    pub fn email(&self) -> Option<&EmailAddr> {
        self.email.as_ref()
    }

    /// The email if the IdP vouches for it. A Microsoft email also counts
    /// when `tid_pinned` (the policy pins tenants and this `tid` passed).
    #[must_use]
    pub fn verified_email(&self, tid_pinned: bool) -> Option<&EmailAddr> {
        let pinned = self.microsoft && tid_pinned && self.claim_str("tid").is_some();
        self.email
            .as_ref()
            .filter(|_| self.email_verified || pinned)
    }

    /// One merged claim.
    #[must_use]
    pub fn claim(&self, name: &str) -> Option<&Value> {
        self.claims.get(name)
    }

    /// A claim roles are mapped from. When the provider signs id_tokens it
    /// counts only from a verified one; otherwise userinfo counts too.
    #[must_use]
    pub fn role_claim(&self, name: &str) -> Option<&Value> {
        if self.jwks && !self.token_claims.iter().any(|k| k == name) {
            return None;
        }
        self.claim(name)
    }

    /// A claim that is a non-empty string.
    #[must_use]
    pub fn claim_str(&self, name: &str) -> Option<&str> {
        self.claim(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }
}

/// A verified token must carry the `nonce` the sign-in sent, if it sent one.
fn check_nonce(
    token: Map<String, Value>,
    sent: Option<&str>,
) -> Result<Map<String, Value>, Option<IdTokenError>> {
    match sent {
        Some(n) if token.get("nonce").and_then(Value::as_str) != Some(n) => {
            Err(Some(IdTokenError::Nonce))
        }
        _ => Ok(token),
    }
}

/// `{"claim": ["value", ...]}`: every claim must hold one of its values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequiredClaims(Vec<(String, Vec<String>)>);

impl RequiredClaims {
    /// Parse the stored JSON. `null` and `{}` require nothing.
    ///
    /// # Errors
    /// Not an object, or a value that is not a string or a non-empty list of strings.
    pub fn parse(v: &Value) -> Result<Self, PolicyError> {
        let obj = match v {
            Value::Null => return Ok(Self::default()),
            Value::Object(o) => o,
            _ => return Err(PolicyError("required_claims must be a JSON object".into())),
        };
        let mut out = Vec::new();
        for (name, want) in obj {
            let values: Vec<String> = match want {
                Value::String(s) => vec![s.clone()],
                Value::Array(list) if !list.is_empty() => list
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .ok_or_else(|| PolicyError(format!("required_claims.{name}: strings only")))?,
                _ => {
                    return Err(PolicyError(format!(
                        "required_claims.{name}: a string or a non-empty list"
                    )))
                }
            };
            if name.is_empty() {
                return Err(PolicyError("required_claims: an empty claim name".into()));
            }
            out.push((name.clone(), values));
        }
        Ok(Self(out))
    }

    /// The first claim `identity` lacks a wanted value for.
    pub(crate) fn check(&self, identity: &Identity) -> Result<(), Refusal> {
        for (name, want) in &self.0 {
            let has = |v: &Value| match v {
                Value::String(s) => want.contains(s),
                Value::Bool(_) | Value::Number(_) => want.contains(&v.to_string()),
                _ => false,
            };
            let ok = match identity.claim(name) {
                Some(Value::Array(list)) => list.iter().any(has),
                Some(v) => has(v),
                None => false,
            };
            if !ok {
                return Err(Refusal::Claim { name: name.clone() });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn test_identity(
    profile: NormalizedUser,
    token: Option<Map<String, Value>>,
    microsoft: bool,
) -> Identity {
    Identity::assemble(profile, token.ok_or(None), microsoft)
}

#[cfg(test)]
impl Identity {
    /// As if the provider published a JWKS.
    pub(crate) fn with_jwks(mut self) -> Self {
        self.jwks = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(raw: Value) -> NormalizedUser {
        NormalizedUser {
            provider: "oidc".into(),
            provider_user_id: "u1".into(),
            email: Some("Ann@Client.org".into()),
            email_verified: false,
            name: None,
            avatar_url: None,
            raw,
        }
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn id_token_only_claims_never_come_from_userinfo() {
        let id = test_identity(
            profile(serde_json::json!({"tid": "forged", "iss": "x", "groups": ["g"]})),
            None,
            true,
        );
        assert_eq!(id.claim("tid"), None);
        assert_eq!(id.claim("iss"), None);
        assert!(id.claim("groups").is_some());
        let id = test_identity(
            profile(serde_json::json!({"tid": "forged"})),
            Some(obj(serde_json::json!({"sub": "u1", "tid": "real"}))),
            true,
        );
        assert_eq!(id.claim_str("tid"), Some("real"));
    }

    /// A verified token with another sign-in's nonce, or none, is refused.
    #[test]
    fn the_nonce_must_be_this_sign_ins() {
        let token = |nonce: Option<&str>| {
            let mut t = obj(serde_json::json!({"sub": "u1", "tid": "t"}));
            if let Some(n) = nonce {
                t.insert("nonce".into(), n.into());
            }
            t
        };
        let run = |nonce, sent| {
            let checked = check_nonce(token(nonce), sent);
            Identity::assemble(profile(serde_json::json!({})), checked, true)
        };
        let ok = run(Some("n1"), Some("n1"));
        assert_eq!(ok.id_token, IdToken::Verified);
        assert!(!ok.replayed());
        for (nonce, sent) in [(Some("n2"), Some("n1")), (None, Some("n1"))] {
            let bad = run(nonce, sent);
            assert!(bad.replayed(), "{nonce:?} vs {sent:?}");
            assert_eq!(bad.claim("tid"), None);
            assert_eq!(
                super::super::EffectivePolicy::legacy(true).check(&bad),
                Err(Refusal::IdTokenReplayed)
            );
        }
        // A flow sealed before nonces were sent checks nothing.
        assert_eq!(run(None, None).id_token, IdToken::Verified);
    }

    #[test]
    fn a_token_for_another_subject_is_dropped() {
        let id = test_identity(
            profile(serde_json::json!({})),
            Some(obj(serde_json::json!({"sub": "u2", "tid": "t"}))),
            true,
        );
        assert!(matches!(id.id_token, IdToken::Invalid(_)));
        assert_eq!(id.claim("tid"), None);
    }

    #[test]
    fn microsoft_email_is_verified_by_edov_or_a_pinned_tid() {
        let token = |edov: bool| {
            Some(obj(serde_json::json!({
                "sub": "u1", "tid": "t1", "email": "ann@client.org", "xms_edov": edov,
            })))
        };
        let id = test_identity(profile(serde_json::json!({})), token(true), true);
        assert!(id.verified_email(false).is_some());
        let id = test_identity(profile(serde_json::json!({})), token(false), true);
        assert!(id.verified_email(false).is_none());
        assert!(id.verified_email(true).is_some());
        // Not Microsoft: a pin vouches for nothing, and `xms_edov` means nothing.
        let id = test_identity(profile(serde_json::json!({})), token(true), false);
        assert!(id.verified_email(true).is_none());
        // No token: a pinned tid cannot be read.
        let id = test_identity(profile(serde_json::json!({})), None, true);
        assert!(id.verified_email(true).is_none());
    }

    #[test]
    fn role_claims_come_only_from_a_verified_token_when_the_provider_has_jwks() {
        let raw = serde_json::json!({"groups": ["forged"]});
        let plain = test_identity(profile(raw.clone()), None, false);
        assert!(
            plain.role_claim("groups").is_some(),
            "no JWKS: userinfo counts"
        );
        let signed = test_identity(profile(raw.clone()), None, false).with_jwks();
        assert_eq!(signed.role_claim("groups"), None);
        let signed = test_identity(
            profile(raw),
            Some(obj(serde_json::json!({"sub": "u1", "groups": ["real"]}))),
            false,
        )
        .with_jwks();
        assert_eq!(
            signed.role_claim("groups"),
            Some(&serde_json::json!(["real"]))
        );
    }

    #[test]
    fn required_claims_take_a_string_or_a_list() {
        let rc =
            RequiredClaims::parse(&serde_json::json!({"roles": ["App.User"], "hd": "client.org"}))
                .unwrap();
        let with = |raw: Value| test_identity(profile(raw), None, false);
        assert!(rc
            .check(&with(
                serde_json::json!({"roles": ["x", "App.User"], "hd": "client.org"})
            ))
            .is_ok());
        assert!(rc
            .check(&with(
                serde_json::json!({"roles": "App.User", "hd": "client.org"})
            ))
            .is_ok());
        assert_eq!(
            rc.check(&with(serde_json::json!({"hd": "client.org"}))),
            Err(Refusal::Claim {
                name: "roles".into()
            }),
            "a missing claim refuses"
        );
        assert!(rc
            .check(&with(
                serde_json::json!({"roles": ["App.Admin"], "hd": "client.org"})
            ))
            .is_err());
        // The issuer pin reads only a verified token's `iss`.
        let iss = RequiredClaims::parse(&serde_json::json!({"iss": ["https://idp"]})).unwrap();
        assert!(iss
            .check(&with(serde_json::json!({"iss": "https://idp"})))
            .is_err());
        for bad in [
            serde_json::json!([]),
            serde_json::json!({"a": []}),
            serde_json::json!({"a": [1]}),
            serde_json::json!({"a": {"b": 1}}),
        ] {
            assert!(RequiredClaims::parse(&bad).is_err(), "{bad}");
        }
        assert_eq!(
            RequiredClaims::parse(&serde_json::json!({})).unwrap(),
            RequiredClaims::default()
        );
    }
}
