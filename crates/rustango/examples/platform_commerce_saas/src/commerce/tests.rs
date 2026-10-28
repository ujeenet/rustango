//! App-level tests that need no database.
//!
//! The ones that do need a database live in `tests/`, where they can
//! skip cleanly when `DATABASE_URL` is unset.

/// Every `#[derive(Model)]` registers itself in `inventory` at link
/// time, and the auto-admin walks that registry — so a model missing
/// here is a model the admin will not show.
///
/// Listing all seven rather than one: this is the cheapest place to
/// notice a model that was written but never reached, which is a
/// failure mode no HTTP test can see.
#[test]
fn every_commerce_model_is_registered() {
    use rustango::core::ModelEntry;
    let tables: Vec<&'static str> = rustango::inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema.table)
        .collect();

    for expected in [
        "commerce_customer",
        "commerce_staff",
        "commerce_product",
        "commerce_inventory",
        "commerce_order",
        "commerce_order_line",
        "commerce_shipment_event",
    ] {
        assert!(
            tables.iter().any(|t| *t == expected),
            "`{expected}` missing from inventory; tables: {tables:?}",
        );
    }
}

/// The #1450 column has to stay nullable, because a nullable `bigint`
/// is the whole reason it exists. Drop the `Option` and the test that
/// matters stops testing anything — silently, since a non-null insert
/// works fine on every dialect.
#[test]
fn the_picker_column_is_still_nullable() {
    use rustango::core::ModelEntry;
    let order = rustango::inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == "commerce_order")
        .expect("commerce_order registered");

    let picker = order
        .schema
        .fields
        .iter()
        .find(|f| f.column == "assigned_picker_id")
        .expect("commerce_order has an assigned_picker_id column");

    assert!(
        picker.nullable,
        "assigned_picker_id must stay nullable — it is the #1450 guard, and a \
         NOT NULL column never reaches the binder that was broken"
    );

    // And the control alongside it: a nullable TEXT column, which
    // always worked. If both were non-nullable the pair proves nothing.
    let note = order
        .schema
        .fields
        .iter()
        .find(|f| f.column == "note")
        .expect("commerce_order has a note column");
    assert!(note.nullable, "note is the nullable-TEXT control");
}

/// The supervisor has to retire queues, not only start them.
///
/// It only ever added: `ensure_queue` for every active tenant, tick
/// after tick, and nothing for a tenant that stopped being active. The
/// queue kept two workers polling a database the tenant no longer used,
/// held its pool against the server's connection limit and against the
/// 64-pool cache cap (which has no eviction), and `shutdown_all` drained
/// it on every deploy.
///
/// Nothing reported it, because a deactivated tenant produces no error —
/// it just stops appearing in the registry query. The set difference
/// below *is* the missing signal, so it is what this pins.
#[test]
fn a_deactivated_tenant_is_retired() {
    use crate::commerce::supervisor::retired_slugs;

    let running = vec!["acme".to_string(), "globex".to_string(), "初芝".to_string()];

    // Steady state: every running queue still active, nothing retired.
    assert!(
        retired_slugs(&running, &running).is_empty(),
        "a tick that changes nothing must retire nothing"
    );

    // `globex` deactivated (or deleted — both look the same from here).
    let active = vec!["acme".to_string(), "初芝".to_string()];
    assert_eq!(
        retired_slugs(&running, &active),
        vec!["globex".to_string()],
        "a tenant absent from the active list must be retired"
    );

    // A tenant that is active but has no queue yet is `ensure_queue`'s
    // job, not this one — it must not appear here.
    let active_plus_new = vec![
        "acme".to_string(),
        "globex".to_string(),
        "初芝".to_string(),
        "newco".to_string(),
    ];
    assert!(
        retired_slugs(&running, &active_plus_new).is_empty(),
        "a newly provisioned tenant is not a retirement"
    );

    // Every tenant gone — a registry emptied by a bad migration, say.
    assert_eq!(
        retired_slugs(&running, &[]).len(),
        3,
        "all queues retire when no tenant is active"
    );
}

/// The storefront escapes product text fields (#1636).
#[test]
fn the_storefront_escapes_product_text() {
    let p = super::models::Product {
        id: rustango::sql::Auto::Unset,
        sku: "<b>sku</b>".into(),
        name: "<script>alert(1)</script>".into(),
        description: None,
        price_cents: 1999,
        active: true,
        created_at: rustango::sql::Auto::Unset,
    };
    let li = super::views::product_item(&p);
    assert!(!li.contains("<script>") && !li.contains("<b>"), "got: {li}");
    assert!(
        li.contains(&rustango::text::html_escape("<script>alert(1)</script>")),
        "got: {li}"
    );
}

/// The tenant database for this build's backend, or `None` to skip:
/// `DATABASE_URL` on Postgres, `MYSQL_TEST_URL` on MySQL, a temp file on SQLite.
fn tenant_db() -> Option<(String, &'static str)> {
    #[cfg(feature = "postgres")]
    return std::env::var("DATABASE_URL").ok().map(|u| (u, "postgres"));
    #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
    return Some((
        format!(
            "sqlite://{}?mode=rwc",
            std::env::temp_dir()
                .join(format!("pcs_api_auth_{}.sqlite", std::process::id()))
                .display()
        ),
        "sqlite",
    ));
    #[cfg(all(feature = "mysql", not(any(feature = "postgres", feature = "sqlite"))))]
    return std::env::var("MYSQL_TEST_URL").ok().map(|u| (u, "mysql"));
}

/// Every request resolves to one tenant on `url`; no registry table needed.
#[derive(Clone)]
struct OneTenant(rustango::tenancy::Org);

#[async_trait::async_trait]
impl rustango::tenancy::OrgResolver for OneTenant {
    async fn resolve(
        &self,
        _parts: &axum::http::request::Parts,
        _registry: &rustango::sql::Pool,
    ) -> Result<Option<rustango::tenancy::Org>, rustango::tenancy::TenancyError> {
        Ok(Some(self.0.clone()))
    }
}

/// The REST API refuses every anonymous write, and a bad token, with 401.
#[tokio::test]
async fn anonymous_api_writes_are_refused() {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use rustango::extractors::TenantContext;
    use rustango::tenancy::auth_routes::{Config, JwtAuth};
    use rustango::tenancy::session::SessionSecret;
    use rustango::tenancy::{ChainResolver, DefaultTenantDb, Org, TenantPools};
    use tower::ServiceExt as _;

    let Some((url, backend)) = tenant_db() else {
        eprintln!("skipping: no database URL for this backend");
        return;
    };
    let org = Org {
        id: rustango::sql::Auto::Unset,
        slug: "acme".into(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: backend.into(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    let registry = rustango::sql::sqlx::Pool::<DefaultTenantDb>::connect(&url)
        .await
        .expect("connect");
    let ctx = std::sync::Arc::new(TenantContext {
        pools: std::sync::Arc::new(TenantPools::new(registry)),
        resolver: ChainResolver::new().push(OneTenant(org)),
        session_secret: SessionSecret::from_bytes(vec![1; 32]),
        operator_secret: SessionSecret::from_bytes(vec![2; 32]),
    });
    let auth = JwtAuth::new(Config {
        session_secret: Some(vec![3; 32]),
        ..Config::default()
    });
    let app = super::urls::rest_api(&auth).layer(axum::Extension(ctx));

    let mut writes = Vec::new();
    for prefix in [
        "products",
        "orders",
        "orders-raw",
        "order-lines",
        "customers",
        "inventory",
        "promotions",
        "gift-cards",
    ] {
        writes.push((Method::POST, format!("/api/v1/{prefix}")));
        for m in [Method::PUT, Method::PATCH, Method::DELETE] {
            writes.push((m, format!("/api/v1/{prefix}/1")));
        }
    }
    for (method, path) in &writes {
        for bearer in [None, Some("Bearer not.a.token")] {
            let mut req = Request::builder()
                .method(method.clone())
                .uri(path)
                .header("content-type", "application/json");
            if let Some(b) = bearer {
                req = req.header("authorization", b);
            }
            let req = req.body(Body::from("{}")).unwrap();
            let status = app.clone().oneshot(req).await.unwrap().status();
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path} with {bearer:?}");
        }
    }
}
