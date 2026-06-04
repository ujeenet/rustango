//! Django parity — `Meta.required_db_vendor` lets a model declare
//! which DB backend it's intended to run against. `manage check
//! --deploy` flags a mismatch so ops catches "I forgot to switch
//! DATABASE_URL" at deploy time rather than the first request that
//! hits a PG-only feature on SQLite.
//!
//! rustango spells the attribute as
//! `#[rustango(required_db_vendor = "postgres|mysql|sqlite")]` on
//! the model container. Django aliases (`postgresql` / `pg` /
//! `mariadb` / `sqlite3`) accepted; macro normalizes to the canonical
//! dialect name so the check verb can compare against
//! `pool.dialect().name()` directly.

use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rdv_pg_only", required_db_vendor = "postgres")]
#[allow(dead_code)]
pub struct PgOnly {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rdv_pg_via_alias", required_db_vendor = "postgresql")]
#[allow(dead_code)]
pub struct PgViaAlias {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rdv_mysql_via_mariadb", required_db_vendor = "mariadb")]
#[allow(dead_code)]
pub struct MariaAlias {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rdv_sqlite3", required_db_vendor = "sqlite3")]
#[allow(dead_code)]
pub struct SqliteViaAlias {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "rdv_any")]
#[allow(dead_code)]
pub struct Any {
    #[rustango(primary_key)]
    pub id: i64,
}

#[test]
fn canonical_vendor_round_trips() {
    let s = <PgOnly as rustango::core::Model>::SCHEMA;
    assert_eq!(s.required_db_vendor, Some("postgres"));
}

#[test]
fn postgresql_alias_normalizes_to_postgres() {
    let s = <PgViaAlias as rustango::core::Model>::SCHEMA;
    assert_eq!(s.required_db_vendor, Some("postgres"));
}

#[test]
fn mariadb_alias_normalizes_to_mysql() {
    let s = <MariaAlias as rustango::core::Model>::SCHEMA;
    assert_eq!(s.required_db_vendor, Some("mysql"));
}

#[test]
fn sqlite3_alias_normalizes_to_sqlite() {
    let s = <SqliteViaAlias as rustango::core::Model>::SCHEMA;
    assert_eq!(s.required_db_vendor, Some("sqlite"));
}

#[test]
fn no_attribute_means_any_backend() {
    let s = <Any as rustango::core::Model>::SCHEMA;
    assert!(s.required_db_vendor.is_none());
}
