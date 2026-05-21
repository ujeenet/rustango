//! Django-parity #413 — `got_request_exception` signal fires on 5xx
//! responses passing through [`RequestSignalsLayer`]. Closes the audit
//! row's PARTIAL gap: pre-#413, the signal was wired only to the
//! `Service::Error` arm, which axum's `Infallible` bound makes
//! effectively dead code; now 500/502/503 responses also trigger it.

#![cfg(feature = "signals")]

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use rustango::signals::request::{
    clear_all, connect_got_request_exception, connect_request_finished, RequestExceptionContext,
    RequestFinishedContext, RequestSignalsLayer,
};

/// Cargo's default parallel harness runs these tests on a shared
/// global signal registry. Take a suite-wide mutex at entry so two
/// tests don't race between each other's `clear_all` + connect.
fn suite_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

fn app() -> Router {
    Router::new()
        .route("/ok", get(|| async { "ok" }))
        .route(
            "/boom",
            get(|| async {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(axum::body::Body::from("server error"))
                    .unwrap()
            }),
        )
        .route(
            "/bad_gateway",
            get(|| async {
                Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(axum::body::Body::from("upstream broken"))
                    .unwrap()
            }),
        )
        .route(
            "/teapot",
            get(|| async {
                // 418 — explicitly 4xx, NOT 5xx; must NOT fire the signal.
                Response::builder()
                    .status(StatusCode::IM_A_TEAPOT)
                    .body(axum::body::Body::from("418"))
                    .unwrap()
            }),
        )
        .layer(RequestSignalsLayer::new())
}

fn req(uri: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(axum::body::Body::empty())
        .unwrap()
}

#[tokio::test]
async fn got_request_exception_fires_on_500_response() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<RequestExceptionContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_got_request_exception(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let resp = app().oneshot(req("/boom")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let got = captured.lock().await;
    assert_eq!(got.len(), 1, "expected 1 signal fire, got {got:?}");
    assert_eq!(got[0].path, "/boom");
    assert_eq!(got[0].method, "GET");
    assert_eq!(got[0].status, Some(500));
    assert_eq!(got[0].error, "http 500");
}

#[tokio::test]
async fn got_request_exception_fires_on_502_response() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<RequestExceptionContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_got_request_exception(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let resp = app().oneshot(req("/bad_gateway")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let got = captured.lock().await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].status, Some(502));
}

#[tokio::test]
async fn got_request_exception_does_not_fire_on_4xx() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<RequestExceptionContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_got_request_exception(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let resp = app().oneshot(req("/teapot")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::IM_A_TEAPOT);

    let got = captured.lock().await;
    assert!(
        got.is_empty(),
        "4xx must NOT fire got_request_exception, got: {got:?}"
    );
}

#[tokio::test]
async fn got_request_exception_does_not_fire_on_2xx() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<RequestExceptionContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_got_request_exception(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    let resp = app().oneshot(req("/ok")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let got = captured.lock().await;
    assert!(got.is_empty(), "2xx must NOT fire got_request_exception");
}

#[tokio::test]
async fn request_finished_still_fires_alongside_exception_on_500() {
    let _g = suite_lock().lock().await;
    clear_all();

    let exc: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let fin: Arc<Mutex<Vec<RequestFinishedContext>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let exc = exc.clone();
        connect_got_request_exception(move |_| {
            let exc = exc.clone();
            async move {
                *exc.lock().await += 1;
            }
        });
    }
    {
        let fin = fin.clone();
        connect_request_finished(move |ctx| {
            let fin = fin.clone();
            async move {
                fin.lock().await.push(ctx);
            }
        });
    }

    let _resp = app().oneshot(req("/boom")).await.unwrap();
    assert_eq!(*exc.lock().await, 1, "exception receiver should fire once");
    let f = fin.lock().await;
    assert_eq!(f.len(), 1, "request_finished should still fire on 5xx");
    assert_eq!(f[0].status, 500);
}
