//! Every editable SSO field carries help text, and the admin form shows it.

#![cfg(all(
    feature = "sqlite",
    feature = "admin",
    feature = "sso",
    feature = "tenancy"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Model as _, ModelSchema};
use rustango::sql::Pool;
use tower::ServiceExt;

/// The fields a form edits: not the key, not read-only, not stamped.
fn editable(schema: &'static ModelSchema) -> Vec<&'static rustango::core::FieldSchema> {
    let readonly = schema.admin.map_or(&[][..], |a| a.readonly_fields);
    schema
        .fields
        .iter()
        .filter(|f| !f.primary_key && !f.auto && f.editable && !readonly.contains(&f.name))
        .collect()
}

#[test]
fn every_editable_sso_field_has_help_text() {
    use rustango::sso::policy::{SsoInvite, SsoSharedOverride, SsoTenantPolicy};
    use rustango::sso::SsoProvider;
    for schema in [
        SsoProvider::SCHEMA,
        SsoInvite::SCHEMA,
        SsoSharedOverride::SCHEMA,
        SsoTenantPolicy::SCHEMA,
    ] {
        for f in editable(schema) {
            assert!(
                f.help_text.is_some_and(|h| !h.is_empty()),
                "{}.{} has no help text",
                schema.table,
                f.name
            );
        }
    }
}

#[tokio::test]
async fn the_provider_form_shows_each_help_text() {
    use rustango::sso::SsoProvider;
    let pool = Pool::connect("sqlite::memory:").await.unwrap();
    rustango::testkit::create_tables_for::<SsoProvider>(&pool)
        .await
        .unwrap();
    let app = rustango::admin::Builder::new(pool).admin_prefix("").build();
    let req = Request::get("/rustango_sso_providers/new")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(body.to_vec()).unwrap();
    let fields = editable(SsoProvider::SCHEMA);
    assert!(fields.len() > 20, "{} fields", fields.len());
    for f in fields {
        let help = f.help_text.unwrap_or_default();
        assert!(
            !help.is_empty() && html.contains(&rustango::template_extensions::help_html(help)),
            "the form does not show the help text of `{}`",
            f.name
        );
        assert!(
            html.contains(&format!(r#"<label for="{}">{}"#, f.name, f.display_label())),
            "`{}` has no readable label",
            f.name
        );
    }
    // Backticks render as code, never literally.
    assert!(html.contains("<code>google</code>"), "no code span");
    assert!(!html.contains("`google`"), "a literal backtick span");
    // `kind` is a dropdown of the presets; `label` and `client_id` are one line.
    assert!(
        html.contains(r#"<select name="kind""#),
        "kind is not a dropdown"
    );
    for name in ["label", "client_id"] {
        assert!(
            html.contains(&format!(r#"<input type="text" name="{name}""#)),
            "`{name}` is not a one-line input"
        );
    }
}
