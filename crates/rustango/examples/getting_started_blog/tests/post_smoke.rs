use getting_started_blog::post_view_set::PostViewSet;
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
    // `router` takes any backend's pool (#1273), so nothing here names
    // a driver and this file compiles on all three.
    PostViewSet::router("/api/posts", pool)
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
async fn create_post_returns_the_new_object() {
    let client = TestClient::new(app().await);
    let response = client
        .post("/api/posts")
        // These are the serializer's field names, not the model's:
        // `content` is `Post.body` renamed, and `status` is absent from
        // the serializer so posting it would be ignored.
        .json(&json!({
            "title": "Test",
            "content": "x",
            "author_id": 1,
        }))
        .send()
        .await;
    assert_eq!(response.status, 201);
    let v: serde_json::Value = response.json();
    assert_eq!(v["title"], "Test");
    assert_eq!(v["content"], "x");
}
