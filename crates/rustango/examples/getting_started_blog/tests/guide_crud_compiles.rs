//! Compile-gate for the getting-started guide's CRUD step.
//!
//! The guide told readers to open a pool with `PgPool`, then call
//! `.save(&pool)` and `.fetch_on(&pool)`. On SQLite none of that exists,
//! and the multi-backend `.fetch(&pool)` it mentioned needs the
//! `FetcherPool` trait in scope — which the guide never said (#1272,
//! #1293).
//!
//! This mirrors the snippet exactly. It never runs a query; compiling on
//! each backend is the whole assertion, which is why it takes no
//! database. `cargo test --no-default-features --features sqlite` here
//! fails if the snippet stops being true for SQLite.

use getting_started_blog::blog::models::Post;
use rustango::sql::{FetcherPool, Pool};
use rustango::Auto;

#[allow(dead_code)]
async fn guide_crud_step(pool: &Pool) -> Result<(), Box<dyn std::error::Error>> {
    // CREATE
    let mut p = Post {
        id: Auto::default(),
        title: "First post".into(),
        body: "Hello, world.".into(),
        status: "draft".into(),
        author_id: 1,
        published_at: Auto::default(),
        deleted_at: None,
    };
    p.save_pool(pool).await?;

    // READ
    let posts = Post::objects().fetch(pool).await?;
    for post in &posts {
        println!("- {}", post.title);
    }
    Ok(())
}

#[test]
fn the_guides_crud_snippet_still_compiles() {}
