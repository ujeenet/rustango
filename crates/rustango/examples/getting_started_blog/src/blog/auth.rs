//! Sign-up, sign-in and the Bearer-token guard for the blog API (Step 14).

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::api_errors::ApiError;
use rustango::core::{ModelSchema, WhereExpr};
use rustango::jwt::{self, Claims};
use rustango::login_throttle::{self, ClientIp, LoginRefused, LoginScope};
use rustango::passwords::{self, PasswordError};
use rustango::prelude::*;
use rustango::sql::{sqlx, ExecError, Pool};
use rustango::viewset::{match_nothing, ViewSetFilter, WritePin};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::blog::models::{ApiUser, Post};

/// How long a token from `/api/login` stays valid.
pub const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// What `/api/register` and `/api/login` read from the request body.
#[derive(Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// What `/api/login` sends back.
#[derive(Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
}

/// The signed-in user. The guard puts it on the request for the handlers.
#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
}

/// The signing key, from the `JWT_SECRET` environment variable.
fn jwt_secret() -> Result<Vec<u8>, ApiError> {
    match std::env::var("JWT_SECRET") {
        Ok(secret) if secret.len() >= 32 => Ok(secret.into_bytes()),
        _ => Err(ApiError::logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth",
            "JWT_SECRET is unset or shorter than 32 bytes",
        )),
    }
}

/// Usernames are stored lower-case, so `Alice` and `alice` are one account.
fn normalize(username: &str) -> String {
    username.trim().to_lowercase()
}

/// A 500 that logs the cause and tells the client nothing about it.
fn server_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::logged(StatusCode::INTERNAL_SERVER_ERROR, "auth", e)
}

/// The same answer for an unknown user and a wrong password.
fn bad_credentials() -> ApiError {
    ApiError::unauthorized("invalid username or password")
}

/// `POST /api/register` — create an account. 201 on success, 409 if the name is taken.
pub async fn register(
    State(pool): State<Pool>,
    Json(body): Json<Credentials>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let username = normalize(&body.username);
    let name_ok = (3..=150).contains(&username.len())
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !name_ok {
        return Err(ApiError::validation(
            "username: 3-150 characters, letters, digits, '_', '-' or '.'",
        )
        .with_field("username"));
    }
    // An upper limit too: hashing a huge password costs the server.
    if body.password.len() > 128 || !passwords::strength_score(&body.password).is_empty() {
        return Err(ApiError::validation(
            "password: 12-128 characters, with a digit or a symbol, not a common one",
        )
        .with_field("password"));
    }

    // argon2 is slow on purpose; the `_async` call keeps the server responsive.
    let password_hash = passwords::hash_async(&body.password)
        .await
        .map_err(server_error)?;

    let mut user = ApiUser {
        id: Auto::default(),
        username,
        password_hash,
        created_at: Auto::default(),
    };
    match user.save(&pool).await {
        Ok(_) => {}
        // The unique index on `username` decides, so two parallel sign-ups cannot both win.
        Err(ExecError::Driver(sqlx::Error::Database(db))) if db.is_unique_violation() => {
            return Err(ApiError::conflict("username already taken").with_field("username"));
        }
        Err(e) => return Err(server_error(e)),
    }

    let id = user.id.get().copied().unwrap_or_default();
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "username": user.username })),
    ))
}

/// `POST /api/login` — check the password and hand out a short-lived token.
pub async fn login(
    State(pool): State<Pool>,
    ip: ClientIp,
    Json(body): Json<Credentials>,
) -> Result<Json<TokenResponse>, Response> {
    let username = normalize(&body.username);

    // Before any lookup: 429 once this IP (or everyone) failed too often.
    // `Sso(name)` is the scope an app can name: own limits, no account lock.
    let scope = LoginScope::Sso("blog-api".to_owned());
    let attempt = login_throttle::shared()
        .begin(&scope, &ip, &username)
        .await
        .map_err(IntoResponse::into_response)?;

    let found = ApiUser::objects()
        .where_(ApiUser::username.eq(username))
        .first(&pool)
        .await
        .map_err(|e| server_error(e).into_response())?;

    let Some(user) = found else {
        // Do the same slow work as a real check, so timing does not reveal the name is unknown.
        if passwords::verify_dummy_async(&body.password).await.is_err() {
            return Err(LoginRefused::Busy.into_response()); // 503: no hashing slot free
        }
        attempt.failed().await;
        return Err(bad_credentials().into_response());
    };
    let password_ok = match passwords::verify_async(&body.password, &user.password_hash).await {
        Ok(ok) => ok,
        Err(PasswordError::Busy) => return Err(LoginRefused::Busy.into_response()),
        // A corrupt stored hash counts as a wrong password, not a server error.
        Err(_) => false,
    };
    if !password_ok {
        attempt.failed().await;
        return Err(bad_credentials().into_response());
    }
    attempt.succeeded().await;

    let id = user.id.get().copied().unwrap_or_default();
    let claims = Claims::new(id.to_string()).ttl(TOKEN_TTL); // subject = user id
    let secret = jwt_secret().map_err(IntoResponse::into_response)?;
    let access_token =
        jwt::encode(&claims, &secret).map_err(|e| server_error(e).into_response())?;
    Ok(Json(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: TOKEN_TTL.as_secs(),
    }))
}

/// The user behind an `Authorization: Bearer <token>` header, or a 401.
pub async fn authenticate(pool: &Pool, parts: &Parts) -> Result<CurrentUser, ApiError> {
    let unauthorized = || ApiError::unauthorized("missing, invalid or expired token");

    let token = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .ok_or_else(unauthorized)?;
    // `decode` checks the signature and the expiry time.
    let claims = jwt::decode(token, &jwt_secret()?).map_err(|_| unauthorized())?;
    let id: i64 = claims
        .subject()
        .and_then(|sub| sub.parse().ok())
        .ok_or_else(unauthorized)?;

    // A deleted account's tokens stop working at once.
    let user = ApiUser::objects()
        .where_(ApiUser::id.eq(id))
        .first(pool)
        .await
        .map_err(server_error)?
        .ok_or_else(unauthorized)?;
    Ok(CurrentUser {
        id,
        username: user.username,
    })
}

/// Middleware: reads pass, every other method needs a valid token.
pub async fn require_user_for_writes(
    State(pool): State<Pool>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(next.run(request).await);
    }
    let (mut parts, body) = request.into_parts();
    let user = authenticate(&pool, &parts).await?;
    parts.extensions.insert(user);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// ViewSet rule: a new post's author is the signed-in user, and only the
/// author may change or delete it. Reads see every post.
pub struct AuthorFromToken;

impl ViewSetFilter for AuthorFromToken {
    fn filter(&self, _: &HashMap<String, String>, _: &'static ModelSchema) -> Vec<WhereExpr> {
        Vec::new()
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        if matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return Vec::new();
        }
        match parts.extensions.get::<CurrentUser>() {
            Some(user) => vec![Post::author_id.eq(user.id).into()],
            // No user: match no row. An empty Vec would mean "every row".
            None => vec![match_nothing(schema)],
        }
    }

    fn write_pins(&self, parts: &Parts, _: &'static ModelSchema) -> Vec<WritePin> {
        match parts.extensions.get::<CurrentUser>() {
            // Whatever `author_id` the body sends, this value is stored.
            Some(user) => vec![WritePin::field("author_id", user.id)],
            None => vec![WritePin::Deny],
        }
    }
}
