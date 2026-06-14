//! HTTP routes for the accounts app.
//!
//! * POST /accounts/register — create user, return public profile
//! * POST /accounts/login    — verify password, mint HS256 JWT
//! * GET  /accounts/me       — Bearer-token-authenticated user lookup
//!
//! Exercises `rustango::passwords::{hash, verify}` + `rustango::jwt::{encode, decode}`
//! end-to-end. The JWT secret is read from `SHOWCASE_JWT_SECRET` so
//! the playwright suite can pre-set it; falls back to a fixed
//! per-process random secret if unset (rejects any token after restart).

use std::time::Duration;

use axum::extract::Extension;
use axum::http::{header, StatusCode};
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use rustango::core::Op;
use rustango::jwt::{decode, encode, Claims};
use rustango::passwords;
use rustango::sql::{Auto, FetcherPool as _, Pool};

use super::models::User;

#[cfg(feature = "postgres")]
type AttachedPool = sqlx::PgPool;
#[cfg(not(feature = "postgres"))]
type AttachedPool = Pool;

fn into_pool(p: &AttachedPool) -> Pool {
    #[cfg(feature = "postgres")]
    {
        Pool::from(p.clone())
    }
    #[cfg(not(feature = "postgres"))]
    {
        p.clone()
    }
}

#[must_use]
pub fn api() -> Router {
    Router::new()
        .route("/accounts/register", post(register))
        .route("/accounts/login", post(login))
        .route("/accounts/me", get(me))
}

fn jwt_secret() -> Vec<u8> {
    std::env::var("SHOWCASE_JWT_SECRET")
        .unwrap_or_else(|_| "showcase-dev-only-jwt-secret-not-for-production".to_owned())
        .into_bytes()
}

#[derive(serde::Deserialize)]
struct RegisterIn {
    username: String,
    email: String,
    password: String,
}

#[derive(serde::Serialize)]
struct UserOut {
    id: i64,
    username: String,
    email: String,
}

impl From<&User> for UserOut {
    fn from(u: &User) -> Self {
        Self {
            id: match u.id {
                Auto::Set(n) => n,
                Auto::Unset => 0,
            },
            username: u.username.clone(),
            email: u.email.clone(),
        }
    }
}

async fn register(
    Extension(pool): Extension<AttachedPool>,
    Json(input): Json<RegisterIn>,
) -> Result<(StatusCode, Json<UserOut>), (StatusCode, String)> {
    let pool = into_pool(&pool);

    if input.password.len() < 8 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Password must be at least 8 characters.".into(),
        ));
    }
    let password_hash = passwords::hash(&input.password)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut u = User {
        id: Auto::Unset,
        username: input.username,
        email: input.email,
        password_hash,
        created_at: Auto::Unset,
    };
    u.insert_pool(&pool)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;

    let id = match u.id {
        Auto::Set(n) => n,
        Auto::Unset => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "insert_pool didn't populate PK".into(),
            ));
        }
    };
    let mut rows: Vec<User> = User::objects()
        .filter_op("id", Op::Eq, id)
        .fetch(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stored = rows.pop().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "could not re-fetch user".into(),
    ))?;
    Ok((StatusCode::CREATED, Json(UserOut::from(&stored))))
}

#[derive(serde::Deserialize)]
struct LoginIn {
    username: String,
    password: String,
}

#[derive(serde::Serialize)]
struct LoginOut {
    token: String,
    user: UserOut,
}

async fn login(
    Extension(pool): Extension<AttachedPool>,
    Json(input): Json<LoginIn>,
) -> Result<Json<LoginOut>, (StatusCode, String)> {
    let pool = into_pool(&pool);
    let mut rows: Vec<User> = User::objects()
        .filter_op("username", Op::Eq, input.username.clone())
        .fetch(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let user = rows
        .pop()
        .ok_or((StatusCode::UNAUTHORIZED, "invalid credentials".into()))?;

    let ok = passwords::verify(&input.password, &user.password_hash)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !ok {
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials".into()));
    }

    let id = match user.id {
        Auto::Set(n) => n,
        Auto::Unset => {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, "user has no PK".into()));
        }
    };
    let claims = Claims::new(id.to_string())
        .issuer("rustango-showcase")
        .ttl(Duration::from_secs(3600));
    let token = encode(&claims, &jwt_secret())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(LoginOut {
        token,
        user: UserOut::from(&user),
    }))
}

async fn me(
    Extension(pool): Extension<AttachedPool>,
    headers: axum::http::HeaderMap,
) -> Result<Json<UserOut>, (StatusCode, String)> {
    let pool = into_pool(&pool);

    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "missing or malformed Authorization header".into(),
        ))?;

    let claims =
        decode(token, &jwt_secret()).map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;
    let id: i64 = claims
        .subject()
        .and_then(|s| s.parse().ok())
        .ok_or((StatusCode::UNAUTHORIZED, "no sub claim".into()))?;

    let mut rows: Vec<User> = User::objects()
        .filter_op("id", Op::Eq, id)
        .fetch(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let user = rows
        .pop()
        .ok_or((StatusCode::UNAUTHORIZED, "user no longer exists".into()))?;
    Ok(Json(UserOut::from(&user)))
}
