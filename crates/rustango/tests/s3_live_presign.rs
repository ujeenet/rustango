//! Live presigned URL test against MinIO. Exercises:
//! - presigned PUT  (browser-style upload — bound to Content-Type)
//! - presigned GET  (private download link)
//! - rejection paths (wrong content-type, expired URL)
//!
//! Skipped silently when RUSTANGO_S3_TEST_KEY etc. aren't set.

use rustango::storage::s3::{S3Config, S3Storage};
use rustango::storage::Storage;
use std::time::Duration;

fn maybe_storage() -> Option<S3Storage> {
    let key = std::env::var("RUSTANGO_S3_TEST_KEY").ok()?;
    let secret = std::env::var("RUSTANGO_S3_TEST_SECRET").ok()?;
    let bucket = std::env::var("RUSTANGO_S3_TEST_BUCKET").ok()?;
    let endpoint = std::env::var("RUSTANGO_S3_TEST_ENDPOINT").ok();
    let region =
        std::env::var("RUSTANGO_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());
    Some(S3Storage::new(S3Config {
        bucket,
        region,
        endpoint: endpoint.clone(),
        access_key_id: key,
        secret_access_key: secret,
        path_style: endpoint.is_some(),
    }))
}

#[tokio::test]
async fn presigned_put_then_get_round_trip() {
    let Some(storage) = maybe_storage() else {
        eprintln!("skipping — set RUSTANGO_S3_TEST_KEY etc.");
        return;
    };
    let key = format!("presign-test/{}.png", uuid::Uuid::new_v4());
    let payload = b"\x89PNG\r\n\x1a\n-fake-png-bytes-";

    // 1. Server-side: generate a presigned PUT URL for the browser.
    let put_url = storage
        .presigned_put_url(&key, Duration::from_secs(60), Some("image/png"))
        .await
        .expect("PUT url");
    println!("[presign] PUT URL: {put_url}");

    // 2. Browser-side: upload directly with the matching Content-Type.
    let client = reqwest::Client::new();
    let resp = client
        .put(&put_url)
        .header("Content-Type", "image/png")
        .body(payload.to_vec())
        .send()
        .await
        .expect("PUT request");
    assert!(
        resp.status().is_success(),
        "PUT failed: {} — {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // 3. Confirm the object actually landed by fetching via the
    //    storage trait directly (no presign).
    let bytes = storage.load(&key).await.expect("load after PUT");
    assert_eq!(&bytes, payload, "content round-tripped");

    // 4. Generate a presigned GET URL and fetch it as a "browser".
    let get_url = storage
        .presigned_get_url(&key, Duration::from_secs(60))
        .await
        .expect("GET url");
    println!("[presign] GET URL: {get_url}");

    let resp = client.get(&get_url).send().await.expect("GET request");
    assert!(resp.status().is_success(), "GET failed: {}", resp.status());
    let bytes = resp.bytes().await.expect("body");
    assert_eq!(&bytes[..], payload, "content from presigned GET");

    // 5. Cleanup.
    storage.delete(&key).await.expect("delete");
}

#[tokio::test]
async fn presigned_put_rejects_wrong_content_type() {
    let Some(storage) = maybe_storage() else {
        eprintln!("skipping — set RUSTANGO_S3_TEST_KEY etc.");
        return;
    };
    let key = format!("presign-reject/{}.bin", uuid::Uuid::new_v4());
    // Sign for image/png — but try to upload as text/plain.
    let put_url = storage
        .presigned_put_url(&key, Duration::from_secs(60), Some("image/png"))
        .await
        .expect("PUT url");
    let client = reqwest::Client::new();
    let resp = client
        .put(&put_url)
        .header("Content-Type", "text/plain")
        .body(b"oops".to_vec())
        .send()
        .await
        .expect("PUT request");
    // S3 (and MinIO) reject because the signed Content-Type is part
    // of the canonical request — mismatch -> SignatureDoesNotMatch.
    assert!(
        !resp.status().is_success(),
        "expected reject for content-type mismatch; got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap_or_default();
    println!("[presign] expected reject: {body}");
    assert!(
        body.contains("SignatureDoesNotMatch") || body.contains("signature"),
        "expected SignatureDoesNotMatch; got: {body}"
    );
}

#[tokio::test]
async fn presigned_get_rejects_after_ttl_expires() {
    let Some(storage) = maybe_storage() else {
        eprintln!("skipping — set RUSTANGO_S3_TEST_KEY etc.");
        return;
    };
    let key = format!("presign-expire/{}.txt", uuid::Uuid::new_v4());
    storage.save(&key, b"x").await.expect("save");

    // 1-second TTL — wait 3 s and the URL must reject.
    let url = storage
        .presigned_get_url(&key, Duration::from_secs(1))
        .await
        .expect("GET url");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let resp = reqwest::get(&url).await.expect("GET");
    assert!(
        !resp.status().is_success(),
        "URL should be rejected after TTL; got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap_or_default();
    println!("[presign] expired body: {body}");
    assert!(
        body.contains("Expired") || body.contains("expired"),
        "expected expiry rejection; got: {body}"
    );

    // Cleanup.
    storage.delete(&key).await.ok();
}
