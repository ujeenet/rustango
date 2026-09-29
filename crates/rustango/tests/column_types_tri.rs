//! Column types that must round-trip the same on every backend.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::audit::{self, AuditLog};
use rustango::core::{Column as _, Model as _, SelectQuery};
use rustango::sql::{select_rows_as_json, Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::{tri_dialect_test, Model};
use uuid::Uuid;

#[derive(Model, Debug, Clone)]
#[rustango(table = "coltypes_page", app = "column_types_tri")]
pub struct Page {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub body: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "coltypes_token", app = "column_types_tri")]
pub struct Token {
    #[rustango(primary_key)]
    pub id: Uuid,
    pub parent: Option<Uuid>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "coltypes_grant", app = "column_types_tri")]
pub struct Grant {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub token: ForeignKey<Token, Uuid>,
    #[rustango(related_name = "backup_grants")]
    pub backup: Option<ForeignKey<Token, Uuid>>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "coltypes_session", app = "column_types_tri")]
pub struct Session {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: Auto<Uuid>,
    #[rustango(max_length = 32)]
    pub label: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "coltypes_key",
    app = "column_types_tri",
    audit(track = "owner, label")
)]
pub struct Key {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub owner: Uuid,
    #[rustango(max_length = 32)]
    pub label: String,
}

const ID: Uuid = uuid::uuid!("6f1c2a4e-9b7d-4c3a-8e21-0d5f4b6a7c89");
const PARENT: Uuid = uuid::uuid!("0d5f4b6a-7c89-4c3a-8e21-6f1c2a4e9b7d");

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Grant::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Page>(pool).await;
    rustango::testkit::matrix::fresh_table::<Token>(pool).await;
    rustango::testkit::matrix::fresh_table::<Grant>(pool).await;
    rustango::testkit::matrix::fresh_table::<Key>(pool).await;
    rustango::testkit::matrix::fresh_table::<Session>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    AuditLog::delete_where("entity_table", Key::SCHEMA.table, pool)
        .await
        .expect("clear audit rows");
}

async fn seed_tokens(pool: &Pool) {
    for (id, parent) in [(ID, Some(PARENT)), (PARENT, None)] {
        Token { id, parent }
            .insert_pool(pool)
            .await
            .expect("insert token");
    }
}

/// #1733: MySQL bound a Uuid as 16 raw bytes into its CHAR(36) column.
async fn uuid_fields_round_trip_through_the_orm(pool: &Pool) {
    seed_tokens(pool).await;
    let rows: Vec<Token> = Token::objects()
        .where_(Token::id.eq(ID))
        .fetch(pool)
        .await
        .expect("fetch by uuid");
    assert_eq!(rows.len(), 1, "{}", pool.dialect().name());
    assert_eq!(rows[0].id, ID);
    assert_eq!(rows[0].parent, Some(PARENT));
    let root: Vec<Token> = Token::objects()
        .where_(Token::id.eq(PARENT))
        .fetch(pool)
        .await
        .expect("fetch root");
    assert_eq!(root[0].parent, None, "{}", pool.dialect().name());
}

/// The flat reads decode through `FlatScalar`, not the model's `FromRow`.
async fn uuid_columns_pluck(pool: &Pool) {
    seed_tokens(pool).await;
    let mut pks: Vec<Uuid> = Token::objects().pks(pool).await.expect("pks");
    pks.sort();
    let mut want = vec![ID, PARENT];
    want.sort();
    assert_eq!(pks, want, "{}", pool.dialect().name());
    let parents: Vec<Option<Uuid>> = Token::objects()
        .order_by(&[("parent", false)])
        .pluck::<Option<Uuid>>("parent", pool)
        .await
        .expect("pluck parent");
    assert!(parents.contains(&None) && parents.contains(&Some(PARENT)));
    let pairs: Vec<(Uuid, Option<Uuid>)> = Token::objects()
        .where_(Token::id.eq(ID))
        .pluck_pairs::<Uuid, Option<Uuid>>("id", "parent", pool)
        .await
        .expect("pluck_pairs");
    assert_eq!(pairs, vec![(ID, Some(PARENT))]);
}

async fn auto_uuid_pk_round_trips(pool: &Pool) {
    let mut s = Session {
        id: Auto::default(),
        label: "s".into(),
    };
    s.insert_pool(pool).await.expect("insert session");
    let id = s.id.get().copied().expect("pk set");
    let rows: Vec<Session> = Session::objects().fetch(pool).await.expect("fetch");
    assert_eq!(
        rows[0].id.get().copied(),
        Some(id),
        "{}",
        pool.dialect().name()
    );
}

async fn uuid_fk_loads_through_select_related(pool: &Pool) {
    seed_tokens(pool).await;
    let mut grant = Grant {
        id: Auto::default(),
        token: ForeignKey::unloaded(ID),
        backup: Some(ForeignKey::unloaded(PARENT)),
    };
    grant.insert_pool(pool).await.expect("insert grant");
    let rows: Vec<Grant> = Grant::objects()
        .select_related("token")
        .fetch(pool)
        .await
        .expect("fetch grants");
    assert_eq!(rows[0].token.pk(), ID);
    assert_eq!(rows[0].backup.as_ref().map(ForeignKey::pk), Some(PARENT));
    let token = rows[0].token.value().expect("token loaded");
    assert_eq!(token.parent, Some(PARENT), "{}", pool.dialect().name());
}

async fn uuid_cells_decode_as_json(pool: &Pool) {
    seed_tokens(pool).await;
    let fields: Vec<_> = Token::SCHEMA.scalar_fields().collect();
    let mut q = SelectQuery::new(Token::SCHEMA);
    q.order_by = vec![rustango::core::OrderItem::column("parent", false)];
    let rows = select_rows_as_json(pool, &q, &fields)
        .await
        .expect("select_rows_as_json");
    let ids: Vec<&serde_json::Value> = rows.iter().map(|r| &r["id"]).collect();
    assert!(
        ids.contains(&&serde_json::json!(ID.to_string())),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r["parent"] == serde_json::json!(PARENT.to_string())),
        "{rows:?}"
    );
}

/// #1708: MySQL `TEXT` refused anything past 64 KiB.
async fn unbounded_string_holds_more_than_64_kib(pool: &Pool) {
    let body = "a".repeat(70_000);
    let mut page = Page {
        id: Auto::default(),
        body: body.clone(),
    };
    page.insert_pool(pool).await.expect("insert 70 KB body");
    let rows: Vec<Page> = Page::objects().fetch(pool).await.expect("fetch");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].body.len(), body.len(), "{}", pool.dialect().name());
}

/// The audit diff reads the old value back; an unchanged Uuid is no change.
async fn audit_diff_skips_an_unchanged_uuid(pool: &Pool) {
    let mut key = Key {
        id: Auto::default(),
        owner: ID,
        label: "a".into(),
    };
    key.insert_pool(pool).await.expect("insert key");
    key.label = "b".into();
    key.save_pool(pool).await.expect("save key");
    let pk = key.id.get().expect("pk").to_string();
    let rows = audit::fetch_for_entity_pool(pool, Key::SCHEMA.table, &pk)
        .await
        .expect("audit rows");
    let update = rows
        .iter()
        .find(|r| r.operation == "update")
        .expect("update row");
    assert!(
        update.changes.get("label").is_some(),
        "{:?}",
        update.changes
    );
    assert!(
        update.changes.get("owner").is_none(),
        "{}: {:?}",
        pool.dialect().name(),
        update.changes
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        unbounded_string_holds_more_than_64_kib,
        uuid_fields_round_trip_through_the_orm,
        uuid_fk_loads_through_select_related,
        uuid_cells_decode_as_json,
        uuid_columns_pluck,
        auto_uuid_pk_round_trips,
        audit_diff_skips_an_unchanged_uuid,
    ],
}
