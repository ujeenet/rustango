//! Backing test for `docs/files.md` — the `Storage` trait (in-memory + local
//! disk), public URLs, and the upload guards (`UploadConfig`,
//! `sanitize_filename`). The multipart `save_uploads` path is dogfooded by the
//! in-file tests in `src/uploads.rs` and by `media_sqlite_live.rs`; the
//! avatar example compiles and runs here on SQLite.
//!
//! Run: `cargo test -p rustango --test files_doc`

#![cfg(all(feature = "storage", feature = "uploads"))]

#[path = "support/doc_snippet.rs"]
mod doc_snippet;

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

// ------------------------------------------------- the page's app examples
//
// Each marked block is a code example of `docs/files.md`, copied verbatim;
// `every_locale_publishes_the_tested_snippets` keeps them in sync. `myblog`
// in the `main.rs` example is this test crate.

#[test]
fn every_locale_publishes_the_tested_snippets() {
    doc_snippet::assert_published(include_str!("files_doc.rs"), "files.md");
}

/// Step 3's upload handler.
#[allow(dead_code)]
#[rustfmt::skip]
mod step3 {
    // doc-snippet:start
    use axum::extract::{Multipart, State};
    use axum::http::StatusCode;
    use axum::Json;
    use rustango::storage::BoxedStorage;
    use rustango::uploads::{save_uploads, UploadConfig};

    async fn upload(
        State(storage): State<BoxedStorage>,             // router state, see below
        mp: Multipart,                                   // must be the last argument
    ) -> Result<Json<Vec<String>>, (StatusCode, String)> {
        let cfg = UploadConfig::new("avatars/")          // key prefix
            .max_bytes(2 * 1024 * 1024)                  // reject files over 2 MiB
            .allowed_extensions(&["png", "jpg", "jpeg", "webp"])
            .randomize_filename(true);                   // avoid collisions

        // UploadError is not a response: map it yourself.
        let saved = save_uploads(mp, &cfg, &storage)     // Vec<SavedUpload>
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        Ok(Json(saved.into_iter().map(|s| s.key).collect()))
    }
    // doc-snippet:end
}

/// "Upload an avatar, end to end": the model, the view and the routes.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
// `avatar_routes` is used only by the `main.rs` example, which needs `batteries`.
#[allow(dead_code)]
#[rustfmt::skip]
mod blog {
    pub mod models {
        use rustango::{Auto, Model};

        // doc-snippet:start
        #[derive(Model, Clone, Debug)]
        #[rustango(table = "profiles")]
        pub struct Profile {
            #[rustango(primary_key)]
            pub id: Auto<i64>,

            pub user_id: i64,

            // Storage key, e.g. "avatars/3f2c…-me.png". None until the first upload.
            pub avatar_key: Option<String>,
        }
        // doc-snippet:end
    }

    pub mod views {
        // doc-snippet:start
        use axum::extract::{Multipart, Path, State};
        use axum::http::StatusCode;
        use axum::Json;
        use rustango::sql::Pool;
        use rustango::storage::BoxedStorage;
        use rustango::uploads::{save_uploads, UploadConfig};

        use crate::blog::models::Profile;

        /// What the avatar routes need: the database and the file storage.
        #[derive(Clone)]
        pub struct AvatarState {
            pub pool: Pool,
            pub storage: BoxedStorage,
        }

        type HttpError = (StatusCode, String);

        fn server_error(e: impl std::fmt::Display) -> HttpError {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }

        /// POST /profiles/{id}/avatar with a multipart form that holds one image.
        pub async fn upload_avatar(
            State(state): State<AvatarState>,
            Path(id): Path<i64>,
            mp: Multipart,
        ) -> Result<Json<serde_json::Value>, HttpError> {
            let mut profile = Profile::objects()
                .find(id, &state.pool)
                .await
                .map_err(server_error)?
                .ok_or((StatusCode::NOT_FOUND, "no such profile".to_owned()))?;

            let cfg = UploadConfig::new("avatars/")
                .max_bytes(2 * 1024 * 1024) // 2 MiB
                .max_files(1)
                .allowed_extensions(&["png", "jpg", "jpeg", "webp"]);
            let saved = save_uploads(mp, &cfg, &state.storage)
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            let Some(file) = saved.into_iter().next() else {
                return Err((StatusCode::BAD_REQUEST, "send one file".to_owned()));
            };

            // Save the key on the row. If that fails, remove the new file.
            let old_key = profile.avatar_key.replace(file.key.clone());
            if let Err(e) = profile.save(&state.pool).await {
                let _ = state.storage.delete(&file.key).await;
                return Err(server_error(e));
            }
            // Nothing points at the old file any more.
            if let Some(old) = old_key {
                let _ = state.storage.delete(&old).await;
            }

            Ok(Json(serde_json::json!({
                "key": file.key,
                "url": state.storage.url(&file.key), // "/uploads/avatars/…-me.png"
            })))
        }
        // doc-snippet:end
    }

    pub mod urls {
        use axum::Router;

        // doc-snippet:start
        use std::path::PathBuf;
        use std::sync::Arc;

        use axum::routing::post;
        use rustango::sql::Pool;
        use rustango::storage::{BoxedStorage, LocalStorage};

        use crate::blog::views::{upload_avatar, AvatarState};

        /// The folder that holds uploads, and the URL prefix that serves it.
        pub const UPLOAD_DIR: &str = "./var/uploads";
        pub const UPLOAD_URL: &str = "/uploads";

        pub fn avatar_routes(pool: Pool) -> Router {
            let storage: BoxedStorage = Arc::new(
                LocalStorage::new(PathBuf::from(UPLOAD_DIR)).with_base_url(UPLOAD_URL),
            );
            Router::new()
                .route("/{id}/avatar", post(upload_avatar))
                .with_state(AvatarState { pool, storage })
        }
        // doc-snippet:end
    }
}

#[cfg(all(
    feature = "batteries",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
#[path = "support/scaffold_urls.rs"]
mod urls;

/// The avatar example's `Cli` chain in `main.rs`.
#[cfg(all(
    feature = "batteries",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
#[allow(dead_code)]
#[rustfmt::skip]
mod avatar_main {
    use crate as myblog;
    use crate::urls;

    async fn run(
        loaded: Result<rustango::config::Settings, rustango::config::ConfigError>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // doc-snippet:start
        rustango::manage::Cli::new()
            .api(urls::api())
            // … the lines you already have …
            .nest_with("/profiles", myblog::blog::urls::avatar_routes)
            .with_uploads(myblog::blog::urls::UPLOAD_URL, myblog::blog::urls::UPLOAD_DIR)
            .with_loaded_settings(loaded)
            .run()
            .await
        // doc-snippet:end
    }
}

/// The avatar route, end to end on in-memory SQLite and a temp folder.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn the_avatar_example_saves_the_key_and_replaces_the_old_file() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use blog::models::Profile;
    use blog::views::{upload_avatar, AvatarState};
    use http_body_util::BodyExt as _;
    use rustango::sql::{Auto, Pool};
    use rustango::storage::BoxedStorage;
    use tower::ServiceExt as _;

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let pool = Pool::Sqlite(pool);
    rustango::testkit::create_tables_for::<Profile>(&pool)
        .await
        .unwrap();
    let mut profile = Profile {
        id: Auto::default(),
        user_id: 7,
        avatar_key: None,
    };
    profile.save(&pool).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let storage: BoxedStorage =
        std::sync::Arc::new(LocalStorage::new(dir.path().to_path_buf()).with_base_url("/uploads"));
    // `avatar_routes` with the temp folder in place of `./var/uploads`.
    let app = axum::Router::new()
        .route("/{id}/avatar", axum::routing::post(upload_avatar))
        .with_state(AvatarState {
            pool: pool.clone(),
            storage: storage.clone(),
        });

    async fn send(app: &axum::Router, path: &str, file: &str) -> (StatusCode, String) {
        let body = format!(
            "--XB\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"{file}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\nbytes\r\n--XB--\r\n"
        );
        let req = Request::post(path)
            .header("content-type", "multipart/form-data; boundary=XB")
            .body(Body::from(body))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    let (status, body) = send(&app, "/1/avatar", "me.png").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let first = json["key"].as_str().unwrap().to_owned();
    assert!(
        first.starts_with("avatars/") && first.ends_with("-me.png"),
        "{first}"
    );
    assert_eq!(json["url"], format!("/uploads/{first}"));
    let row = Profile::objects()
        .find(1_i64, &pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.avatar_key.as_deref(), Some(first.as_str()));

    // A second upload replaces the key and removes the old file.
    let (status, _) = send(&app, "/1/avatar", "me.png").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!storage.exists(&first).await.unwrap(), "old avatar deleted");

    let (status, body) = send(&app, "/1/avatar", "me.gif").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "file extension `gif` not allowed");
    let (status, _) = send(&app, "/99/avatar", "me.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The page's `[server]` block sets the body limit, in each locale.
#[cfg(feature = "config")]
#[test]
fn the_documented_body_limit_loads() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    for dir in ["", "de", "es", "fr"] {
        let page = std::fs::read_to_string(docs.join(dir).join("files.md")).unwrap();
        let block = page
            .split("```toml\n")
            .skip(1)
            .filter_map(|rest| rest.split("```").next())
            .find(|block| block.contains("[server]"))
            .expect("a toml block with [server]");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("default.toml"), block).unwrap();
        let settings = rustango::config::Settings::load_from(tmp.path(), "dev").unwrap();
        assert_eq!(settings.server.max_body_bytes, Some(5 * 1024 * 1024));
    }
}
