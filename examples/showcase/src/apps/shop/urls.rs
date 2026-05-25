//! Shop HTTP routes. Showcases:
//!
//! - `i64` round-trip for money fields (prices in cents — track #524
//!   for the eventual macro `Decimal` support that would replace this)
//! - Query-param filtering: `/shop/products?active=true`
//! - `Option<i64>` nullable round-trip
//! - Re-fetch-after-insert (MySQL parity, same as the blog app)

use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use axum::Router;
use rustango::core::Op;
use rustango::sql::{Auto, FetcherPool as _, Pool};

use super::models::Product;

#[cfg(feature = "postgres")]
type AttachedPool = sqlx::PgPool;
#[cfg(not(feature = "postgres"))]
type AttachedPool = Pool;

fn into_pool(p: &AttachedPool) -> Pool {
    #[cfg(feature = "postgres")]
    {
        Pool::from(p.clone())
    }
    #[cfg(not(feature = "postgres"))]
    {
        p.clone()
    }
}

#[must_use]
pub fn api() -> Router {
    Router::new()
        .route(
            "/shop/products",
            get(list_products).post(create_product),
        )
        .route("/shop/products/{id}", get(retrieve_product))
}

#[derive(serde::Serialize)]
struct ProductOut {
    id: i64,
    name: String,
    sku: String,
    price_cents: i64,
    stock: Option<i64>,
    active: bool,
}

impl From<Product> for ProductOut {
    fn from(p: Product) -> Self {
        Self {
            id: match p.id {
                Auto::Set(n) => n,
                Auto::Unset => 0,
            },
            name: p.name,
            sku: p.sku,
            price_cents: p.price_cents,
            stock: p.stock,
            active: p.active,
        }
    }
}

#[derive(serde::Deserialize)]
struct ProductIn {
    name: String,
    sku: String,
    price_cents: i64,
    #[serde(default)]
    stock: Option<i64>,
    #[serde(default = "default_true")]
    active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(serde::Deserialize)]
struct ListFilters {
    /// `?active=true` / `?active=false`. Omit to list everything.
    active: Option<bool>,
}

async fn list_products(
    Extension(pool): Extension<AttachedPool>,
    Query(filters): Query<ListFilters>,
) -> Result<Json<Vec<ProductOut>>, (StatusCode, String)> {
    let pool = into_pool(&pool);
    let mut qs = Product::objects().order_by(&[("id", false)]);
    if let Some(want_active) = filters.active {
        qs = qs.filter_op("active", Op::Eq, want_active);
    }
    let rows: Vec<Product> = qs
        .fetch_pool(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(rows.into_iter().map(ProductOut::from).collect()))
}

async fn retrieve_product(
    Extension(pool): Extension<AttachedPool>,
    Path(id): Path<i64>,
) -> Result<Json<ProductOut>, (StatusCode, String)> {
    let pool = into_pool(&pool);
    let mut rows: Vec<Product> = Product::objects()
        .filter_op("id", Op::Eq, id)
        .fetch_pool(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(p) = rows.pop() {
        Ok(Json(ProductOut::from(p)))
    } else {
        Err((StatusCode::NOT_FOUND, format!("product {id} not found")))
    }
}

async fn create_product(
    Extension(pool): Extension<AttachedPool>,
    Json(input): Json<ProductIn>,
) -> Result<(StatusCode, Json<ProductOut>), (StatusCode, String)> {
    let pool = into_pool(&pool);
    let mut p = Product {
        id: Auto::Unset,
        name: input.name,
        sku: input.sku,
        price_cents: input.price_cents,
        stock: input.stock,
        active: input.active,
    };
    p.insert_pool(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Re-fetch by PK — same MySQL parity reason as blog.
    let id = match p.id {
        Auto::Set(n) => n,
        Auto::Unset => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "insert_pool didn't populate PK".into(),
            ));
        }
    };
    let mut rows: Vec<Product> = Product::objects()
        .filter_op("id", Op::Eq, id)
        .fetch_pool(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stored = rows.pop().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "could not re-fetch inserted row".into(),
    ))?;
    Ok((StatusCode::CREATED, Json(ProductOut::from(stored))))
}
