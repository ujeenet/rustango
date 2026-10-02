mod blog;
mod models;
mod post_serializer;
mod post_view_set;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    // The multi-backend pool: one line that works on all three, instead
    // of a driver-specific type that only exists under one feature.
    let pool = rustango::sql::Pool::connect(&std::env::var("DATABASE_URL")?).await?;

    let api = urls::api()
        .nest("/admin", urls::admin_router(pool.clone()))
        .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool));

    rustango::manage::Cli::new()
        .api(api)
        .with_health() // /health + /ready endpoints
        .run()
        .await
}
