//! Issue #384 — `register_template_context_processor!`
//! registers a callable that gets merged into every Tera
//! template's context via `apply_to_context`. Verifies the macro
//! shape end-to-end + the handler-key-wins override semantics.

#![cfg(all(feature = "sqlite", feature = "_tera"))]

use std::collections::HashMap;

use axum::http::Request;
use serde_json::{json, Value};
use tera::{Context, Tera};

// Register two processors at static-init time. Both fire on every
// `apply_to_context` call, in registration order.
rustango::register_template_context_processor!(|_parts| {
    let mut out: HashMap<String, Value> = HashMap::new();
    out.insert("build_version".into(), json!("test-build"));
    out.insert("winner".into(), json!("processor")); // collides with handler key
    out
});

rustango::register_template_context_processor!(|parts| {
    let mut out: HashMap<String, Value> = HashMap::new();
    out.insert("request_path".into(), json!(parts.uri.path().to_string()));
    out
});

// Collides with view keys (`object`, `is_update`) and an `ExtraContext`
// key (`shared`): the view, then `ExtraContext`, must win.
rustango::register_template_context_processor!(|_parts| {
    let mut out: HashMap<String, Value> = HashMap::new();
    out.insert("object".into(), json!("processor"));
    out.insert("is_update".into(), json!("processor"));
    out.insert("shared".into(), json!("processor"));
    out
});

fn parts(path: &str) -> axum::http::request::Parts {
    let req: Request<()> = Request::builder().uri(path).body(()).unwrap();
    let (p, ()) = req.into_parts();
    p
}

#[test]
fn registered_processors_inject_their_keys() {
    let parts = parts("/some/path");
    let ctx = rustango::template_context_processors::context_from_processors(&parts);
    let json = ctx.into_json();
    let obj = json.as_object().unwrap();
    assert_eq!(obj.get("build_version").unwrap(), &json!("test-build"));
    assert_eq!(obj.get("request_path").unwrap(), &json!("/some/path"));
    assert_eq!(obj.get("winner").unwrap(), &json!("processor"));
}

#[test]
fn handler_supplied_keys_win_over_processor_keys() {
    let parts = parts("/handler-wins-path");
    let mut ctx = Context::new();
    ctx.insert("winner", &"caller");
    rustango::template_context_processors::apply_to_context(&mut ctx, &parts);
    let json = ctx.into_json();
    let obj = json.as_object().unwrap();
    // Handler's `winner` survives; processor's was skipped.
    assert_eq!(obj.get("winner").unwrap(), &json!("caller"));
    // Processor-only keys still land.
    assert_eq!(obj.get("build_version").unwrap(), &json!("test-build"));
    assert_eq!(
        obj.get("request_path").unwrap(),
        &json!("/handler-wins-path")
    );
}

#[test]
fn processors_render_through_an_actual_tera_template() {
    let parts = parts("/render-test");
    let ctx = rustango::template_context_processors::context_from_processors(&parts);

    let mut tera = Tera::default();
    // Tera's default HTML auto-escape policy fires on `.html`
    // templates and turns `/` into `&#x2F;`. Use `safe` so the
    // assertion stays readable.
    tera.add_raw_template(
        "page.html",
        "v={{ build_version | safe }} path={{ request_path | safe }}",
    )
    .unwrap();
    let rendered = tera.render("page.html", &ctx).unwrap();
    assert_eq!(rendered, "v=test-build path=/render-test");
}

#[test]
fn parts_value_threads_through_to_the_processor() {
    // Different paths produce different `request_path` injections.
    let p1 = parts("/a");
    let p2 = parts("/b");
    let c1 = rustango::template_context_processors::context_from_processors(&p1);
    let c2 = rustango::template_context_processors::context_from_processors(&p2);
    assert_eq!(c1.into_json().get("request_path").unwrap(), &json!("/a"));
    assert_eq!(c2.into_json().get("request_path").unwrap(), &json!("/b"));
}

/// The class-based views merge the registered processors; a layout
/// reading a processor key used to fail to render.
#[cfg(feature = "template_views")]
mod class_based_views {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use rustango::core::Model as _;
    use rustango::sql::{Auto, Pool};
    use rustango::template_views::{
        CreateView, DeleteView, DetailView, ExtraContext, FormView, ListView, TemplateView,
        UpdateView,
    };
    use rustango::{Form, Model};
    use tera::{Context, Tera};
    use tower::ServiceExt;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "cp_post", display = "title")]
    #[allow(dead_code)]
    pub struct CpPost {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 200)]
        pub title: String,
    }

    async fn pool() -> Pool {
        let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
        rustango::sql::raw_execute_pool(
            &pool,
            "CREATE TABLE cp_post (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL)",
            Vec::new(),
        )
        .await
        .expect("create");
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO cp_post (id, title) VALUES (1, 'Hello')",
            Vec::new(),
        )
        .await
        .expect("seed");
        pool
    }

    fn tera() -> Arc<Tera> {
        let mut tera = Tera::default();
        for name in [
            "page.html",
            "cp_post_list.html",
            "cp_post_detail.html",
            "cp_post_form.html",
        ] {
            tera.add_raw_template(name, "v={{ build_version }} p={{ request_path | safe }}")
                .unwrap();
        }
        Arc::new(tera)
    }

    async fn get(app: axum::Router, uri: &str) -> String {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes).into_owned();
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        body
    }

    #[tokio::test]
    async fn template_view_merges_processors() {
        let app = TemplateView::new("page.html").router("/about", tera());
        assert_eq!(get(app, "/about").await, "v=test-build p=/about");
    }

    #[tokio::test]
    async fn list_detail_and_create_views_merge_processors() {
        let pool = pool().await;
        let list = ListView::for_model(CpPost::SCHEMA).router("/posts", tera(), pool.clone());
        assert_eq!(get(list, "/posts").await, "v=test-build p=/posts");
        let detail = DetailView::for_model(CpPost::SCHEMA).router("/posts", tera(), pool.clone());
        assert_eq!(get(detail, "/posts/1").await, "v=test-build p=/posts/1");
        let create =
            CreateView::for_model(CpPost::SCHEMA, &["title"]).router("/posts", tera(), pool);
        assert_eq!(get(create, "/posts/new").await, "v=test-build p=/posts/new");
    }

    const CSRF: &str = "cp-csrf-token-cp-csrf-token-cp-csrf-token-x";

    /// Templates print a view key, an `ExtraContext` key and a processor-only key.
    fn precedence_tera() -> Arc<Tera> {
        let mut tera = Tera::default();
        tera.add_raw_templates(vec![
            (
                "cp_post_detail.html",
                "{{ object.title }}|{{ shared }}|{{ build_version }}",
            ),
            (
                "cp_post_confirm_delete.html",
                "{{ object.title }}|{{ shared }}|{{ build_version }}",
            ),
            (
                "cp_post_form.html",
                "{{ is_update }}|{{ shared }}|{{ build_version }}",
            ),
            (
                "form.html",
                "{{ errors | length }}|{{ shared }}|{{ build_version }}",
            ),
        ])
        .unwrap();
        Arc::new(tera)
    }

    fn with_extra(app: axum::Router) -> axum::Router {
        let mut extra = Context::new();
        extra.insert("shared", "extra");
        extra.insert("object", "extra");
        extra.insert("is_update", "extra");
        app.layer(axum::Extension(ExtraContext(extra)))
    }

    async fn send(app: axum::Router, method: &str, uri: &str, form: &str) -> (StatusCode, String) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", format!("rustango_csrf={CSRF}"))
                    .body(Body::from(format!("_csrf={CSRF}&{form}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn view_keys_beat_extra_context_which_beats_processors() {
        let pool = pool().await;
        let detail = with_extra(DetailView::for_model(CpPost::SCHEMA).router(
            "/posts",
            precedence_tera(),
            pool.clone(),
        ));
        assert_eq!(get(detail, "/posts/1").await, "Hello|extra|test-build");
        let delete = with_extra(DeleteView::for_model(CpPost::SCHEMA).router(
            "/posts",
            precedence_tera(),
            pool.clone(),
        ));
        assert_eq!(
            get(delete, "/posts/1/delete").await,
            "Hello|extra|test-build"
        );
        let update = || {
            with_extra(UpdateView::for_model(CpPost::SCHEMA, &["title"]).router(
                "/posts",
                precedence_tera(),
                pool.clone(),
            ))
        };
        assert_eq!(
            get(update(), "/posts/1/edit").await,
            "true|extra|test-build"
        );
        // A failed POST re-renders the form through the same merge.
        let (status, body) = send(update(), "POST", "/posts/1/edit", "title=").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body, "true|extra|test-build");
    }

    #[derive(Form)]
    pub struct CpForm {
        #[allow(dead_code)]
        pub name: String,
    }

    #[tokio::test]
    async fn form_view_merges_extra_context_and_processors() {
        let view = || {
            with_extra(
                FormView::<CpForm>::for_form(|_f| async { Ok(()) })
                    .router("/contact", precedence_tera()),
            )
        };
        assert_eq!(get(view(), "/contact").await, "0|extra|test-build");
        // An invalid form re-renders as a 422.
        let (status, body) = send(view(), "POST", "/contact", "").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body, "1|extra|test-build");
    }
}
