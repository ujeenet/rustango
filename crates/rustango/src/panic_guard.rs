//! `catch_unwind` for a future, without a `futures` dependency.

use std::future::Future;

/// Drive `fut` to completion, turning a panic in any single `poll`
/// into an `Err`.
pub(crate) async fn catch_unwind<F: Future>(fut: F) -> std::thread::Result<F::Output> {
    use std::task::Poll;
    let mut fut = Box::pin(fut);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    })
    .await
}

/// The message of a caught panic, when it carried a string.
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// Turn a panicking handler into a logged, opaque 500 instead of a
/// dropped connection (#1541). Mount it inside the request-id and
/// access-log layers so both see the 500, and inside CORS and the
/// security headers so the 500 carries them.
/// A panic inside a streaming body, after the headers are sent, is not caught.
#[cfg(any(feature = "manage", feature = "tenancy", feature = "runserver"))]
#[must_use]
pub(crate) fn catch_panics(router: axum::Router) -> axum::Router {
    router.layer(axum::middleware::from_fn(
        |req: axum::extract::Request, next: axum::middleware::Next| async move {
            // A `runserver`-only build has no request-id layer.
            #[cfg(feature = "_http_layers")]
            let request_id = req
                .extensions()
                .get::<crate::request_id::RequestId>()
                .map(|id| id.0.clone());
            #[cfg(not(feature = "_http_layers"))]
            let request_id: Option<String> = None;
            let method = req.method().clone();
            let path = req.uri().path().to_owned();
            match catch_unwind(next.run(req)).await {
                Ok(response) => response,
                Err(panic) => {
                    tracing::error!(
                        target: "rustango::error",
                        request_id = request_id.as_deref().unwrap_or("-"),
                        method = %method,
                        path = %path,
                        panic = panic_message(&*panic),
                        "handler panicked"
                    );
                    let mut resp = axum::response::Response::new(axum::body::Body::from(
                        crate::error::OPAQUE_SERVER_ERROR,
                    ));
                    *resp.status_mut() = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                    resp.headers_mut().insert(
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
                    );
                    resp
                }
            }
        },
    ))
}
