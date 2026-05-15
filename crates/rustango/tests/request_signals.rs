//! Tests for `signals::request` (issue #53) — `request_started` /
//! `request_finished` / `got_request_exception`. The registry is
//! process-global, so we serialize the tests with a `Mutex` and
//! `clear_all()` between cases.

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::signals::request::{
    clear_all, connect_request_finished, connect_request_started, disconnect_request_started,
    receiver_count, send_got_request_exception, send_request_finished, send_request_started,
    RequestExceptionContext, RequestFinishedContext, RequestSignalsLayer, RequestStartedContext,
};
use tower::ServiceExt;

/// Process-global lock — the request-signals registry is process-wide,
/// so tests can't run in parallel without clobbering each other.
fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn dispatch_fires_every_connected_receiver_in_order() {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    clear_all();
    let order: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    let o1 = order.clone();
    connect_request_started(move |_ctx| {
        let o = o1.clone();
        Box::pin(async move {
            o.lock().unwrap().push(1);
        })
    });
    let o2 = order.clone();
    connect_request_started(move |_ctx| {
        let o = o2.clone();
        Box::pin(async move {
            o.lock().unwrap().push(2);
        })
    });

    send_request_started(RequestStartedContext {
        method: "GET".into(),
        path: "/x".into(),
        query: String::new(),
    })
    .await;

    assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    clear_all();
}

#[tokio::test]
async fn disconnect_removes_receiver() {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    clear_all();

    let id = connect_request_started(|_ctx| Box::pin(async move {}));
    assert_eq!(receiver_count(), 1);
    assert!(disconnect_request_started(id));
    assert_eq!(receiver_count(), 0);
    // Double-disconnect is a no-op (false).
    assert!(!disconnect_request_started(id));
    clear_all();
}

#[tokio::test]
async fn finished_carries_status_and_elapsed() {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    clear_all();
    let captured: Arc<Mutex<Option<RequestFinishedContext>>> = Arc::new(Mutex::new(None));
    let c = captured.clone();
    connect_request_finished(move |ctx| {
        let c = c.clone();
        Box::pin(async move {
            *c.lock().unwrap() = Some(ctx);
        })
    });

    send_request_finished(RequestFinishedContext {
        method: "POST".into(),
        path: "/api/things".into(),
        status: 201,
        elapsed_ms: 12.5,
    })
    .await;

    let ctx = captured.lock().unwrap().clone().expect("finished fired");
    assert_eq!(ctx.method, "POST");
    assert_eq!(ctx.path, "/api/things");
    assert_eq!(ctx.status, 201);
    assert!((ctx.elapsed_ms - 12.5).abs() < f64::EPSILON);
    clear_all();
}

#[tokio::test]
async fn exception_dispatch_works() {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    clear_all();
    let captured: Arc<Mutex<Option<RequestExceptionContext>>> = Arc::new(Mutex::new(None));
    let c = captured.clone();
    rustango::signals::request::connect_got_request_exception(move |ctx| {
        let c = c.clone();
        Box::pin(async move {
            *c.lock().unwrap() = Some(ctx);
        })
    });

    send_got_request_exception(RequestExceptionContext {
        method: "GET".into(),
        path: "/boom".into(),
        error: "kaboom".into(),
    })
    .await;

    let ctx = captured.lock().unwrap().clone().expect("exception fired");
    assert_eq!(ctx.method, "GET");
    assert_eq!(ctx.path, "/boom");
    assert_eq!(ctx.error, "kaboom");
    clear_all();
}

#[tokio::test]
async fn layer_fires_started_and_finished_around_handler() {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    clear_all();

    // Sentinels: track ordering — start=1, handler=2, finish=3.
    static SEEN: AtomicU64 = AtomicU64::new(0);
    static FINISH_STATUS: AtomicI32 = AtomicI32::new(0);
    SEEN.store(0, Ordering::SeqCst);

    connect_request_started(|_ctx| {
        Box::pin(async move {
            SEEN.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .ok();
        })
    });
    connect_request_finished(|ctx| {
        Box::pin(async move {
            SEEN.compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst)
                .ok();
            FINISH_STATUS.store(i32::from(ctx.status), Ordering::SeqCst);
        })
    });

    let app: Router = Router::new()
        .route(
            "/hello",
            get(|| async {
                SEEN.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
                    .ok();
                "ok"
            }),
        )
        .layer(RequestSignalsLayer::new());

    let req = Request::builder()
        .method("GET")
        .uri("/hello")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        SEEN.load(Ordering::SeqCst),
        3,
        "expected ordering: started(1) → handler(2) → finished(3)"
    );
    assert_eq!(FINISH_STATUS.load(Ordering::SeqCst), 200);
    clear_all();
}

#[tokio::test]
async fn layer_passes_through_404_with_correct_status() {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    clear_all();
    static OBSERVED_STATUS: AtomicI32 = AtomicI32::new(0);
    OBSERVED_STATUS.store(0, Ordering::SeqCst);

    connect_request_finished(|ctx| {
        Box::pin(async move {
            OBSERVED_STATUS.store(i32::from(ctx.status), Ordering::SeqCst);
        })
    });

    let app: Router = Router::new()
        .route("/known", get(|| async { "ok" }))
        .layer(RequestSignalsLayer::new());

    let req = Request::builder()
        .method("GET")
        .uri("/unknown")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(OBSERVED_STATUS.load(Ordering::SeqCst), 404);
    clear_all();
}

#[tokio::test]
async fn layer_records_method_and_path() {
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    clear_all();
    let captured: Arc<Mutex<Option<RequestStartedContext>>> = Arc::new(Mutex::new(None));
    let c = captured.clone();
    connect_request_started(move |ctx| {
        let c = c.clone();
        Box::pin(async move {
            *c.lock().unwrap() = Some(ctx);
        })
    });

    let app: Router = Router::new()
        .route("/posts/{id}", get(|| async { "ok" }))
        .layer(RequestSignalsLayer::new());

    let req = Request::builder()
        .method("GET")
        .uri("/posts/42?draft=1")
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap();

    let ctx = captured.lock().unwrap().clone().expect("started fired");
    assert_eq!(ctx.method, "GET");
    assert_eq!(ctx.path, "/posts/42");
    assert_eq!(ctx.query, "draft=1");
    clear_all();
}
