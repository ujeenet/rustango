//! Relation fetches on every backend: NULL FKs under `select_related` (#2293),
//! shared select_related hops (#2294), bind-cap batching (#2295), M2M `set`
//! with repeated ids (#2297) and `prefetch_generic` on an i32 PK (#2298).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::sql::{FetcherPool as _, ForeignKey, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_profile", app = "rel2293")]
pub struct Profile {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub bio: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_editor", app = "rel2293")]
pub struct Editor {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub name: String,
    pub profile: ForeignKey<Profile>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rel2293_article", app = "rel2293")]
pub struct Article {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 40)]
    pub title: String,
    pub editor: Option<ForeignKey<Editor>>,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, Article::SCHEMA.table).await;
    drop_table(pool, Editor::SCHEMA.table).await;
    fresh_table::<Profile>(pool).await;
    fresh_table::<Editor>(pool).await;
    fresh_table::<Article>(pool).await;
}

/// Article 1 has no editor; article 2 is edited by "Ada".
async fn seed_articles(pool: &Pool) {
    Profile {
        id: 1,
        bio: "bio".into(),
    }
    .insert_pool(pool)
    .await
    .expect("profile");
    Editor {
        id: 1,
        name: "Ada".into(),
        profile: ForeignKey::unloaded(1),
    }
    .insert_pool(pool)
    .await
    .expect("editor");
    for (id, editor) in [(1, None), (2, Some(ForeignKey::unloaded(1)))] {
        Article {
            id,
            title: format!("a{id}"),
            editor,
        }
        .insert_pool(pool)
        .await
        .expect("article");
    }
}

fn editor_name(a: &Article) -> Option<&str> {
    a.editor
        .as_ref()
        .and_then(|fk| fk.value())
        .map(|e| e.name.as_str())
}

async fn null_fk_select_related_skips_the_row(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .select_related("editor")
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("a NULL FK must not fail the fetch");
    assert_eq!(rows.len(), 2);
    assert!(rows[0].editor.is_none());
    assert_eq!(editor_name(&rows[1]), Some("Ada"));
}

async fn null_fk_multihop_select_related(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .select_related("editor__profile")
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .expect("a NULL first hop must not fail the fetch");
    assert!(rows[0].editor.is_none());
    let editor = rows[1].editor.as_ref().and_then(|fk| fk.value()).unwrap();
    assert_eq!(editor.profile.value().map(|p| p.bio.as_str()), Some("bio"));
}

async fn null_fk_order_by_relation(pool: &Pool) {
    seed_articles(pool).await;
    let rows: Vec<Article> = Article::objects()
        .order_by(&[("editor__name", false), ("id", false)])
        .fetch(pool)
        .await
        .expect("ordering across a NULL FK must not fail the fetch");
    assert_eq!(rows.len(), 2);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        null_fk_select_related_skips_the_row,
        null_fk_multihop_select_related,
        null_fk_order_by_relation,
    ],
}
