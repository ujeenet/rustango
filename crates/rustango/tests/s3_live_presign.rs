#![cfg(all(feature = "postgres", feature = "storage-s3"))]
//! Live presigned URL test against MinIO. Exercises:
//! - presigned PUT  (browser-style upload — bound to Content-Type)
//! - presigned GET  (private download link)
//! - rejection paths (wrong content-type, expired URL)
//!
//! Skipped silently when RUSTANGO_S3_TEST_KEY etc. aren't set.

use rustango::storage::s3::{S3Config, S3Credentials, S3Storage};
use rustango::storage::{PutConditions, Storage};
use std::time::Duration;

/// `RUSTANGO_S3_TEST_SESSION_TOKEN` is set when the keys are STS ones.
fn storage_with(token: Option<String>) -> Option<S3Storage> {
    let key = std::env::var("RUSTANGO_S3_TEST_KEY").ok()?;
    let secret = std::env::var("RUSTANGO_S3_TEST_SECRET").ok()?;
    let bucket = std::env::var("RUSTANGO_S3_TEST_BUCKET").ok()?;
    let endpoint = std::env::var("RUSTANGO_S3_TEST_ENDPOINT").ok();
    let region = std::env::var("RUSTANGO_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());
    let mut cfg = S3Config::new(bucket, region).with_path_style(endpoint.is_some());
    cfg.endpoint = endpoint;
    let mut creds = S3Credentials::new(key, secret);
    if let Some(token) = token {
        creds = creds.with_session_token(token);
    }
    Some(S3Storage::new(cfg, creds))
}

fn maybe_storage() -> Option<S3Storage> {
    storage_with(std::env::var("RUSTANGO_S3_TEST_SESSION_TOKEN").ok())
}

/// With STS keys, the token is what makes requests pass, so the
/// tests above prove it is sent and signed.
#[tokio::test]
async fn temporary_keys_need_their_session_token() {
    let (Ok(_), Some(storage), Some(no_token)) = (
        std::env::var("RUSTANGO_S3_TEST_SESSION_TOKEN"),
        maybe_storage(),
        storage_with(None),
    ) else {
        eprintln!("skipping — set RUSTANGO_S3_TEST_SESSION_TOKEN etc.");
        return;
    };
    let key = format!("sts-test/{}.txt", uuid::Uuid::new_v4());
    storage.save(&key, b"x").await.expect("save with the token");
    let url = storage
        .presigned_get_url(&key, Duration::from_secs(60))
        .await
        .expect("GET url");
    assert!(reqwest::get(&url).await.unwrap().status().is_success());

    assert!(
        no_token.load(&key).await.is_err(),
        "loaded without the token"
    );
    let url = no_token
        .presigned_get_url(&key, Duration::from_secs(60))
        .await
        .expect("GET url");
    assert!(!reqwest::get(&url).await.unwrap().status().is_success());
    storage.delete(&key).await.expect("delete");
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
        .presigned_put_url(
            &key,
            Duration::from_secs(60),
            &PutConditions::new().content_type("image/png"),
        )
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
        .presigned_put_url(
            &key,
            Duration::from_secs(60),
            &PutConditions::new().content_type("image/png"),
        )
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

/// #1851: a signed length caps the PUT, and `metadata` reports the
/// object's real size and type.
#[tokio::test]
async fn presigned_put_refuses_another_size_and_metadata_reports_the_object() {
    let Some(storage) = maybe_storage() else {
        eprintln!("skipping — set RUSTANGO_S3_TEST_KEY etc.");
        return;
    };
    let key = format!("presign-size/{}.png", uuid::Uuid::new_v4());
    let client = reqwest::Client::new();
    let put = |body: &'static [u8]| {
        let (storage, key, client) = (&storage, &key, &client);
        async move {
            let url = storage
                .presigned_put_url(
                    key,
                    Duration::from_secs(60),
                    &PutConditions::new()
                        .content_type("image/png")
                        .content_length(10),
                )
                .await
                .expect("PUT url");
            client
                .put(&url)
                .header("Content-Type", "image/png")
                .body(body.to_vec())
                .send()
                .await
                .expect("PUT request")
                .status()
        }
    };
    assert!(
        !put(b"twenty-bytes-of-body").await.is_success(),
        "oversized PUT accepted"
    );
    assert_eq!(storage.metadata(&key).await.expect("head"), None);
    assert!(put(b"ten-bytes!").await.is_success());
    let meta = storage.metadata(&key).await.expect("head").expect("object");
    assert_eq!(meta.size, 10);
    assert_eq!(meta.content_type.as_deref(), Some("image/png"));
    storage.delete(&key).await.expect("delete");
}

/// A create-only URL lands once; a replay gets 412 and the object stays.
#[tokio::test]
async fn presigned_put_create_only_refuses_a_replay() {
    let Some(storage) = maybe_storage() else {
        eprintln!("skipping — set RUSTANGO_S3_TEST_KEY etc.");
        return;
    };
    let key = format!("presign-once/{}.bin", uuid::Uuid::new_v4());
    let put = PutConditions::new().create_only();
    let url = storage
        .presigned_put_url(&key, Duration::from_secs(60), &put)
        .await
        .expect("PUT url");
    let send = |body: &'static [u8]| {
        let mut req = reqwest::Client::new().put(&url).body(body);
        for (k, v) in put.headers() {
            req = req.header(k, v);
        }
        req.send()
    };
    let bare = reqwest::Client::new().put(&url).body(&b"bare"[..]).send();
    assert!(
        !bare.await.expect("PUT").status().is_success(),
        "PUT without If-None-Match accepted"
    );
    assert!(send(b"first").await.expect("PUT").status().is_success());
    assert_eq!(send(b"second").await.expect("PUT").status().as_u16(), 412);
    assert_eq!(storage.load(&key).await.expect("load"), b"first");
    storage.delete(&key).await.expect("delete");
}

async fn read_all(s: rustango::storage::ByteStream) -> Vec<u8> {
    use http_body_util::BodyExt as _;
    let body = axum::body::Body::from_stream(s);
    body.collect().await.expect("read").to_bytes().to_vec()
}

/// `save_stream` over plain http chunk-signs the body: the server checks
/// each chunk, create-only holds, and a cut-short body stores nothing.
#[tokio::test]
async fn save_stream_round_trip_create_only_and_cut_short() {
    use rustango::storage::ByteStream;
    let Some(storage) = maybe_storage() else {
        eprintln!("skipping — set RUSTANGO_S3_TEST_KEY etc.");
        return;
    };
    let key = format!("stream-test/{}.bin", uuid::Uuid::new_v4());
    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let len = data.len() as u64;
    let once = PutConditions::new()
        .content_type("application/octet-stream")
        .create_only();
    storage
        .save_stream(&key, len, ByteStream::from(data.clone()), &once)
        .await
        .expect("save_stream");
    let err = storage
        .save_stream(&key, 3, ByteStream::from(b"new".to_vec()), &once)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    let meta = storage.metadata(&key).await.expect("head").expect("object");
    assert_eq!(meta.size, len);
    assert_eq!(
        meta.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(
        read_all(storage.load_stream(&key).await.expect("load_stream")).await,
        data
    );

    let cut = format!("stream-test/{}.cut", uuid::Uuid::new_v4());
    let short = ByteStream::from(data[..1 << 20].to_vec());
    assert!(storage
        .save_stream(&cut, len, short, &PutConditions::new())
        .await
        .is_err());
    assert_eq!(storage.metadata(&cut).await.expect("head"), None);
    storage.delete(&key).await.expect("delete");
}
