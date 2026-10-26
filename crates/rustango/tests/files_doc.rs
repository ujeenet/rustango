//! Backing test for `docs/files.md` — the `Storage` trait (in-memory + local
//! disk), public URLs, and the upload guards (`UploadConfig`,
//! `sanitize_filename`). The multipart `save_uploads` path is dogfooded by the
//! in-file tests in `src/uploads.rs` and by `media_sqlite_live.rs`.
//!
//! Run: `cargo test -p rustango --test files_doc`

#![cfg(all(feature = "storage", feature = "uploads"))]

use rustango::storage::{InMemoryStorage, LocalStorage, Storage};
use rustango::uploads::{sanitize_filename, UploadConfig};

#[tokio::test]
async fn in_memory_storage_roundtrip() {
    let store = InMemoryStorage::new();

    assert!(!store.exists("avatars/7.png").await.unwrap());
    store.save("avatars/7.png", b"\x89PNG...").await.unwrap();
    assert!(store.exists("avatars/7.png").await.unwrap());
    assert_eq!(store.load("avatars/7.png").await.unwrap(), b"\x89PNG...");

    store.delete("avatars/7.png").await.unwrap();
    assert!(!store.exists("avatars/7.png").await.unwrap());
}

#[tokio::test]
async fn local_storage_roundtrip_and_public_url() {
    let dir = tempfile::tempdir().unwrap();
    // Attach a base URL so saved files get a public address (a CDN / static host).
    let store = LocalStorage::new(dir.path().to_path_buf())
        .with_base_url("https://cdn.example.com/uploads");

    store.save("docs/report.pdf", b"%PDF-1.7").await.unwrap();
    assert_eq!(store.load("docs/report.pdf").await.unwrap(), b"%PDF-1.7");

    // url() builds {base}/{key} — what you store on the model / hand to a browser.
    assert_eq!(
        store.url("docs/report.pdf").as_deref(),
        Some("https://cdn.example.com/uploads/docs/report.pdf")
    );
    // Without a base URL there's no public URL (you'd serve it via a handler).
    assert_eq!(InMemoryStorage::new().url("x").as_deref(), None);
}

#[test]
fn upload_config_sets_guards() {
    let cfg = UploadConfig::new("avatars/")
        .max_bytes(2 * 1024 * 1024) // reject files over 2 MiB
        .allowed_extensions(&["PNG", "Jpg"]); // case-insensitive

    // Extensions are normalized to lowercase so the check is case-insensitive.
    assert!(cfg.allowed_extensions.contains("png"));
    assert!(cfg.allowed_extensions.contains("jpg"));
    assert!(!cfg.allowed_extensions.contains("PNG"));
}

#[test]
fn sanitize_filename_blocks_path_traversal_and_unsafe_chars() {
    // Client-supplied names are taken down to a safe basename.
    assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
    assert_eq!(sanitize_filename("my photo!.png"), "my_photo_.png");
    assert_eq!(sanitize_filename(""), "upload"); // never empty
}

mod streaming {
    use axum::body::Body;
    use axum::http::{header, HeaderMap, StatusCode};
    use rustango::storage::{BoxedStorage, ByteStream, PutConditions, StorageError};

    // The handlers from the "Large files" section, with the extractors' values passed in.
    async fn upload(
        storage: BoxedStorage,
        headers: HeaderMap,
        body: Body,
    ) -> Result<String, StatusCode> {
        let len = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
            .ok_or(StatusCode::LENGTH_REQUIRED)?;
        let key = format!("uploads/{}", uuid::Uuid::new_v4());
        let put = PutConditions::new().create_only();
        storage
            .save_stream(&key, len, ByteStream::from(body), &put)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        Ok(key)
    }

    async fn download(storage: BoxedStorage, key: String) -> Result<Body, StatusCode> {
        let stream = storage.load_stream(&key).await.map_err(|e| match e {
            StorageError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_GATEWAY,
        })?;
        Ok(Body::from_stream(stream))
    }

    #[tokio::test]
    async fn proxy_an_upload_into_save_stream_and_back() {
        use http_body_util::BodyExt as _;
        let dir = tempfile::tempdir().unwrap();
        let storage: BoxedStorage =
            std::sync::Arc::new(rustango::storage::LocalStorage::new(dir.path().into()));
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, "11".parse().unwrap());
        let key = upload(storage.clone(), headers.clone(), Body::from("hello world"))
            .await
            .unwrap();
        let body = download(storage.clone(), key).await.unwrap();
        assert_eq!(body.collect().await.unwrap().to_bytes(), "hello world");
        // A body of another size than declared is refused.
        assert_eq!(
            upload(storage.clone(), headers, Body::from("short")).await,
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            upload(storage.clone(), HeaderMap::new(), Body::from("x")).await,
            Err(StatusCode::LENGTH_REQUIRED)
        );
        assert_eq!(
            download(storage, "uploads/nope".into()).await.err(),
            Some(StatusCode::NOT_FOUND)
        );
    }
}
