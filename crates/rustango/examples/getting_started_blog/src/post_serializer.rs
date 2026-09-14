use rustango::{Auto, Serializer};
use crate::blog::models::Post;
use chrono::{DateTime, Utc};

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,

    #[serializer(source = "body")]                      // rename in API
    pub content: String,

    // `Post.author_id` is NOT NULL with no default, so it has to be
    // writable here — a field the serializer omits is dropped on POST,
    // and the INSERT fails on the missing column (#1299).
    pub author_id: i64,

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<DateTime<Utc>>,
}
