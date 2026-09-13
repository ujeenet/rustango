use rustango::ViewSet;
use crate::blog::models::Post;

// `serializer` is what the guide reaches at Step 13, and it changes the
// write path: the serializer's shape replaces the `fields` projection,
// so a field absent from it is ignored on POST. This example stopped one
// step short of that for a long time, which is why the guide's own test
// could fail while this gate stayed green (#1299).
#[derive(ViewSet)]
#[viewset(
    model         = Post,
    serializer    = crate::post_serializer::PostSerializer,
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;
