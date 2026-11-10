//! The cookie side of [`SessionStore`]: a layer that loads the session
//! named by the cookie and saves it after the handler, plus the
//! [`CurrentSession`] extractor handlers read and write it through.
//!
//! The cookie is always `HttpOnly`; `Secure` and `SameSite=Lax` by default.
//! Under tenancy the store is scoped to the request's tenant.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Response, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tower::Service;

use super::{Session, SessionError, SessionStore};
use crate::cache::keyspace::KeySpace;
use crate::cookies::{Cookie, SameSite};

/// Default session cookie name, apart from the tenancy and admin cookies.
const DEFAULT_COOKIE: &str = "rustango_sid";

/// The session key holding the CSRF token in session-bound mode.
#[cfg(feature = "csrf")]
pub(crate) const CSRF_KEY: &str = "_csrf";

/// How the layer writes its cookie. There is no switch for `HttpOnly`.
#[derive(Debug, Clone)]
struct CookieSpec {
    name: String,
    path: String,
    domain: Option<String>,
    same_site: SameSite,
    secure: bool,
    rolling: bool,
}

impl CookieSpec {
    fn issue(&self, id: &str, ttl: std::time::Duration) -> Option<HeaderValue> {
        let mut c = Cookie::new(&self.name, id)
            .path(&self.path)
            .http_only()
            .same_site(self.same_site)
            .max_age(ttl);
        // Browsers drop `SameSite=None` without `Secure`.
        if self.secure || self.same_site == SameSite::None {
            c = c.secure();
        }
        if let Some(d) = &self.domain {
            c = c.domain(d);
        }
        c.header_value()
    }

    fn remove(&self) -> Option<HeaderValue> {
        let mut c = Cookie::deletion(&self.name, &self.path);
        if let Some(d) = &self.domain {
            c = c.domain(d);
        }
        c.header_value()
    }
}

/// Loads the session named by the request's cookie and saves it once the
/// handler returns. Mount it outside every route that takes [`CurrentSession`].
///
/// ```ignore
/// use rustango::sessions::{SessionLayer, SessionStore};
/// let app = Router::new()
///     .route("/me", get(me))
///     .layer(SessionLayer::new(SessionStore::new(cache)));
/// ```
#[derive(Clone)]
pub struct SessionLayer {
    store: SessionStore,
    cookie: Arc<CookieSpec>,
}

impl SessionLayer {
    /// A layer over `store`; the cookie lives as long as the store's TTL.
    #[must_use]
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            cookie: Arc::new(CookieSpec {
                name: DEFAULT_COOKIE.to_owned(),
                path: "/".to_owned(),
                domain: None,
                same_site: SameSite::Lax,
                secure: true,
                rolling: false,
            }),
        }
    }

    fn spec(mut self, f: impl FnOnce(&mut CookieSpec)) -> Self {
        f(Arc::make_mut(&mut self.cookie));
        self
    }

    /// Cookie name. Default `rustango_sid`.
    #[must_use]
    pub fn cookie_name(self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.spec(|c| c.name = name)
    }

    /// Cookie `Path`. Default `/`.
    #[must_use]
    pub fn cookie_path(self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.spec(|c| c.path = path)
    }

    /// Cookie `Domain`. Default none: only the host that set it.
    #[must_use]
    pub fn cookie_domain(self, domain: impl Into<String>) -> Self {
        let domain = domain.into();
        self.spec(|c| c.domain = Some(domain))
    }

    /// `SameSite`. Default `Lax`; `None` always adds `Secure`.
    #[must_use]
    pub fn same_site(self, same_site: SameSite) -> Self {
        self.spec(|c| c.same_site = same_site)
    }

    /// Drop `Secure` for local plain-HTTP development.
    #[must_use]
    pub fn allow_insecure_for_dev(self) -> Self {
        self.spec(|c| c.secure = false)
    }

    /// Renew the session's TTL and cookie on every request, not only on save.
    #[must_use]
    pub fn rolling(self) -> Self {
        self.spec(|c| c.rolling = true)
    }

    /// This layer with a CSRF check against a token kept in the session,
    /// in place of the double-submit cookie. Read the token with
    /// [`CurrentSession::csrf_token`]; send it as `X-CSRF-Token`.
    #[cfg(feature = "csrf")]
    #[must_use]
    pub fn with_csrf(
        self,
        cfg: crate::forms::csrf::CsrfConfig,
    ) -> tower::layer::util::Stack<crate::forms::csrf::CsrfLayer, Self> {
        tower::layer::util::Stack::new(crate::forms::csrf::CsrfLayer::session_bound(cfg), self)
    }
}

impl<S> tower::Layer<S> for SessionLayer {
    type Service = SessionService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        SessionService {
            inner,
            store: self.store.clone(),
            cookie: Arc::clone(&self.cookie),
        }
    }
}

/// The service [`SessionLayer`] wraps routes in.
#[derive(Clone)]
pub struct SessionService<S> {
    inner: S,
    store: SessionStore,
    cookie: Arc<CookieSpec>,
}

impl<S> Service<Request<Body>> for SessionService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        let store = self.store.clone();
        let cookie = Arc::clone(&self.cookie);
        Box::pin(async move {
            let (mut parts, body) = req.into_parts();
            let Ok(space) = KeySpace::of_request(&mut parts).await else {
                return Ok(status(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "tenant could not be resolved",
                ));
            };
            let store = store.in_space(space);
            let sent = crate::cookies::cookie_from_headers(&parts.headers, &cookie.name)
                .filter(|v| super::is_session_id(v))
                .map(str::to_owned);
            let loaded = match &sent {
                Some(id) => match store.load(id).await {
                    Ok(s) => s,
                    Err(e) => return Ok(store_failed(&e)),
                },
                None => None,
            };
            let current =
                CurrentSession::new(store.clone(), loaded.as_ref().and(sent.clone()), loaded);
            parts.extensions.insert(current.clone());
            let mut response = inner.call(Request::from_parts(parts, body)).await?;
            match current.finish(&cookie, sent.is_some()).await {
                Ok(Some(set)) => {
                    response.headers_mut().append(header::SET_COOKIE, set);
                    // A response that sets a session cookie is never shared.
                    response.headers_mut().insert(
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("private, no-store"),
                    );
                }
                Ok(None) => {}
                Err(e) => return Ok(store_failed(&e)),
            }
            Ok(response)
        })
    }
}

fn status(code: StatusCode, body: &'static str) -> Response<Body> {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = code;
    r
}

fn store_failed(e: &SessionError) -> Response<Body> {
    tracing::error!(target: "rustango::sessions", error = %e, "session store failed");
    status(
        StatusCode::INTERNAL_SERVER_ERROR,
        "session store unavailable",
    )
}

/// What the handler did to the session.
#[derive(Debug, Default)]
struct State {
    /// The id the session is stored under; `None` until first saved.
    id: Option<String>,
    session: Session,
    /// Save under a new id, dropping the old one.
    rotate: bool,
    /// The id changed during the request: send the cookie.
    issued: bool,
    /// Logged out: remove the cookie.
    ended: bool,
}

/// The request's session, from [`SessionLayer`]. Changes are saved after
/// the handler returns; the cookie is sent only when the id changes.
#[derive(Clone)]
pub struct CurrentSession {
    store: SessionStore,
    state: Arc<Mutex<State>>,
}

impl CurrentSession {
    fn new(store: SessionStore, id: Option<String>, session: Option<Session>) -> Self {
        Self {
            store,
            state: Arc::new(Mutex::new(State {
                id,
                session: session.unwrap_or_default(),
                ..State::default()
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A typed value; `None` when missing or of another type.
    #[must_use]
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.lock().session.get(key)
    }

    /// Store a value; saved after the handler returns.
    pub fn set<T: Serialize>(&self, key: impl Into<String>, value: T) {
        self.lock().session.set(key, value);
    }

    /// Remove a key and return its old value.
    pub fn remove(&self, key: &str) -> Option<Value> {
        self.lock().session.remove(key)
    }

    /// The session id, once the session is stored.
    #[must_use]
    pub fn id(&self) -> Option<String> {
        self.lock().id.clone()
    }

    /// The user [`Self::login`] bound this session to.
    #[must_use]
    pub fn user(&self) -> Option<String> {
        self.lock().session.user()
    }

    /// The request's store, scoped to its tenant: list or revoke the
    /// user's other sessions through it.
    #[must_use]
    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    /// Save under a new id after the handler, dropping the old one. Call it
    /// when the user's rights change; [`Self::login`] does it for you.
    pub fn cycle_id(&self) {
        self.lock().rotate = true;
    }

    /// Bind the session to `user` under a new id, so an id fixed before login
    /// is worthless. Keeps the data; drops the CSRF token so a new one is minted.
    ///
    /// # Errors
    /// A cache or serialization error.
    pub async fn login(&self, user: &str) -> Result<(), SessionError> {
        #[cfg_attr(not(feature = "csrf"), allow(unused_mut))]
        let (old, mut session) = {
            let st = self.lock();
            (st.id.clone(), st.session.clone())
        };
        #[cfg(feature = "csrf")]
        session.remove(CSRF_KEY);
        let (id, saved) = self.store.login_session(user, &session).await?;
        if let Some(old) = old {
            self.store.destroy(&old).await?;
        }
        let mut st = self.lock();
        st.session = saved;
        st.id = Some(id);
        st.issued = true;
        st.rotate = false;
        st.ended = false;
        Ok(())
    }

    /// Delete the session and remove the cookie.
    ///
    /// # Errors
    /// A cache error.
    pub async fn logout(&self) -> Result<(), SessionError> {
        let old = self.lock().id.take();
        if let Some(id) = old {
            self.store.destroy(&id).await?;
        }
        let mut st = self.lock();
        st.session = Session::default();
        st.ended = true;
        Ok(())
    }

    /// The session's CSRF token, minted on first use. Session-bound CSRF
    /// ([`SessionLayer::with_csrf`]) checks submissions against it.
    #[cfg(feature = "csrf")]
    #[must_use]
    pub fn csrf_token(&self) -> String {
        let mut st = self.lock();
        if let Some(t) = st.session.get::<String>(CSRF_KEY) {
            return t;
        }
        let t = crate::forms::csrf::mint_token();
        st.session.set(CSRF_KEY, &t);
        t
    }

    /// The token without minting one: the session-bound check's side.
    #[cfg(feature = "csrf")]
    pub(crate) fn existing_csrf_token(&self) -> Option<String> {
        self.lock().session.get(CSRF_KEY)
    }

    /// Save what changed; the `Set-Cookie` value to send, if any.
    async fn finish(
        &self,
        cookie: &CookieSpec,
        had_cookie: bool,
    ) -> Result<Option<HeaderValue>, SessionError> {
        let (id, session, rotate, issued, ended) = {
            let st = self.lock();
            (
                st.id.clone(),
                st.session.clone(),
                st.rotate,
                st.issued,
                st.ended,
            )
        };
        let ttl = self.store.ttl_value();
        if ended {
            return Ok(had_cookie.then(|| cookie.remove()).flatten());
        }
        match id {
            Some(old) if rotate => {
                let new = self.store.reissue(&old, &session).await?;
                Ok(cookie.issue(&new, ttl))
            }
            Some(id) if session.is_dirty() => {
                self.store.save_with_id(&id, &session).await?;
                Ok(cookie.issue(&id, ttl))
            }
            Some(id) if issued => Ok(cookie.issue(&id, ttl)),
            Some(id) if cookie.rolling => {
                if self.store.touch(&id).await? {
                    Ok(cookie.issue(&id, ttl))
                } else {
                    Ok(None)
                }
            }
            Some(_) => Ok(None),
            None if session.is_dirty() => {
                let new = self.store.save(&session).await?;
                Ok(cookie.issue(&new, ttl))
            }
            // A dead or forged cookie: drop it.
            None => Ok(had_cookie.then(|| cookie.remove()).flatten()),
        }
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for CurrentSession {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "CurrentSession needs SessionLayer mounted outside this route",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{BoxedCache, InMemoryCache};
    use axum::routing::{get, post};
    use tower::ServiceExt as _;

    fn app(store: SessionStore) -> axum::Router {
        axum::Router::new()
            .route(
                "/set",
                get(|s: CurrentSession| async move {
                    s.set("n", 1);
                    "ok"
                }),
            )
            .route(
                "/get",
                get(|s: CurrentSession| async move {
                    format!("{:?} {:?}", s.get::<i64>("n"), s.user())
                }),
            )
            .route(
                "/login",
                post(
                    |s: CurrentSession| async move { s.login("u1").await.map(|()| "in").unwrap() },
                ),
            )
            .route(
                "/logout",
                post(|s: CurrentSession| async move { s.logout().await.map(|()| "out").unwrap() }),
            )
            .layer(SessionLayer::new(store))
    }

    fn store() -> SessionStore {
        let cache: BoxedCache = Arc::new(InMemoryCache::new());
        SessionStore::new(cache)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        sid: Option<&str>,
    ) -> (String, Option<String>) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(id) = sid {
            req = req.header("cookie", format!("{DEFAULT_COOKIE}={id}"));
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(res.status().is_success(), "{uri}: {}", res.status());
        let set = res
            .headers()
            .get(header::SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        (String::from_utf8(body.to_vec()).unwrap(), set)
    }

    fn sid(set_cookie: &str) -> String {
        let pair = set_cookie.split(';').next().unwrap();
        pair.split_once('=').unwrap().1.to_owned()
    }

    /// The cookie is HttpOnly, Secure and Lax, lives for the store TTL,
    /// and is sent only when the session is first saved.
    #[tokio::test]
    async fn cookie_carries_secure_defaults_and_round_trips() {
        let app = app(store());
        let (_, set) = send(&app, "GET", "/set", None).await;
        let set = set.expect("no session cookie");
        for flag in [
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Max-Age=1209600",
            "Path=/",
        ] {
            assert!(set.contains(flag), "{flag} missing from {set}");
        }
        let id = sid(&set);
        let (body, again) = send(&app, "GET", "/get", Some(&id)).await;
        assert_eq!(body, "Some(1) None");
        assert!(again.is_none(), "unchanged session re-sent its cookie");
    }

    /// Login moves the data to a new id; the id fixed before login is dead.
    #[tokio::test]
    async fn login_rotates_the_id() {
        let app = app(store());
        let (_, set) = send(&app, "GET", "/set", None).await;
        let before = sid(&set.unwrap());
        let (_, set) = send(&app, "POST", "/login", Some(&before)).await;
        let after = sid(&set.expect("login sent no cookie"));
        assert_ne!(before, after);
        assert_eq!(
            send(&app, "GET", "/get", Some(&before)).await.0,
            "None None"
        );
        assert_eq!(
            send(&app, "GET", "/get", Some(&after)).await.0,
            "Some(1) Some(\"u1\")"
        );
    }

    /// Logout deletes the session and expires the cookie.
    #[tokio::test]
    async fn logout_ends_the_session_and_cookie() {
        let app = app(store());
        let (_, set) = send(&app, "GET", "/set", None).await;
        let id = sid(&set.unwrap());
        let (_, set) = send(&app, "POST", "/logout", Some(&id)).await;
        assert!(set.expect("no removal cookie").contains("Max-Age=0"));
        assert_eq!(send(&app, "GET", "/get", Some(&id)).await.0, "None None");
    }

    /// A user's sessions listed through the layer's store; revoking one
    /// logs that browser out.
    #[tokio::test]
    async fn revoked_session_is_logged_out() {
        let store = store();
        let app = app(store.clone());
        let (_, set) = send(&app, "POST", "/login", None).await;
        let id = sid(&set.unwrap());
        let listed = store.sessions_of("u1").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(store.revoke("u1", &listed[0].key).await.unwrap());
        assert_eq!(send(&app, "GET", "/get", Some(&id)).await.0, "None None");
    }

    /// Under tenancy a session saved for one tenant never loads for another,
    /// and a request whose tenant failed to resolve gets no session.
    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn sessions_stay_with_their_tenant() {
        use crate::tenancy::{RequestTenant, TenantSlug};
        let app = app(store());
        let send_as = |uri: &'static str, tenant: RequestTenant, sid: Option<String>| {
            let app = app.clone();
            async move {
                let mut req = Request::builder().uri(uri).extension(tenant);
                if let Some(id) = sid {
                    req = req.header("cookie", format!("{DEFAULT_COOKIE}={id}"));
                }
                app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
            }
        };
        let acme = || RequestTenant::Tenant(TenantSlug::new(1, "acme"));
        let res = send_as("/set", acme(), None).await;
        let id = sid(res.headers()[header::SET_COOKIE].to_str().unwrap());
        let body = |res: Response<Body>| async move {
            let b = axum::body::to_bytes(res.into_body(), 1 << 16)
                .await
                .unwrap();
            String::from_utf8(b.to_vec()).unwrap()
        };
        assert_eq!(
            body(send_as("/get", acme(), Some(id.clone())).await).await,
            "Some(1) None"
        );
        let beta = RequestTenant::Tenant(TenantSlug::new(2, "beta"));
        assert_eq!(
            body(send_as("/get", beta, Some(id.clone())).await).await,
            "None None"
        );
        let global = send_as("/get", RequestTenant::Global, Some(id.clone())).await;
        assert_eq!(body(global).await, "None None");
        let unknown = send_as("/get", RequestTenant::Unknown, Some(id)).await;
        assert_eq!(unknown.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[cfg(feature = "csrf")]
    async fn csrf_post(
        app: &axum::Router,
        sid: Option<&str>,
        extra: &[(&str, String)],
    ) -> StatusCode {
        let mut req = Request::builder().method("POST").uri("/act");
        if let Some(id) = sid {
            req = req.header("cookie", format!("{DEFAULT_COOKIE}={id}"));
        }
        for (k, v) in extra {
            req = req.header(*k, v);
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// Session-bound CSRF takes only the session's token: a matching
    /// double-submit pair is refused, and login invalidates the old token.
    #[cfg(feature = "csrf")]
    #[tokio::test]
    async fn session_bound_csrf_checks_the_session_token() {
        use crate::forms::csrf::CsrfConfig;
        let app = axum::Router::new()
            .route(
                "/token",
                get(|s: CurrentSession| async move { s.csrf_token() }),
            )
            .route("/act", post(|| async { "done" }))
            .route(
                "/login",
                post(
                    |s: CurrentSession| async move { s.login("u1").await.map(|()| "in").unwrap() },
                ),
            )
            .layer(SessionLayer::new(store()).with_csrf(CsrfConfig::default()));
        let (token, set) = send(&app, "GET", "/token", None).await;
        let id = sid(&set.unwrap());
        let hdr = |t: &str| ("x-csrf-token", t.to_owned());
        assert_eq!(
            csrf_post(&app, Some(&id), &[hdr(&token)]).await,
            StatusCode::OK
        );
        assert_eq!(
            csrf_post(&app, Some(&id), &[hdr("x")]).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            csrf_post(&app, None, &[hdr(&token)]).await,
            StatusCode::FORBIDDEN
        );
        let pair = "A".repeat(43);
        let planted = [
            hdr(&pair),
            (
                "cookie",
                format!("{DEFAULT_COOKIE}={id}; rustango_csrf={pair}"),
            ),
        ];
        assert_eq!(csrf_post(&app, None, &planted).await, StatusCode::FORBIDDEN);
        // Login with the token, then the old token is dead.
        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("cookie", format!("{DEFAULT_COOKIE}={id}"))
            .header("x-csrf-token", &token)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let fresh = sid(res.headers()[header::SET_COOKIE].to_str().unwrap());
        assert_eq!(
            csrf_post(&app, Some(&fresh), &[hdr(&token)]).await,
            StatusCode::FORBIDDEN
        );
    }
}
