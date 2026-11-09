//! Backing test for `docs/getting-started.md` — Step 14: register, log in,
//! and write posts with a Bearer token.
//!
//! Uses `DATABASE_URL` (migrated) when set. Without it, a SQLite build
//! makes a throwaway database file; a Postgres build skips.
//!
//! Run: `cargo test --no-default-features --features sqlite --test auth_flow`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

use getting_started_blog::urls;
use rustango::jwt::{self, Claims};
use rustango::sql::Pool;
use rustango::test_client::TestClient;
use serde_json::{json, Value};

const SECRET: &str = "test-only-jwt-secret-at-least-32-bytes-long";
const PASSWORD: &str = "correct-Horse-battery-9";

static ENV: Once = Once::new();

async fn pool() -> Option<Pool> {
    ENV.call_once(|| {
        // Ours wins over a developer's `.env`, so the tests can forge tokens.
        std::env::set_var("JWT_SECRET", SECRET);
        let _ = dotenvy::dotenv();
    });
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return Some(Pool::connect(&url).await.expect("connect DATABASE_URL"));
    }
    #[cfg(feature = "sqlite")]
    {
        let path = std::env::temp_dir().join(format!("gsb_auth_{}.db", unique("db")));
        let pool = Pool::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("connect sqlite");
        rustango::migrate::apply_all_pool(&pool).await.expect("create tables");
        Some(pool)
    }
    #[cfg(not(feature = "sqlite"))]
    {
        eprintln!("skipped: set DATABASE_URL, or run with `--features sqlite`");
        None
    }
}

/// The `/api` routes exactly as `main.rs` mounts them.
async fn client() -> Option<TestClient> {
    let pool = pool().await?;
    Some(TestClient::new(
        axum::Router::new().nest("/api", urls::rest_api(pool)),
    ))
}

/// A username no other test or earlier run has used.
fn unique(prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}_{nanos}_{}", N.fetch_add(1, Ordering::Relaxed))
}

async fn register(c: &TestClient, username: &str) -> Value {
    let r = c
        .post("/api/register")
        .json(&json!({ "username": username, "password": PASSWORD }))
        .send()
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    r.json_value()
}

async fn login(c: &TestClient, username: &str, password: &str) -> rustango::test_client::TestResponse {
    c.post("/api/login")
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
}

/// Register + log in; the user's id and a token.
async fn signed_in(c: &TestClient) -> (i64, String) {
    let username = unique("writer");
    let id = register(c, &username).await["id"].as_i64().unwrap();
    let r = login(c, &username, PASSWORD).await;
    assert_eq!(r.status, 200, "{}", r.text());
    (id, r.json_value()["access_token"].as_str().unwrap().to_owned())
}

async fn create_post(c: &TestClient, token: &str, body: Value) -> rustango::test_client::TestResponse {
    c.post("/api/posts")
        .header("authorization", &format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
}

/// The Step 13 ViewSet, before Step 14 turns writes on.
#[derive(rustango::ViewSet)]
#[viewset(
    model = getting_started_blog::blog::models::Post,
    fields = "id, title, body, author_id, published_at",
    serializer = getting_started_blog::post_serializer::PostSerializer,
    ordering = "-published_at",
    read_only,
)]
struct Step13PostViewSet;

#[tokio::test]
async fn before_step_14_the_api_is_read_only() {
    let Some(pool) = pool().await else { return };
    let c = TestClient::new(Step13PostViewSet::router("/api/posts", pool));
    assert_eq!(c.get("/api/posts").send().await.status, 200);
    let r = c.post("/api/posts").json(&json!({ "title": "t", "content": "x" })).send().await;
    assert_eq!(r.status, 405, "{}", r.text());
}

#[tokio::test]
async fn register_creates_an_account() {
    let Some(c) = client().await else { return };
    let username = unique("Alice");
    let v = register(&c, &username).await;
    assert!(v["id"].as_i64().is_some());
    assert_eq!(v["username"], username.to_lowercase());
    assert!(v.get("password_hash").is_none(), "the hash never leaves the server");
}

#[tokio::test]
async fn a_taken_username_is_409_whatever_its_case() {
    let Some(c) = client().await else { return };
    let username = unique("bob");
    register(&c, &username).await;
    let r = c
        .post("/api/register")
        .json(&json!({ "username": username.to_uppercase(), "password": PASSWORD }))
        .send()
        .await;
    assert_eq!(r.status, 409, "{}", r.text());
}

#[tokio::test]
async fn a_weak_password_is_refused() {
    let Some(c) = client().await else { return };
    let r = c
        .post("/api/register")
        .json(&json!({ "username": unique("carol"), "password": "password123" }))
        .send()
        .await;
    assert_eq!(r.status, 422, "{}", r.text());
}

#[tokio::test]
async fn login_returns_a_short_lived_token_for_the_user() {
    let Some(c) = client().await else { return };
    let username = unique("dave");
    let id = register(&c, &username).await["id"].as_i64().unwrap();
    let r = login(&c, &username, PASSWORD).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json_value();
    assert_eq!(v["token_type"], "Bearer");
    assert_eq!(v["expires_in"], 900);
    let claims = jwt::decode(v["access_token"].as_str().unwrap(), SECRET.as_bytes()).unwrap();
    assert_eq!(claims.subject(), Some(id.to_string().as_str()));
}

#[tokio::test]
async fn wrong_password_and_unknown_user_get_the_same_401() {
    let Some(c) = client().await else { return };
    let username = unique("erin");
    register(&c, &username).await;

    let wrong_password = login(&c, &username, "not-the-Password-1").await;
    let unknown_user = login(&c, &unique("nobody"), PASSWORD).await;
    assert_eq!(wrong_password.status, 401);
    assert_eq!(unknown_user.status, 401);
    // The same body, so the answer does not reveal which names exist.
    assert_eq!(wrong_password.text(), unknown_user.text());
}

#[tokio::test]
async fn repeated_failed_logins_from_one_ip_are_429() {
    let Some(c) = client().await else { return };
    // An address of its own, so other tests' logins do not count here.
    let ip: std::net::SocketAddr = "192.0.2.77:4000".parse().unwrap();
    let limit = rustango::login_throttle::DEFAULT_IP_LIMIT as usize;
    let mut statuses = Vec::new();
    // The bucket refills slowly while we try, so allow a few extra attempts.
    while statuses.len() < 2 * limit && statuses.last() != Some(&429) {
        let r = c
            .post("/api/login")
            .remote_addr(ip)
            .json(&json!({ "username": unique("guess"), "password": "Wrong-guess-123" }))
            .send()
            .await;
        statuses.push(r.status);
    }
    assert!(statuses[..limit].iter().all(|s| *s == 401), "{statuses:?}");
    assert_eq!(statuses.last(), Some(&429), "{statuses:?}");
}

#[tokio::test]
async fn reads_are_public_and_writes_without_a_token_are_401() {
    let Some(c) = client().await else { return };
    assert_eq!(c.get("/api/posts").send().await.status, 200);
    let r = c
        .post("/api/posts")
        .json(&json!({ "title": "No token", "content": "x", "author_id": 1 }))
        .send()
        .await;
    assert_eq!(r.status, 401, "{}", r.text());
}

#[tokio::test]
async fn a_tampered_token_is_401() {
    let Some(c) = client().await else { return };
    let (_, token) = signed_in(&c).await;
    let (victim, _) = signed_in(&c).await;
    // Swap in another real user's id; the signature no longer matches.
    let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
    let forged = Claims::new(victim.to_string()).ttl(std::time::Duration::from_secs(900));
    let other = jwt::encode(&forged, b"someone-elses-secret-32-bytes-long!!").unwrap();
    parts[1] = other.split('.').nth(1).unwrap().to_owned();
    let r = create_post(&c, &parts.join("."), json!({ "title": "t", "content": "x" })).await;
    assert_eq!(r.status, 401, "{}", r.text());
}

#[tokio::test]
async fn an_expired_token_is_401() {
    let Some(c) = client().await else { return };
    let (id, _) = signed_in(&c).await;
    let expired = jwt::encode(&Claims::new(id.to_string()).expires_at(1), SECRET.as_bytes()).unwrap();
    let r = create_post(&c, &expired, json!({ "title": "t", "content": "x" })).await;
    assert_eq!(r.status, 401, "{}", r.text());
}

#[tokio::test]
async fn a_token_for_a_missing_user_is_401() {
    let Some(c) = client().await else { return };
    let ghost = Claims::new(i64::MAX.to_string()).ttl(std::time::Duration::from_secs(900));
    let token = jwt::encode(&ghost, SECRET.as_bytes()).unwrap();
    let r = create_post(&c, &token, json!({ "title": "t", "content": "x" })).await;
    assert_eq!(r.status, 401, "{}", r.text());
}

#[tokio::test]
async fn the_author_comes_from_the_token_not_the_body() {
    let Some(c) = client().await else { return };
    let (id, token) = signed_in(&c).await;
    let r = create_post(
        &c,
        &token,
        json!({ "title": "Mine", "content": "x", "author_id": id + 1000 }),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let post = r.json_value();
    assert_eq!(post["author_id"], id);

    // Reading it back needs no token and shows the same author.
    let r = c.get(&format!("/api/posts/{}", post["id"])).send().await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json_value()["author_id"], id);
}

#[tokio::test]
async fn only_the_author_can_change_or_delete_a_post() {
    let Some(c) = client().await else { return };
    let (author, author_token) = signed_in(&c).await;
    let (_, other_token) = signed_in(&c).await;
    let post = create_post(&c, &author_token, json!({ "title": "Mine", "content": "x" }))
        .await
        .json_value();
    let url = format!("/api/posts/{}", post["id"]);

    // Someone else's post looks missing to them.
    let r = c
        .patch(&url)
        .header("authorization", &format!("Bearer {other_token}"))
        .json(&json!({ "title": "Hijacked" }))
        .send()
        .await;
    assert_eq!(r.status, 404, "{}", r.text());
    let r = c
        .delete(&url)
        .header("authorization", &format!("Bearer {other_token}"))
        .send()
        .await;
    assert_eq!(r.status, 404, "{}", r.text());

    // The author can edit it, but cannot hand it to someone else.
    let r = c
        .patch(&url)
        .header("authorization", &format!("Bearer {author_token}"))
        .json(&json!({ "title": "Edited", "author_id": author + 1000 }))
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json_value()["title"], "Edited");
    assert_eq!(r.json_value()["author_id"], author);
}
