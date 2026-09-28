//! `/_soak/*` probes: each runs one framework path the HTTP API alone
//! cannot reach, and answers with what happened, so the soak driver can
//! assert on it.
//!
//! **Twin file.** Identical in `platform_commerce_saas`; each app's
//! `urls.rs` passes the pool (the tenant's own on the SaaS app).
//!
//! Everything here writes rows or makes outbound requests, so the routes
//! answer 403 unless `SOAK_PROBES=1`. An example does not ship an open
//! endpoint that deletes rows or dials arbitrary URLs.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use rustango::core::Model as _;
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, Pool, UpdaterPool as _};
use serde_json::{json, Value};

use super::models::{Product, Promotion};

pub type ProbeResult = Result<Json<Value>, (StatusCode, String)>;

/// `Err(403)` unless the deployment opted in.
pub fn gate() -> Result<(), (StatusCode, String)> {
    if std::env::var("SOAK_PROBES").as_deref() == Ok("1") {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "set SOAK_PROBES=1 to enable the soak probes".to_owned(),
        ))
    }
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn nonce() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    )
}

async fn promotion(pool: &Pool, code: String, amount_cents: i64, visible: bool) -> Result<i64, (StatusCode, String)> {
    let mut row = Promotion {
        id: Auto::default(),
        label: format!("Promotion {code}"),
        code,
        amount_cents,
        visible,
        deleted_at: None,
    };
    row.insert_pool(pool).await.map_err(internal)?;
    row.id.get().copied().ok_or_else(|| internal("no pk"))
}

/// Two visible and two hidden promotions; the driver then drives the
/// ViewSet and template views at the hidden ones (#1746).
pub async fn scopes_seed(pool: &Pool) -> ProbeResult {
    gate()?;
    let tag = nonce();
    let mut visible = Vec::new();
    let mut hidden = Vec::new();
    for i in 0..2 {
        visible.push(promotion(pool, format!("VIS-{tag}-{i}"), 100, true).await?);
        hidden.push(promotion(pool, format!("HID-{tag}-{i}"), 100, false).await?);
    }
    Ok(Json(json!({ "tag": tag, "visible": visible, "hidden": hidden })))
}

/// A promotion read past the scope, so the driver can see a hidden row
/// is still there and unchanged.
pub async fn scopes_row(pool: &Pool, pk: i64) -> ProbeResult {
    gate()?;
    let rows: Vec<Promotion> = Promotion::objects()
        .without_global_scopes()
        .filter("id", pk)
        .fetch(pool)
        .await
        .map_err(internal)?;
    let Some(p) = rows.into_iter().next() else {
        return Err((StatusCode::NOT_FOUND, format!("no promotion {pk}")));
    };
    Ok(Json(json!({
        "id": pk, "code": p.code, "label": p.label,
        "amount_cents": p.amount_cents, "visible": p.visible,
    })))
}

/// The `Model::*` shortcuts next to their scoped `QuerySet` twins (#1675).
pub async fn scopes_shortcuts(pool: &Pool) -> ProbeResult {
    gate()?;
    let tag = nonce();
    // Hidden rows hold the extremes, so an unscoped aggregate is visibly wrong.
    let low = promotion(pool, format!("SHL-{tag}"), 1, false).await?;
    promotion(pool, format!("SHH-{tag}"), 1_000_000_000, false).await?;
    let shown = promotion(pool, format!("SHV-{tag}"), 500, true).await?;

    let qs = Promotion::objects();
    let out = json!({
        "shortcut_sum": Promotion::sum::<i64>("amount_cents", pool).await.map_err(internal)?,
        "scoped_sum": qs.clone().sum::<i64>("amount_cents", pool).await.map_err(internal)?,
        "shortcut_min": Promotion::min::<i64>("amount_cents", pool).await.map_err(internal)?,
        "scoped_min": qs.clone().min::<i64>("amount_cents", pool).await.map_err(internal)?,
        "shortcut_max": Promotion::max::<i64>("amount_cents", pool).await.map_err(internal)?,
        "scoped_max": qs.clone().max::<i64>("amount_cents", pool).await.map_err(internal)?,
        "shortcut_avg": Promotion::avg::<f64>("amount_cents", pool).await.map_err(internal)?,
        "scoped_avg": qs.avg::<f64>("amount_cents", pool).await.map_err(internal)?,
        // Both must touch nothing: every target is hidden.
        "destroy_hidden": Promotion::destroy([low], pool).await.map_err(internal)?,
        "delete_where_hidden": Promotion::delete_where("code", format!("SHH-{tag}"), pool)
            .await
            .map_err(internal)?,
        "hidden_left": Promotion::objects()
            .without_global_scopes()
            .filter("code__endswith", tag.clone())
            .filter("visible", false)
            .count(pool)
            .await
            .map_err(internal)?,
        "destroy_visible": Promotion::destroy([shown], pool).await.map_err(internal)?,
    });
    // Clean up past the scope, so repeated runs do not pile up rows.
    let q = Promotion::objects()
        .without_global_scopes()
        .filter("code__endswith", tag)
        .compile_delete()
        .map_err(internal)?;
    rustango::sql::delete_pool(pool, &q).await.map_err(internal)?;
    Ok(Json(out))
}

/// An audited insert, soft delete and restore on `&Pool`; each must
/// write one audit row carrying the real PK (#1675).
pub async fn audit_probe(pool: &Pool) -> ProbeResult {
    gate()?;
    rustango::audit::ensure_table_pool(pool).await.map_err(internal)?;
    let mut row = Promotion {
        id: Auto::default(),
        code: format!("AUD-{}", nonce()),
        label: "audited".into(),
        amount_cents: 250,
        visible: true,
        deleted_at: None,
    };
    row.insert_pool(pool).await.map_err(internal)?;
    let pk = row.id.get().copied().ok_or_else(|| internal("no pk"))?;
    let deleted = row.soft_delete(pool).await.map_err(internal)?;
    let restored = row.restore(pool).await.map_err(internal)?;
    let entries = rustango::audit::fetch_for_entity_pool(pool, Promotion::SCHEMA.table, &pk.to_string())
        .await
        .map_err(internal)?;
    let ops: Vec<Value> = entries
        .iter()
        .map(|e| json!({ "operation": e.operation, "entity_pk": e.entity_pk, "changes": e.changes }))
        .collect();
    Ok(Json(json!({ "pk": pk, "soft_deleted": deleted, "restored": restored, "entries": ops })))
}

/// Three products, then a bounded delete and a bounded update (#1666).
pub async fn dml_bounded(pool: &Pool) -> ProbeResult {
    gate()?;
    let tag = format!("DML-{}", nonce());
    for i in 0..3 {
        let mut p = Product {
            id: Auto::default(),
            sku: format!("{tag}-{i}"),
            name: "bounded dml probe".into(),
            description: None,
            price_cents: 100 + i,
            active: false,
            created_at: Auto::default(),
        };
        p.insert_pool(pool).await.map_err(internal)?;
    }
    let tagged = || Product::objects().filter("sku__startswith", tag.clone());
    let updated = tagged()
        .order_by(&[("price_cents", true)])
        .limit(2)
        .update()
        .set("name", "bounded update")
        .execute_pool(pool)
        .await
        .map_err(internal)?;
    let q = tagged()
        .order_by(&[("price_cents", false)])
        .limit(1)
        .compile_delete()
        .map_err(internal)?;
    let deleted = rustango::sql::delete_pool(pool, &q).await.map_err(internal)?;
    let left: Vec<Product> = tagged()
        .order_by(&[("price_cents", false)])
        .fetch(pool)
        .await
        .map_err(internal)?;
    let renamed = left.iter().filter(|p| p.name == "bounded update").count();
    let prices: Vec<i64> = left.iter().map(|p| p.price_cents).collect();
    // Tidy up; the probe rows are inactive, so the storefront never shows them.
    if let Ok(q) = tagged().compile_delete() {
        let _ = rustango::sql::delete_pool(pool, &q).await;
    }
    Ok(Json(json!({
        "updated": updated, "deleted": deleted, "left_prices": prices, "renamed_left": renamed,
    })))
}

async fn put(tx: &rustango::sql::AtomicTx, sku: String) -> Result<(), rustango::sql::ExecError> {
    let mut row = Product {
        id: Auto::default(),
        sku,
        name: "atomic probe".into(),
        description: None,
        price_cents: 1,
        active: false,
        created_at: Auto::default(),
    };
    row.insert_tx(&mut *tx.lock().await?).await
}

fn bail() -> rustango::sql::ExecError {
    rustango::sql::ExecError::Sql(rustango::sql::SqlError::EmptyInList)
}

async fn present(pool: &Pool, sku: &str) -> Result<bool, (StatusCode, String)> {
    Ok(Product::objects()
        .filter("sku", sku.to_owned())
        .count(pool)
        .await
        .map_err(internal)?
        > 0)
}

/// Nested `atomic()` is a savepoint on the outer connection, and
/// `on_commit` waits for the outermost commit (#1666).
pub async fn dml_atomic(pool: &Pool) -> ProbeResult {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    gate()?;
    let tag = format!("ATM-{}", nonce());
    let sku = |s: &str| format!("{tag}-{s}");

    // Outer rolls back after the inner block committed: both writes go.
    let (a, b) = (sku("outer-rb-a"), sku("outer-rb-b"));
    let (a1, b1) = (a.clone(), b.clone());
    let (p1, p2) = (pool.clone(), pool.clone());
    let outer_rb: Result<(), rustango::sql::ExecError> = rustango::atomic!(&p1, |tx| {
        put(tx, a1.clone()).await?;
        rustango::atomic!(&p2, |sp| { put(sp, b1.clone()).await }).await?;
        Err(bail())
    })
    .await;

    // Inner rolls back: the outer write stays.
    let (c, d) = (sku("inner-rb-c"), sku("inner-rb-d"));
    let (c1, d1) = (c.clone(), d.clone());
    let (p3, p4) = (pool.clone(), pool.clone());
    let inner_rb: Result<(), rustango::sql::ExecError> = rustango::atomic!(&p3, |tx| {
        put(tx, c1.clone()).await?;
        let _ = rustango::atomic!(&p4, |sp| {
            put(sp, d1.clone()).await?;
            Err::<(), _>(bail())
        })
        .await;
        Ok(())
    })
    .await;

    // A callback registered in the inner block fires after the outer commit.
    let fired = Arc::new(AtomicUsize::new(0));
    let (f, seen) = (Arc::clone(&fired), Arc::clone(&fired));
    let e = sku("commit-e");
    let (p5, p6) = (pool.clone(), pool.clone());
    let fired_inside: Result<usize, rustango::sql::ExecError> = rustango::atomic!(&p5, |_tx| {
        rustango::atomic!(&p6, |sp| {
            put(sp, e.clone()).await?;
            rustango::sql::on_commit(move || {
                f.fetch_add(1, Ordering::SeqCst);
            });
            Ok(())
        })
        .await?;
        Ok(seen.load(Ordering::SeqCst))
    })
    .await;

    let out = json!({
        "outer_rollback_err": outer_rb.is_err(),
        "outer_rollback_kept_outer": present(pool, &a).await?,
        "outer_rollback_kept_inner": present(pool, &b).await?,
        "inner_rollback_ok": inner_rb.is_ok(),
        "inner_rollback_kept_outer": present(pool, &c).await?,
        "inner_rollback_kept_inner": present(pool, &d).await?,
        "on_commit_fired_before_outer_commit": fired_inside.map_err(internal)?,
        "on_commit_fired_after": fired.load(Ordering::SeqCst),
    });
    if let Ok(q) = Product::objects()
        .filter("sku__startswith", tag.clone())
        .compile_delete()
    {
        let _ = rustango::sql::delete_pool(pool, &q).await;
    }
    Ok(Json(out))
}

/// Deliver one webhook now, in this process, and say what happened
/// (#1670). The driver aims it at blocked and allowed targets.
pub async fn webhook_probe(body: Value) -> ProbeResult {
    use rustango::jobs::Job as _;
    gate()?;
    let url = body["url"].as_str().unwrap_or_default().to_owned();
    let event = rustango::webhook_delivery::WebhookEvent {
        id: nonce(),
        event: "soak.probe".into(),
        target_url: url,
        signing_secret: "soak-webhook-secret".into(),
        signature_format: rustango::webhook::SignatureFormat::HexSha256WithPrefix,
        payload: json!({ "probe": true }),
        headers: HashMap::new(),
        timeout_secs: 3,
        retry_status_codes: Vec::new(),
        allow_private_targets: body["allow_private"].as_bool().unwrap_or(false),
    };
    Ok(Json(match event.run().await {
        Ok(()) => json!({ "delivered": true }),
        Err(e) => json!({ "delivered": false, "error": e.to_string() }),
    }))
}

fn sink() -> &'static Mutex<HashMap<String, u64>> {
    static SINK: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Loopback receiver: a blocked delivery must never land here.
pub async fn hook_sink_hit(nonce: String) -> ProbeResult {
    gate()?;
    let mut map = sink().lock().map_err(internal)?;
    if map.len() > 10_000 {
        map.clear();
    }
    *map.entry(nonce).or_default() += 1;
    Ok(Json(json!({ "ok": true })))
}

pub async fn hook_sink_count(nonce: String) -> ProbeResult {
    gate()?;
    let n = sink().lock().map_err(internal)?.get(&nonce).copied().unwrap_or(0);
    Ok(Json(json!({ "hits": n })))
}

/// Round-trip one key through a database cache table (#1674). Keys over
/// 255 bytes used to truncate on MySQL and collide.
pub async fn dbcache(pool: &Pool, body: Value) -> ProbeResult {
    use rustango::cache::Cache as _;
    gate()?;
    let cache = rustango::cache::DatabaseCache::new(pool.clone(), "soak_cache");
    cache.ensure_table().await.map_err(internal)?;
    let pairs = body["pairs"].as_array().cloned().unwrap_or_default();
    for p in &pairs {
        let (Some(k), Some(v)) = (p[0].as_str(), p[1].as_str()) else {
            continue;
        };
        if let Err(e) = cache.set(k, v, Some(std::time::Duration::from_secs(300))).await {
            return Ok(Json(json!({ "error": e.to_string() })));
        }
    }
    let mut got = Vec::new();
    for p in &pairs {
        let k = p[0].as_str().unwrap_or_default();
        got.push(cache.get(k).await.map_err(internal)?);
    }
    Ok(Json(json!({ "got": got })))
}

/// A service token checked with `jwt::decode`, which must refuse one
/// with no `exp` (#1538).
pub async fn service_token(headers: HeaderMap) -> ProbeResult {
    let secret = std::env::var("SOAK_SERVICE_TOKEN_SECRET").unwrap_or_default();
    if secret.len() < 32 {
        return Err((StatusCode::FORBIDDEN, "SOAK_SERVICE_TOKEN_SECRET is not set".into()));
    }
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    match rustango::jwt::decode(token, secret.as_bytes()) {
        Ok(c) => Ok(Json(json!({ "sub": c.subject() }))),
        Err(e) => Err((StatusCode::UNAUTHORIZED, e.to_string())),
    }
}

/// What `RealIpLayer` resolved: the claimed and the trusted client (#1673).
pub async fn client_ip(
    real: Option<axum::Extension<rustango::real_ip::RealIp>>,
    trusted: Option<axum::Extension<rustango::real_ip::TrustedRealIp>>,
) -> Json<Value> {
    Json(json!({
        "real_ip": real.map(|r| r.0 .0.to_string()),
        "trusted_ip": trusted.map(|t| t.0 .0.to_string()),
    }))
}

/// A stand-in payment: a fresh id on every real execution, so a replay
/// is visible. `"remember": true` also sets a cookie, which the
/// idempotency layer must not store (#1668).
pub async fn payment(Json(body): Json<Value>) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse as _;
    gate()?;
    let id = nonce();
    let mut resp = (
        StatusCode::CREATED,
        Json(json!({ "payment_id": id, "amount_cents": body["amount_cents"] })),
    )
        .into_response();
    if body["remember"].as_bool() == Some(true) {
        if let Ok(v) = axum::http::HeaderValue::from_str(&format!("last_payment={id}; Path=/; HttpOnly")) {
            resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
        }
    }
    Ok(resp)
}
