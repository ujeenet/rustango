//! `#[ignore]`-d helper that creates visible Media rows + S3
//! objects via `MediaManager`, leaves everything behind for
//! inspection.
//!
//! ```text
//! env DATABASE_URL=postgres://rustango:rustango@127.0.0.1:5532/rustango_test \
//!     RUSTANGO_S3_TEST_KEY=rustango \
//!     RUSTANGO_S3_TEST_SECRET=rustango-test-secret \
//!     RUSTANGO_S3_TEST_BUCKET=rustango-test \
//!     RUSTANGO_S3_TEST_ENDPOINT=http://127.0.0.1:9100 \
//!     cargo test -p rustango --test media_live_seed -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use rustango::media::{Media, MediaManager, SaveOpts, UploadIntent};
use rustango::storage::s3::{S3Config, S3Storage};
use rustango::storage::{BoxedStorage, StorageRegistry};
use sqlx::PgPool;

#[ignore = "seed test that LEAVES MEDIA ROWS + S3 OBJECTS BEHIND"]
#[tokio::test]
async fn seed_media_rows_for_inspection() {
    let url = std::env::var("DATABASE_URL").expect("set DATABASE_URL");
    let key = std::env::var("RUSTANGO_S3_TEST_KEY").expect("set RUSTANGO_S3_TEST_KEY");
    let secret =
        std::env::var("RUSTANGO_S3_TEST_SECRET").expect("set RUSTANGO_S3_TEST_SECRET");
    let bucket =
        std::env::var("RUSTANGO_S3_TEST_BUCKET").expect("set RUSTANGO_S3_TEST_BUCKET");
    let endpoint = std::env::var("RUSTANGO_S3_TEST_ENDPOINT").ok();
    let region =
        std::env::var("RUSTANGO_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());

    let pool = PgPool::connect(&url).await.expect("connect");
    Media::ensure_table(&pool).await.expect("ensure_table");

    let storage: BoxedStorage = Arc::new(S3Storage::new(S3Config {
        bucket,
        region,
        endpoint: endpoint.clone(),
        access_key_id: key,
        secret_access_key: secret,
        path_style: endpoint.is_some(),
    }));
    let registry = StorageRegistry::new()
        .set("avatars", storage.clone())
        .cdn("avatars", "https://cdn.example.com/avatars")
        .with_default("avatars");
    let manager = MediaManager::new(pool, registry);

    println!("\n=== seeding Media rows + S3 objects ===\n");

    // Two server-side uploads.
    for (name, body, mime) in [
        ("alice.png", &b"\x89PNG\r\n\x1a\n-alice-png-bytes-"[..], "image/png"),
        ("bob.jpg", &b"\xff\xd8\xff\xe0-jpeg-bob-bytes-"[..], "image/jpeg"),
    ] {
        let m = manager
            .save_bytes(SaveOpts {
                disk: "avatars".into(),
                key_prefix: "media-seed/users".into(),
                bytes: body.to_vec(),
                mime: mime.into(),
                original_filename: name.into(),
                uploaded_by_id: Some(if name == "alice.png" { 1 } else { 2 }),
                collection_id: None,
                metadata: serde_json::json!({"alt": format!("avatar for {name}")}),
            })
            .await
            .expect("save");
        let id = match m.id {
            rustango::sql::Auto::Set(v) => v,
            _ => unreachable!(),
        };
        println!(
            "  Media #{id:>3}  status={status:8}  disk={disk:8}  size={size:>4}B  cdn_url={cdn}",
            id = id,
            status = m.status,
            disk = m.disk,
            size = m.size_bytes,
            cdn = manager.url(&m).unwrap_or_else(|| "(no url)".into()),
        );
    }

    // One direct-browser-upload demo (begin -> presigned URL ->
    // browser PUT -> finalize).
    let intent = UploadIntent {
        disk: "avatars".into(),
        key_prefix: "media-seed/direct".into(),
        mime: "text/plain".into(),
        original_filename: "manifesto.txt".into(),
        size_bytes: 27,
        uploaded_by_id: Some(3),
        collection_id: None,
        ttl: Duration::from_secs(60),
    };
    let ticket = manager.begin_upload(intent).await.expect("begin");
    println!(
        "\n  begin_upload -> media #{} pending @ {}",
        ticket.media_id, ticket.upload_url
    );
    let resp = reqwest::Client::new()
        .put(&ticket.upload_url)
        .header("Content-Type", "text/plain")
        .body("rustango media is first-class.".as_bytes().to_vec())
        .send()
        .await
        .expect("PUT");
    assert!(resp.status().is_success(), "PUT failed: {}", resp.status());
    let finalized = manager
        .finalize_upload(ticket.media_id)
        .await
        .expect("finalize");
    println!(
        "  finalize_upload -> media #{} {} ({}B)",
        ticket.media_id, finalized.status, finalized.size_bytes
    );

    println!("\n=== leaving 3 Media rows + 3 S3 objects behind ===");
    println!("MinIO console:    http://127.0.0.1:9101");
    println!("Bucket:           rustango-test");
    println!("Login:            rustango / rustango-test-secret");
    println!();
    println!("Postgres rows:");
    println!("  psql {url} \\");
    println!("    -c \"SELECT id, disk, storage_key, mime, size_bytes, status FROM rustango_media;\"");
}
