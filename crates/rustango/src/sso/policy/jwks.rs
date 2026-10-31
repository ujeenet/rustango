//! id_token verification: the signature against the issuer's JWKS (RS256 or
//! ES256), then `aud`, `azp`, `exp`, `nbf` and `iss`.
//!
//! Key sets are cached per URL and egress policy for the response's
//! `max-age` (clamped), and refetched early at most once a minute when a
//! token names a key the set lacks (rotation).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::{Map, Value};

use crate::oauth2::{IssuerRule, OAuth2Provider};
use crate::outbound::TargetPolicy;

/// Clock skew allowed on `exp` and `nbf`.
const LEEWAY_SECS: i64 = 60;
const DEFAULT_TTL: Duration = Duration::from_secs(3600);
const MIN_TTL: Duration = Duration::from_secs(300);
const MAX_TTL: Duration = Duration::from_secs(86_400);
/// How long a failed fetch is reused.
const FAIL_TTL: Duration = Duration::from_secs(30);
/// Least time between two fetches of one set for an unknown `kid`.
const REFETCH_GAP: Duration = Duration::from_secs(60);
/// Most key sets cached; past it, the oldest is dropped.
const MAX_SETS: usize = 256;

/// Why an id_token was not trusted. Its claims are then absent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdTokenError {
    /// The provider has no JWKS to check against.
    NoKeys,
    /// Not a compact JWS, or a part does not decode.
    Malformed(&'static str),
    /// An algorithm other than RS256 / ES256.
    Algorithm(String),
    /// No key in the set has this `kid` (or fits the algorithm).
    UnknownKey,
    BadSignature,
    /// `aud` (or `azp`) is not this client.
    Audience,
    Expired,
    NotYetValid,
    Issuer,
    /// The key set could not be fetched or read.
    Keys(String),
    /// Signed and valid, but its `nonce` is not the one this sign-in sent.
    Nonce,
}

impl std::fmt::Display for IdTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKeys => f.write_str("the provider has no JWKS"),
            Self::Malformed(what) => write!(f, "malformed id_token: {what}"),
            Self::Algorithm(a) => write!(f, "unsupported id_token alg {a:?}"),
            Self::UnknownKey => f.write_str("no JWKS key matches the id_token"),
            Self::BadSignature => f.write_str("id_token signature does not verify"),
            Self::Audience => f.write_str("id_token is for another client"),
            Self::Expired => f.write_str("id_token expired"),
            Self::NotYetValid => f.write_str("id_token not yet valid"),
            Self::Issuer => f.write_str("id_token issuer not accepted"),
            Self::Nonce => f.write_str("id_token nonce is not this sign-in's"),
            Self::Keys(e) => write!(f, "JWKS: {e}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alg {
    Rs256,
    Es256,
}

impl Alg {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "RS256" => Some(Self::Rs256),
            "ES256" => Some(Self::Es256),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Material {
    /// Modulus and exponent, big-endian without leading zeros.
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// Uncompressed SEC1 point.
    P256(Vec<u8>),
}

#[derive(Debug)]
struct Key {
    kid: Option<String>,
    alg: Alg,
    material: Material,
}

impl Key {
    fn verifies(&self, input: &[u8], sig: &[u8]) -> bool {
        use ring::signature as s;
        match &self.material {
            Material::Rsa { n, e } => s::RsaPublicKeyComponents { n, e }
                .verify(&s::RSA_PKCS1_2048_8192_SHA256, input, sig)
                .is_ok(),
            Material::P256(point) => s::UnparsedPublicKey::new(&s::ECDSA_P256_SHA256_FIXED, point)
                .verify(input, sig)
                .is_ok(),
        }
    }
}

/// One fetched key set.
#[derive(Debug)]
pub(crate) struct KeySet {
    keys: Vec<Key>,
    fetched: Instant,
    ttl: Duration,
}

fn b64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

fn strip_zeros(mut v: Vec<u8>) -> Vec<u8> {
    let lead = v.iter().take_while(|b| **b == 0).count();
    v.drain(..lead);
    v
}

impl KeySet {
    /// The usable signing keys of a JWKS document; others are skipped.
    fn parse(body: &[u8], ttl: Duration) -> Result<Self, IdTokenError> {
        let doc: Value =
            serde_json::from_slice(body).map_err(|e| IdTokenError::Keys(e.to_string()))?;
        let list = doc
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| IdTokenError::Keys("no `keys` array".into()))?;
        let text = |k: &Value, f: &str| k.get(f).and_then(Value::as_str).map(str::to_owned);
        let keys = list
            .iter()
            .filter(|k| text(k, "use").is_none_or(|u| u == "sig"))
            .filter_map(|k| {
                let declared = text(k, "alg");
                let (alg, material) = match (text(k, "kty")?.as_str(), text(k, "crv")) {
                    ("RSA", _) => (
                        Alg::Rs256,
                        Material::Rsa {
                            n: strip_zeros(b64(&text(k, "n")?)?),
                            e: strip_zeros(b64(&text(k, "e")?)?),
                        },
                    ),
                    ("EC", Some(c)) if c == "P-256" => {
                        let (x, y) = (b64(&text(k, "x")?)?, b64(&text(k, "y")?)?);
                        if x.len() != 32 || y.len() != 32 {
                            return None;
                        }
                        let mut point = vec![4u8];
                        point.extend(x);
                        point.extend(y);
                        (Alg::Es256, Material::P256(point))
                    }
                    _ => return None,
                };
                if declared.is_some_and(|d| Alg::parse(&d) != Some(alg)) {
                    return None;
                }
                Some(Key {
                    kid: text(k, "kid"),
                    alg,
                    material,
                })
            })
            .collect();
        Ok(Self {
            keys,
            fetched: Instant::now(),
            ttl,
        })
    }

    fn knows(&self, kid: Option<&str>) -> bool {
        kid.is_none_or(|kid| self.keys.iter().any(|k| k.kid.as_deref() == Some(kid)))
    }
}

/// A compact JWS split into its parts.
struct Jws<'a> {
    alg: Alg,
    kid: Option<String>,
    input: &'a [u8],
    sig: Vec<u8>,
    claims: Map<String, Value>,
}

fn split(token: &str) -> Result<Jws<'_>, IdTokenError> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(IdTokenError::Malformed("not three parts"));
    };
    let header: Map<String, Value> = b64(h)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(IdTokenError::Malformed("header"))?;
    if header.contains_key("crit") {
        return Err(IdTokenError::Malformed("crit header"));
    }
    let alg_name = header.get("alg").and_then(Value::as_str).unwrap_or("");
    let alg = Alg::parse(alg_name).ok_or_else(|| IdTokenError::Algorithm(alg_name.to_owned()))?;
    let claims: Map<String, Value> = b64(p)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(IdTokenError::Malformed("payload"))?;
    Ok(Jws {
        alg,
        kid: header.get("kid").and_then(Value::as_str).map(str::to_owned),
        input: token[..h.len() + 1 + p.len()].as_bytes(),
        sig: b64(s).ok_or(IdTokenError::Malformed("signature"))?,
        claims,
    })
}

fn check_signature(set: &KeySet, jws: &Jws<'_>) -> Result<(), IdTokenError> {
    let mut candidates = set
        .keys
        .iter()
        .filter(|k| k.alg == jws.alg)
        .filter(|k| jws.kid.is_none() || k.kid == jws.kid)
        .peekable();
    if candidates.peek().is_none() {
        return Err(IdTokenError::UnknownKey);
    }
    if candidates.any(|k| k.verifies(jws.input, &jws.sig)) {
        Ok(())
    } else {
        Err(IdTokenError::BadSignature)
    }
}

fn check_claims(
    claims: &Map<String, Value>,
    client_id: &str,
    issuer: &IssuerRule,
    now: i64,
) -> Result<(), IdTokenError> {
    let aud_ok = match claims.get("aud") {
        Some(Value::String(a)) => a == client_id,
        Some(Value::Array(list)) => list.iter().any(|a| a.as_str() == Some(client_id)),
        _ => false,
    };
    // Several audiences: the token must name this client as its authorized party.
    let many = matches!(claims.get("aud"), Some(Value::Array(l)) if l.len() > 1);
    let azp_ok = match claims.get("azp") {
        Some(a) => a.as_str() == Some(client_id),
        None => !many,
    };
    if !aud_ok || !azp_ok {
        return Err(IdTokenError::Audience);
    }
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or(IdTokenError::Malformed("no exp"))?;
    if now > exp + LEEWAY_SECS {
        return Err(IdTokenError::Expired);
    }
    if claims
        .get("nbf")
        .and_then(Value::as_i64)
        .is_some_and(|nbf| now + LEEWAY_SECS < nbf)
    {
        return Err(IdTokenError::NotYetValid);
    }
    if !issuer.accepts(claims) {
        return Err(IdTokenError::Issuer);
    }
    Ok(())
}

/// Verify `token` against `set`: the signature, then the claims.
fn verify_with(
    set: &KeySet,
    token: &str,
    client_id: &str,
    issuer: &IssuerRule,
    now: i64,
) -> Result<Map<String, Value>, IdTokenError> {
    let jws = split(token)?;
    check_signature(set, &jws)?;
    check_claims(&jws.claims, client_id, issuer, now)?;
    Ok(jws.claims)
}

type CacheKey = (String, TargetPolicy);
type Cached = (Instant, Result<Arc<KeySet>, String>);

static SETS: LazyLock<Mutex<HashMap<CacheKey, Cached>>> = LazyLock::new(Mutex::default);

/// When each set was last refetched for an unknown `kid`, success or not.
static REFETCHED: LazyLock<Mutex<HashMap<CacheKey, Instant>>> = LazyLock::new(Mutex::default);

/// Claim the one early refetch `key` may run now: none within
/// [`REFETCH_GAP`] of the set's fetch or of the last attempt. Single-flight:
/// concurrent callers see the claim and keep the cached set.
fn claim_refetch(key: &CacheKey, set: &KeySet) -> bool {
    if set.fetched.elapsed() < REFETCH_GAP {
        return false;
    }
    let mut map = REFETCHED.lock().unwrap_or_else(PoisonError::into_inner);
    if map.get(key).is_some_and(|at| at.elapsed() < REFETCH_GAP) {
        return false;
    }
    if map.len() >= MAX_SETS {
        map.retain(|_, at| at.elapsed() < REFETCH_GAP);
    }
    map.insert(key.clone(), Instant::now());
    true
}

fn cached(key: &CacheKey) -> Option<Result<Arc<KeySet>, String>> {
    let map = SETS.lock().unwrap_or_else(PoisonError::into_inner);
    let (at, found) = map.get(key)?;
    let ttl = found.as_ref().map_or(FAIL_TTL, |s| s.ttl);
    (at.elapsed() < ttl).then(|| found.clone())
}

fn remember(key: CacheKey, found: Result<Arc<KeySet>, String>) {
    let mut map = SETS.lock().unwrap_or_else(PoisonError::into_inner);
    if map.len() >= MAX_SETS && !map.contains_key(&key) {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(key, (Instant::now(), found));
}

async fn fetch(provider: &OAuth2Provider, url: &str) -> Result<Arc<KeySet>, String> {
    let (body, max_age) = provider
        .get_document(url)
        .await
        .map_err(|e| e.to_string())?;
    let ttl = max_age.map_or(DEFAULT_TTL, |s| {
        Duration::from_secs(s).clamp(MIN_TTL, MAX_TTL)
    });
    KeySet::parse(&body, ttl)
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

/// Verify `token` for `provider`: its JWKS, `client_id` and issuer rule.
///
/// # Errors
/// [`IdTokenError`]; the caller then trusts none of the token's claims.
pub(crate) async fn verify(
    provider: &OAuth2Provider,
    token: &str,
) -> Result<Map<String, Value>, IdTokenError> {
    let (url, issuer) = provider.id_token_keys().ok_or(IdTokenError::NoKeys)?;
    let jws = split(token)?;
    let key: CacheKey = (url.to_owned(), provider.egress_policy());
    let mut set = match cached(&key) {
        Some(found) => found,
        None => {
            let found = fetch(provider, url).await;
            remember(key.clone(), found.clone());
            found
        }
    }
    .map_err(IdTokenError::Keys)?;
    // A key the set lacks may be a fresh rotation: refetch, at most once a
    // minute per set, failed attempts included; a failure keeps the old set.
    if !set.knows(jws.kid.as_deref()) && claim_refetch(&key, &set) {
        match fetch(provider, url).await {
            Ok(fresh) => {
                remember(key, Ok(Arc::clone(&fresh)));
                set = fresh;
            }
            Err(e) => tracing::warn!(target: "rustango::sso", "JWKS refetch failed: {e}"),
        }
    }
    let now = chrono::Utc::now().timestamp();
    verify_with(&set, token, &provider.client_id, issuer, now)
}

#[cfg(test)]
pub(crate) mod testing {
    //! Signing helpers for tests: an ES256 key made per run, and a fixed RSA key.

    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{self as s, KeyPair as _};

    pub(crate) fn enc(v: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(v)
    }

    /// A P-256 signer and its JWK.
    pub(crate) struct Es256 {
        pair: s::EcdsaKeyPair,
        pub(crate) kid: String,
    }

    impl Es256 {
        pub(crate) fn new(kid: &str) -> Self {
            let rng = SystemRandom::new();
            let doc =
                s::EcdsaKeyPair::generate_pkcs8(&s::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let pair = s::EcdsaKeyPair::from_pkcs8(
                &s::ECDSA_P256_SHA256_FIXED_SIGNING,
                doc.as_ref(),
                &rng,
            )
            .unwrap();
            Self {
                pair,
                kid: kid.into(),
            }
        }

        pub(crate) fn jwk(&self) -> Value {
            let p = self.pair.public_key().as_ref();
            serde_json::json!({
                "kty": "EC", "crv": "P-256", "use": "sig", "kid": self.kid,
                "x": enc(&p[1..33]), "y": enc(&p[33..65]),
            })
        }

        pub(crate) fn sign(&self, claims: &Value) -> String {
            let header = serde_json::json!({"alg": "ES256", "kid": self.kid, "typ": "JWT"});
            let input = format!(
                "{}.{}",
                enc(header.to_string().as_bytes()),
                enc(claims.to_string().as_bytes())
            );
            let sig = self
                .pair
                .sign(&SystemRandom::new(), input.as_bytes())
                .unwrap();
            format!("{input}.{}", enc(sig.as_ref()))
        }
    }

    /// The fixed RS256 test key (test data only).
    pub(crate) fn rs256() -> s::RsaKeyPair {
        s::RsaKeyPair::from_der(include_bytes!("testdata/rsa_test_key.der")).unwrap()
    }

    pub(crate) fn rs256_jwk(pair: &s::RsaKeyPair, kid: &str) -> Value {
        let public: s::RsaPublicKeyComponents<Vec<u8>> = pair.public().into();
        serde_json::json!({
            "kty": "RSA", "alg": "RS256", "kid": kid,
            "n": enc(&public.n), "e": enc(&public.e),
        })
    }

    pub(crate) fn rs256_sign(pair: &s::RsaKeyPair, kid: &str, claims: &Value) -> String {
        let header = serde_json::json!({"alg": "RS256", "kid": kid});
        let input = format!(
            "{}.{}",
            enc(header.to_string().as_bytes()),
            enc(claims.to_string().as_bytes())
        );
        let mut sig = vec![0; pair.public().modulus_len()];
        pair.sign(
            &s::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            input.as_bytes(),
            &mut sig,
        )
        .unwrap();
        format!("{input}.{}", enc(&sig))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const CID: &str = "client-1";

    fn issuer() -> IssuerRule {
        IssuerRule::AnyOf(vec!["https://idp.test".into()])
    }

    fn claims(now: i64) -> Value {
        serde_json::json!({
            "iss": "https://idp.test", "sub": "u1", "aud": CID,
            "exp": now + 300, "iat": now, "email": "ann@client.org",
        })
    }

    fn set(keys: Vec<Value>) -> KeySet {
        let doc = serde_json::json!({ "keys": keys });
        KeySet::parse(doc.to_string().as_bytes(), DEFAULT_TTL).unwrap()
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[test]
    fn es256_and_rs256_tokens_verify() {
        let ec = Es256::new("ec1");
        let rsa = rs256();
        let keys = set(vec![ec.jwk(), rs256_jwk(&rsa, "rsa1")]);
        let t = now();
        let got = verify_with(&keys, &ec.sign(&claims(t)), CID, &issuer(), t).unwrap();
        assert_eq!(got["sub"], "u1");
        let tok = rs256_sign(&rsa, "rsa1", &claims(t));
        assert!(verify_with(&keys, &tok, CID, &issuer(), t).is_ok());
    }

    #[test]
    fn a_token_that_fails_any_check_is_refused() {
        let ec = Es256::new("ec1");
        let keys = set(vec![ec.jwk()]);
        let t = now();
        let with = |f: &dyn Fn(&mut Value)| {
            let mut c = claims(t);
            f(&mut c);
            verify_with(&keys, &ec.sign(&c), CID, &issuer(), t)
        };
        assert_eq!(
            with(&|c| c["aud"] = "other".into()),
            Err(IdTokenError::Audience)
        );
        assert_eq!(
            with(&|c| c["aud"] = serde_json::json!(["x", CID])),
            Err(IdTokenError::Audience),
            "several audiences and no azp"
        );
        assert!(with(&|c| {
            c["aud"] = serde_json::json!(["x", CID]);
            c["azp"] = CID.into();
        })
        .is_ok());
        assert!(with(&|c| c["aud"] = serde_json::json!([CID])).is_ok());
        assert_eq!(
            with(&|c| c["azp"] = "other".into()),
            Err(IdTokenError::Audience)
        );
        assert_eq!(
            with(&|c| c["exp"] = (t - 120).into()),
            Err(IdTokenError::Expired)
        );
        assert!(
            with(&|c| c["exp"] = (t - 30).into()).is_ok(),
            "within leeway"
        );
        assert_eq!(
            with(&|c| c["nbf"] = (t + 600).into()),
            Err(IdTokenError::NotYetValid)
        );
        assert_eq!(
            with(&|c| c["iss"] = "https://evil.test".into()),
            Err(IdTokenError::Issuer)
        );
        assert!(matches!(
            with(&|c| {
                c.as_object_mut().unwrap().remove("exp");
            }),
            Err(IdTokenError::Malformed(_))
        ));
        // Another key's signature, a tampered payload, an unknown kid, alg none.
        let other = Es256::new("ec1");
        assert_eq!(
            verify_with(&keys, &other.sign(&claims(t)), CID, &issuer(), t),
            Err(IdTokenError::BadSignature)
        );
        let tok = ec.sign(&claims(t));
        let mut parts: Vec<&str> = tok.split('.').collect();
        let forged = enc(serde_json::json!({"iss": "https://idp.test", "sub": "admin", "aud": CID, "exp": t + 300}).to_string().as_bytes());
        parts[1] = &forged;
        assert_eq!(
            verify_with(&keys, &parts.join("."), CID, &issuer(), t),
            Err(IdTokenError::BadSignature)
        );
        let stranger = Es256::new("ec2");
        assert_eq!(
            verify_with(&keys, &stranger.sign(&claims(t)), CID, &issuer(), t),
            Err(IdTokenError::UnknownKey)
        );
        let none = format!(
            "{}.{}.",
            enc(br#"{"alg":"none"}"#),
            enc(claims(t).to_string().as_bytes())
        );
        assert!(matches!(
            verify_with(&keys, &none, CID, &issuer(), t),
            Err(IdTokenError::Algorithm(_))
        ));
        // An RS256 header never picks an EC key, nor an HS256 one anything.
        let hs = format!(
            "{}.{}.x",
            enc(br#"{"alg":"HS256","kid":"ec1"}"#),
            enc(b"{}")
        );
        assert!(matches!(
            verify_with(&keys, &hs, CID, &issuer(), t),
            Err(IdTokenError::Algorithm(_))
        ));
    }

    #[test]
    fn microsoft_issuer_carries_the_tid() {
        let rule = IssuerRule::MicrosoftTenant;
        let c = |iss: &str, tid: &str| {
            serde_json::json!({"iss": iss, "tid": tid})
                .as_object()
                .unwrap()
                .clone()
        };
        assert!(rule.accepts(&c("https://login.microsoftonline.com/t1/v2.0", "t1")));
        assert!(!rule.accepts(&c("https://login.microsoftonline.com/t2/v2.0", "t1")));
        assert!(!rule.accepts(&c("https://login.microsoftonline.com//v2.0", "")));
    }

    #[test]
    fn jwks_skips_keys_it_cannot_use() {
        let ec = Es256::new("ec1");
        let mut enc_key = ec.jwk();
        enc_key["use"] = "enc".into();
        let mut wrong_alg = ec.jwk();
        wrong_alg["alg"] = "RS256".into();
        let s = set(vec![
            enc_key,
            wrong_alg,
            serde_json::json!({"kty": "oct", "k": "AAAA"}),
            serde_json::json!({"kty": "EC", "crv": "P-384", "x": "AA", "y": "AA"}),
        ]);
        assert!(s.keys.is_empty());
    }

    /// A key set server; `rotate` swaps in a second key.
    /// `later: None` answers every refetch with a 502.
    async fn server(
        first: Value,
        later: Option<Value>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/jwks",
            axum::routing::get(move || {
                let n = h.fetch_add(1, Ordering::SeqCst);
                let body = if n == 0 {
                    Some(first.clone())
                } else {
                    later.clone()
                };
                async move {
                    body.map(axum::Json)
                        .ok_or(axum::http::StatusCode::BAD_GATEWAY)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("{base}/jwks"), hits)
    }

    fn provider(jwks: &str) -> OAuth2Provider {
        OAuth2Provider::new("oidc", CID, "s", "https://app/cb", "https://a", "https://t")
            .with_id_token_keys(jwks, issuer())
            .with_egress(TargetPolicy::Public(crate::outbound::Allowlist::parse(
                "127.0.0.1",
            )))
    }

    /// The set is cached; an unknown kid refetches only after the gap.
    #[tokio::test]
    async fn key_sets_are_cached_and_refetched_on_rotation() {
        use std::sync::atomic::Ordering;
        let (old, new) = (Es256::new("old"), Es256::new("new"));
        let (url, hits) = server(
            serde_json::json!({"keys": [old.jwk()]}),
            Some(serde_json::json!({"keys": [old.jwk(), new.jwk()]})),
        )
        .await;
        let p = provider(&url);
        let t = now();
        verify(&p, &old.sign(&claims(t))).await.unwrap();
        verify(&p, &old.sign(&claims(t))).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "cached");
        // Rotated in under a minute: not refetched yet.
        assert_eq!(
            verify(&p, &new.sign(&claims(t))).await,
            Err(IdTokenError::UnknownKey)
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Age the set past the gap: one refetch finds the new key.
        let key = (url.clone(), p.egress_policy());
        {
            let mut map = SETS.lock().unwrap();
            let entry = map.get_mut(&key).unwrap();
            let mut aged = KeySet::parse(
                serde_json::json!({"keys": [old.jwk()]})
                    .to_string()
                    .as_bytes(),
                DEFAULT_TTL,
            )
            .unwrap();
            aged.fetched = Instant::now().checked_sub(REFETCH_GAP).unwrap();
            entry.1 = Ok(Arc::new(aged));
        }
        verify(&p, &new.sign(&claims(t))).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// A failed refetch counts against the gap: unknown kids cannot make
    /// every request fetch, and the cached set keeps working.
    #[tokio::test]
    async fn a_failed_refetch_is_not_retried_per_request() {
        use std::sync::atomic::Ordering;
        let (old, stranger) = (Es256::new("old"), Es256::new("stranger"));
        let (url, hits) = server(serde_json::json!({"keys": [old.jwk()]}), None).await;
        let p = provider(&url);
        let t = now();
        verify(&p, &old.sign(&claims(t))).await.unwrap();
        let key = (url.clone(), p.egress_policy());
        {
            let mut aged = KeySet::parse(
                serde_json::json!({"keys": [old.jwk()]})
                    .to_string()
                    .as_bytes(),
                DEFAULT_TTL,
            )
            .unwrap();
            aged.fetched = Instant::now().checked_sub(REFETCH_GAP).unwrap();
            SETS.lock().unwrap().get_mut(&key).unwrap().1 = Ok(Arc::new(aged));
        }
        let tok = stranger.sign(&claims(t));
        let runs = (0..5).map(|_| verify(&p, &tok));
        for r in futures_util::future::join_all(runs).await {
            assert_eq!(r, Err(IdTokenError::UnknownKey));
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "one refetch for five requests"
        );
        assert_eq!(
            verify(&p, &stranger.sign(&claims(t))).await,
            Err(IdTokenError::UnknownKey)
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2, "the failure is remembered");
        verify(&p, &old.sign(&claims(t))).await.unwrap();
    }
}
