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

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<DateTime<Utc>>,
}
