use rustango::sql::{Auto, ForeignKey};
use rustango::{Model, ViewSet};

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "author",
    display = "name",
    admin(
        list_display  = "name, bio",
        search_fields = "name, bio",
        ordering      = "name",
    ),
)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    #[rustango(max_length = 500)]
    pub bio: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "post",
    display = "title",
    admin(
        list_display  = "title, author_id, published_at",
        search_fields = "title, body",
        ordering      = "-published_at",
        list_filter   = "author_id",
    ),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 8000)]
    pub body: String,
    pub author_id: ForeignKey<Author>,
    pub published_at: chrono::DateTime<chrono::Utc>,
}

/// REST API viewset for posts — `GET/POST /api/posts` + `GET/PUT/PATCH/DELETE /api/posts/{pk}`.
#[derive(ViewSet)]
#[viewset(
    model         = Post,
    fields        = "id, title, body, author_id, published_at",
    filter_fields = "author_id",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;

/// Read-only viewset for authors.
#[derive(ViewSet)]
#[viewset(
    model         = Author,
    fields        = "id, name, bio",
    search_fields = "name, bio",
    ordering      = "name",
    read_only,
)]
pub struct AuthorViewSet;
