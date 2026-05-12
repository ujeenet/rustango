#![cfg(feature = "storage-s3")]
//! `#[ignore]`-d helper that uploads a few visible files to the
//! configured S3-compatible bucket via `S3Storage`, leaves them
//! behind, and prints the URLs so you can see them in the MinIO /
//! S3 web console.
//!
//! Run with:
//! ```text
//! env RUSTANGO_S3_TEST_KEY=… RUSTANGO_S3_TEST_SECRET=… RUSTANGO_S3_TEST_BUCKET=… \
//!     RUSTANGO_S3_TEST_ENDPOINT=http://127.0.0.1:9100 \
//!     cargo test -p rustango --test s3_live_seed -- --ignored --nocapture
//! ```
//!
//! The other live tests (`live_round_trip`, `presigned_*`) clean up
//! after themselves, so the console looks empty after they finish.
//! This one stays put.

use rustango::storage::s3::{S3Config, S3Storage};
use rustango::storage::Storage;
use std::time::Duration;

fn maybe_storage() -> Option<S3Storage> {
    let key = std::env::var("RUSTANGO_S3_TEST_KEY").ok()?;
    let secret = std::env::var("RUSTANGO_S3_TEST_SECRET").ok()?;
    let bucket = std::env::var("RUSTANGO_S3_TEST_BUCKET").ok()?;
    let endpoint = std::env::var("RUSTANGO_S3_TEST_ENDPOINT").ok();
    let region = std::env::var("RUSTANGO_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());
    Some(S3Storage::new(S3Config {
        bucket,
        region,
        endpoint: endpoint.clone(),
        access_key_id: key,
        secret_access_key: secret,
        path_style: endpoint.is_some(),
    }))
}

#[ignore = "live test that LEAVES FILES BEHIND for the console — run with --ignored"]
#[tokio::test]
async fn upload_visible_seed_files() {
    let Some(storage) = maybe_storage() else {
        panic!("set RUSTANGO_S3_TEST_KEY / _SECRET / _BUCKET to run this");
    };

    // Three files at different paths so the console shows folder
    // navigation working.
    let files: &[(&str, &[u8], &str)] = &[
        (
            "seed/hello.txt",
            b"hello from rustango S3Storage",
            "text/plain",
        ),
        (
            "seed/avatars/alice.png",
            b"\x89PNG\r\n\x1a\n-fake-image-data",
            "image/png",
        ),
        (
            "seed/docs/2026/launch.md",
            b"# Launch Notes\n\nFrom rustango.",
            "text/markdown",
        ),
    ];

    println!("\n=== uploading {} files to MinIO ===", files.len());
    for (key, body, ct) in files {
        // 1. Direct save via the trait.
        storage.save(key, body).await.expect("save");
        // 2. Print the public URL so you can paste it in the console
        //    or compare against what the console shows.
        let url = storage.url(key).expect("url");
        println!("  saved {key:50} -> {url}  ({} bytes, {ct})", body.len());

        // Bonus: presigned GET URL so you can paste in a browser.
        let presigned = storage
            .presigned_get_url(key, Duration::from_secs(3600))
            .await
            .expect("presigned");
        println!("  presigned (1h):   {presigned}");
    }

    println!("\n=== files left behind for inspection ===");
    println!("Open MinIO console: http://127.0.0.1:9101");
    println!("Bucket: rustango-test");
    println!("Login:  rustango / rustango-test-secret");
    println!();
    println!("To clean up later:");
    for (key, _, _) in files {
        println!("  curl -X DELETE  {}", storage.url(key).unwrap());
    }
    println!();
}
