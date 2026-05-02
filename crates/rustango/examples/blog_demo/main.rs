mod models;
mod seed;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::server::Builder::from_env().await?
        .admin_show_only(["author", "post"])
        .migrate("crates/rustango/examples/blog_demo").await?
        .api(urls::api())
        .seed_with(seed::run).await?
        .serve("0.0.0.0:8080").await
}
