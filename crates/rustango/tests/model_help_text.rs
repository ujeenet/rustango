//! Every non-key field of every framework model has help text and a label,
//! and the admin form shows them.

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

/// The framework's own models: every `rustango_*` table this build registers.
fn framework_models() -> Vec<&'static ModelSchema> {
    let mut models: Vec<_> = rustango::migrate::registered_models()
        .into_iter()
        .filter(|s| s.table.starts_with("rustango_"))
        .collect();
    models.sort_by_key(|s| s.table);
    models.dedup_by_key(|s| s.table);
    models
}

#[test]
fn every_framework_field_has_help_text_and_a_label() {
    let mut missing = Vec::new();
    for schema in framework_models() {
        // Read-only and stamped fields too: the admin shows them all.
        for f in schema.fields.iter().filter(|f| !f.primary_key) {
            if !f.help_text.is_some_and(|h| !h.trim().is_empty()) {
                missing.push(format!("{}.{}: no help_text", schema.table, f.name));
            }
            if !f.verbose_name.is_some_and(|v| !v.trim().is_empty()) {
                missing.push(format!("{}.{}: no verbose_name", schema.table, f.name));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

/// The walk above sees every framework model, so it cannot pass on an
/// empty registry.
#[cfg(all(feature = "media", feature = "passkey", feature = "mcp"))]
#[test]
fn the_walk_covers_every_framework_model() {
    let seen: Vec<&str> = framework_models().iter().map(|s| s.table).collect();
    for table in [
        "rustango_orgs",
        "rustango_org_hosts",
        "rustango_db_servers",
        "rustango_tenant_databases",
        "rustango_operators",
        "rustango_users",
        "rustango_roles",
        "rustango_permissions",
        "rustango_user_roles",
        "rustango_role_permissions",
        "rustango_user_permissions",
        "rustango_admin_users",
        "rustango_api_keys",
        "rustango_agents",
        "rustango_agent_skills",
        "rustango_agent_grants",
        "rustango_agent_skill_tools",
        "rustango_agent_skill_permissions",
        "rustango_agent_skill_resources",
        "rustango_media",
        "rustango_media_collections",
        "rustango_media_tags",
        "rustango_media_tag_links",
        "rustango_translations",
        "rustango_content_types",
        "rustango_audit_log",
        "rustango_provisioning_runs",
        "rustango_provisioning_events",
        "rustango_webauthn_credentials",
        "rustango_admin_totp",
        "rustango_operator_totp",
        "rustango_sso_providers",
        "rustango_shared_sso_providers",
    ] {
        assert!(seen.contains(&table), "{table} is not registered: {seen:?}");
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
