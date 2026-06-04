//! Django parity — `Meta.required_db_features` lets a model declare
//! capability tokens it depends on (e.g. `"json_path"`, `"hstore"`,
//! `"listen_notify"`). `manage check --deploy` walks every model and
//! warns when the active dialect's `Dialect::supports(token)` returns
//! `false`. Finer-grained than `required_db_vendor` — composes with it.
//!
//! rustango spells the attribute as
//! `#[rustango(required_db_features = "tok1, tok2")]` on the model
//! container. Stored on `ModelSchema::required_db_features` as a
//! `&'static [&'static str]`.

use rustango::sql::Dialect;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rdf_pg_heavy",
    required_db_features = "listen_notify, gist_index, hstore"
)]
#[allow(dead_code)]
pub struct PgHeavy {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rdf_portable",
    required_db_features = "window_functions, json_extract"
)]
#[allow(dead_code)]
pub struct Portable {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rdf_plain")]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn schema_carries_capability_tokens() {
    let schema = <PgHeavy as rustango::core::Model>::SCHEMA;
    assert_eq!(
        schema.required_db_features,
        &["listen_notify", "gist_index", "hstore"]
    );
}

#[test]
fn plain_model_has_no_required_features() {
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    assert!(plain.required_db_features.is_empty());
}

#[cfg(feature = "postgres")]
#[test]
fn pg_supports_all_pg_heavy_tokens() {
    let d = rustango::sql::Postgres;
    let schema = <PgHeavy as rustango::core::Model>::SCHEMA;
    for token in schema.required_db_features {
        assert!(d.supports(token), "Postgres should advertise `{token}`");
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_pg_only_tokens_but_accepts_portable_ones() {
    let d = rustango::sql::Sqlite;
    // PG-only tokens — SQLite does not advertise.
    assert!(!d.supports("listen_notify"));
    assert!(!d.supports("gist_index"));
    assert!(!d.supports("hstore"));
    assert!(!d.supports("array_type"));
    // Portable tokens that all three backends ship.
    assert!(d.supports("window_functions"));
    assert!(d.supports("json_extract"));
    assert!(d.supports("recursive_cte"));
}

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_pg_only_and_partial_index() {
    let d = rustango::sql::MySql;
    assert!(!d.supports("listen_notify"));
    assert!(!d.supports("array_type"));
    assert!(!d.supports("hstore"));
    // MySQL has no native partial-index support.
    assert!(!d.supports("partial_index"));
    // But window functions + CTEs + JSON_EXTRACT all ship (MySQL 8+).
    assert!(d.supports("window_functions"));
    assert!(d.supports("json_extract"));
    assert!(d.supports("recursive_cte"));
}

#[test]
fn unknown_capability_returns_false_on_every_dialect() {
    #[cfg(feature = "postgres")]
    assert!(!rustango::sql::Postgres.supports("not_a_real_capability_xyz"));
    #[cfg(feature = "sqlite")]
    assert!(!rustango::sql::Sqlite.supports("not_a_real_capability_xyz"));
    #[cfg(feature = "mysql")]
    assert!(!rustango::sql::MySql.supports("not_a_real_capability_xyz"));
}
