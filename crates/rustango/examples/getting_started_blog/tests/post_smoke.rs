use getting_started_blog::urls;
use rustango::sql::Pool;
use rustango::test_client::TestClient;
use serde_json::json;

async fn app() -> axum::Router {
    // An integration test is its own crate and never runs `main`, so
    // nothing has loaded `.env` for it. Without this, `DATABASE_URL` is
    // unset and every test here panics before reaching the database.
    // CI exports it instead, which is why this gate stayed green while
    // the same code failed on a contributor's machine (#1299).
    let _ = dotenvy::dotenv();

    let pool = Pool::connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    // The same `/api` routes `main.rs` mounts: posts plus register/login.
    axum::Router::new().nest("/api", urls::rest_api(pool))
}

#[tokio::test]
async fn list_posts_returns_200() {
    let client = TestClient::new(app().await);
    let response = client.get("/api/posts").send().await;
    assert_eq!(response.status, 200);
    let v = response.json_value();
    assert!(v["results"].is_array());
}

#[tokio::test]
async fn create_post_without_a_token_returns_401() {
    let client = TestClient::new(app().await);
    let response = client
        .post("/api/posts")
        // The serializer's field names: `content` is `Post.body` renamed.
        .json(&json!({ "title": "Test", "content": "x" }))
        .send()
        .await;
    assert_eq!(response.status, 401);
}
