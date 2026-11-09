//! Step 14 with `JWT_SECRET` unset: every login is the same 500, so the
//! answer does not tell a right password from a wrong one.
//!
//! Its own test binary, because the other suites set the secret for the process.

use getting_started_blog::urls;
use rustango::sql::Pool;
use rustango::test_client::TestClient;
use serde_json::json;

const PASSWORD: &str = "correct-Horse-battery-9";

async fn client() -> Option<TestClient> {
    // No `.env` here: a developer's `JWT_SECRET` would hide the case under test.
    std::env::remove_var("JWT_SECRET");
    let pool = if let Ok(url) = std::env::var("DATABASE_URL") {
        Pool::connect(&url).await.expect("connect DATABASE_URL")
    } else {
        #[cfg(feature = "sqlite")]
        {
            let pool = Pool::connect("sqlite::memory:").await.expect("connect sqlite");
            rustango::migrate::apply_all_pool(&pool).await.expect("create tables");
            pool
        }
        #[cfg(not(feature = "sqlite"))]
        {
            eprintln!("skipped: set DATABASE_URL, or run with `--features sqlite`");
            return None;
        }
    };
    Some(TestClient::new(axum::Router::new().nest("/api", urls::rest_api(pool))))
}

#[tokio::test]
async fn without_a_secret_right_and_wrong_passwords_get_the_same_answer() {
    let Some(c) = client().await else { return };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let username = format!("nosecret_{nanos}");
    let r = c
        .post("/api/register")
        .json(&json!({ "username": username, "password": PASSWORD }))
        .send()
        .await;
    assert_eq!(r.status, 201, "{}", r.text());

    let login = |password: &'static str| {
        c.post("/api/login")
            .json(&json!({ "username": username, "password": password }))
            .send()
    };
    let right = login(PASSWORD).await;
    let wrong = login("not-the-Password-1").await;
    assert_eq!(right.status, 500, "{}", right.text());
    assert_eq!(right.status, wrong.status);
    assert_eq!(right.text(), wrong.text());
}
