//! `#[rustango(schema = "…")]` against a real MySQL server (#2820): a model
//! on a table in a second database, reached through a pool whose default
//! database is another one.
//!
//! A decoy table with the same bare name sits in the pool's own database,
//! holding different rows. Every assertion that reads a name back would
//! read the decoy's if a query lost its qualifier, so the suite fails
//! loudly rather than passing against the wrong table.
//!
//! Activated by `MYSQL_TEST_URL` (e.g.
//! `mysql://rustango:rustango@127.0.0.1:3406/rustango_test`). Unset is a
//! skip; set-but-unreachable panics (#1440). The user needs `CREATE` on
//! `*.*` for the second database, which `docker/mysql-init` and CI grant.
//!
//!   docker compose up -d mysql
//!   export MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/rustango_test
//!   cargo test -p rustango --features mysql --test schema_qualified_mysql_live

#![cfg(feature = "mysql")]

use rustango::core::{Column as _, Model as _};
use rustango::sql::{sqlx, Auto, ExecError, FetcherPool as _, ForeignKey, Pool};
use rustango::Model;

/// The second database. Created by the suite, never dropped: the tables
/// in it are rebuilt on every run.
const OTHER_DB: &str = "rustango_qualified_test";

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "qm_kinds",
    schema = "rustango_qualified_test",
    managed = false
)]
pub struct Kind {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "qm_items", managed = false)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub title: String,
    pub kind: ForeignKey<Kind>,
}

async fn pool_or_skip() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let p = sqlx::MySqlPool::connect(&url)
        .await
        .expect("connect to MYSQL_TEST_URL");
    let kind_col = Item::SCHEMA.field("kind").expect("kind field").column;
    for sql in [
        format!("CREATE DATABASE IF NOT EXISTS `{OTHER_DB}`"),
        format!("DROP TABLE IF EXISTS `{OTHER_DB}`.`qm_kinds`"),
        "DROP TABLE IF EXISTS `qm_items`".to_owned(),
        "DROP TABLE IF EXISTS `qm_kinds`".to_owned(),
        format!(
            "CREATE TABLE `{OTHER_DB}`.`qm_kinds` \
             (id BIGINT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) NOT NULL)"
        ),
        // The decoy: same bare name, the pool's own database, other rows.
        "CREATE TABLE `qm_kinds` \
         (id BIGINT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) NOT NULL)"
            .to_owned(),
        "INSERT INTO `qm_kinds` (id, name) VALUES (1, 'decoy-1'), (2, 'decoy-2'), (3, 'decoy-3')"
            .to_owned(),
        format!(
            "CREATE TABLE `qm_items` (id BIGINT AUTO_INCREMENT PRIMARY KEY, \
             title VARCHAR(50) NOT NULL, `{kind_col}` BIGINT NOT NULL)"
        ),
    ] {
        sqlx::query(&sql)
            .execute(&p)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    Some(Pool::Mysql(p))
}

/// The real table's names, read without the ORM.
#[allow(irrefutable_let_patterns)] // `Pool` is one variant in a mysql-only build
async fn other_names(pool: &Pool) -> Vec<String> {
    let Pool::Mysql(p) = pool else { unreachable!() };
    sqlx::query_scalar(&format!(
        "SELECT name FROM `{OTHER_DB}`.`qm_kinds` ORDER BY id"
    ))
    .fetch_all(p)
    .await
    .unwrap()
}

#[allow(irrefutable_let_patterns)]
async fn decoy_names(pool: &Pool) -> Vec<String> {
    let Pool::Mysql(p) = pool else { unreachable!() };
    sqlx::query_scalar("SELECT name FROM `qm_kinds` ORDER BY id")
        .fetch_all(p)
        .await
        .unwrap()
}

/// One test, in order: the steps share the tables, and splitting them
/// would need a suite lock for no extra coverage.
#[tokio::test]
async fn crud_join_and_refusals_hit_the_qualified_table() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset; skipping");
        return;
    };
    let decoy = vec![
        "decoy-1".to_owned(),
        "decoy-2".to_owned(),
        "decoy-3".to_owned(),
    ];

    // INSERT, and the generated key read back from the right table.
    let mut person = Kind {
        id: Auto::default(),
        name: "person".into(),
    };
    person.save(&pool).await.unwrap();
    let mut org = Kind {
        id: Auto::default(),
        name: "org".into(),
    };
    org.save(&pool).await.unwrap();
    let person_id = person.id.require().unwrap();
    let org_id = org.id.require().unwrap();
    assert_eq!(other_names(&pool).await, ["person", "org"]);

    // SELECT: a filter, a full read, a count.
    let found: Vec<Kind> = Kind::objects()
        .where_(Kind::name.eq("org"))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id.require().unwrap(), org_id);
    let all: Vec<Kind> = Kind::objects().fetch(&pool).await.unwrap();
    let names: Vec<_> = all.iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names, ["person", "org"]);

    // UPDATE, per instance and per queryset.
    person.name = "individual".into();
    person.save(&pool).await.unwrap();
    Kind::objects()
        .where_(Kind::name.eq("org"))
        .update()
        .set("name", "organization")
        .execute_on(&pool)
        .await
        .unwrap();
    assert_eq!(other_names(&pool).await, ["individual", "organization"]);

    // JOIN from the pool's database into the other one. The decoy has
    // rows under the same ids, so a bare JOIN would load `decoy-*`.
    let mut item = Item {
        id: Auto::default(),
        title: "first".into(),
        kind: ForeignKey::unloaded(org_id),
    };
    item.save(&pool).await.unwrap();
    let items: Vec<Item> = Item::objects()
        .select_related("kind")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].kind.value().map(|k| k.name.as_str()),
        Some("organization")
    );

    #[cfg(any(feature = "admin", feature = "tenancy"))]
    viewset_reaches_the_qualified_table(&pool, org_id).await;

    // DELETE, per queryset and per instance.
    Kind::objects()
        .where_(Kind::name.eq("organization"))
        .delete_on(&pool)
        .await
        .unwrap();
    assert_eq!(other_names(&pool).await, ["individual"]);
    let survivor = Kind::objects()
        .where_(Kind::id.eq(person_id))
        .fetch(&pool)
        .await
        .unwrap()
        .remove(0);
    survivor.delete(&pool).await.unwrap();
    assert!(other_names(&pool).await.is_empty());

    // Truncate cannot name the table yet: refused, and the same-named
    // table in the pool's database keeps its rows.
    let err = Kind::truncate(&pool).await.unwrap_err();
    assert!(
        matches!(
            err,
            ExecError::QualifiedTableUnsupported {
                operation: "truncate",
                ..
            }
        ),
        "{err:?}"
    );
    let err = Kind::truncate_cascade(&pool).await.unwrap_err();
    assert!(
        matches!(
            err,
            ExecError::QualifiedTableUnsupported {
                operation: "truncate_cascade",
                ..
            }
        ),
        "{err:?}"
    );

    // Nothing above touched the decoy.
    assert_eq!(decoy_names(&pool).await, decoy);
}

/// A `ViewSet` over the qualified model: create, list, retrieve, delete over
/// HTTP, each checked against the real table (the decoy has ids 1-3 too).
#[cfg(any(feature = "admin", feature = "tenancy"))]
async fn viewset_reaches_the_qualified_table(pool: &Pool, org_id: i64) {
    use axum::body::{to_bytes, Body};
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt as _;

    async fn call(
        app: axum::Router,
        method: Method,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    let app = || {
        rustango::viewset::ViewSet::for_model(Kind::SCHEMA, &["id", "name"])
            .allow_anonymous()
            .router("/kinds", pool.clone())
    };

    let (status, created) = call(app(), Method::POST, "/kinds", r#"{"name":"via-viewset"}"#).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let new_id = created["id"].as_i64().expect("created id");
    assert_eq!(
        other_names(pool).await,
        ["individual", "organization", "via-viewset"]
    );

    let (status, list) = call(app(), Method::GET, "/kinds?ordering=id", "").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["count"], 3, "{list}");
    let names: Vec<_> = list["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["individual", "organization", "via-viewset"]);

    let (status, one) = call(app(), Method::GET, &format!("/kinds/{org_id}"), "").await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["name"], "organization");

    let (status, _) = call(app(), Method::DELETE, &format!("/kinds/{new_id}"), "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(other_names(pool).await, ["individual", "organization"]);
}
