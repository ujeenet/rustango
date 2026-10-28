#![cfg(feature = "serializer")]
//! `#[derive(Serializer)]` honours the `#[serde(...)]` field and container
//! attributes that change the wire shape. No database required.

use rustango::serializer::ModelSerializer;
use rustango::Serializer;

#[derive(rustango::Model, Clone)]
#[rustango(table = "serde_attr_users")]
pub struct Account {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    pub display_name: String,
    pub password_hash: String,
    pub nickname: Option<String>,
    pub login_count: i64,
}

fn account() -> Account {
    Account {
        id: rustango::sql::Auto::Set(1),
        display_name: "Ann".into(),
        password_hash: "argon2$secret".into(),
        nickname: None,
        login_count: 3,
    }
}

#[allow(dead_code)]
#[derive(Serializer, Default)]
#[serializer(model = Account)]
struct AccountOut {
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(skip_serializing)]
    password_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    nickname: Option<String>,
    #[serde(skip)]
    login_count: i64,
}

#[test]
fn skip_serializing_keeps_the_field_out_of_the_output() {
    let v = AccountOut::from_model(&account()).to_value();
    assert!(v.get("password_hash").is_none(), "hash leaked: {v}");
    assert!(v.get("login_count").is_none(), "serde(skip) leaked: {v}");
}

#[test]
fn skip_serializing_if_drops_the_field_only_when_the_predicate_holds() {
    let v = AccountOut::from_model(&account()).to_value();
    assert!(v.get("nickname").is_none(), "None nickname rendered: {v}");
    let mut a = account();
    a.nickname = Some("annie".into());
    let v = AccountOut::from_model(&a).to_value();
    assert_eq!(v["nickname"], "annie");
}

#[test]
fn rename_changes_the_output_and_input_key() {
    let v = AccountOut::from_model(&account()).to_value();
    assert_eq!(v["displayName"], "Ann");
    assert!(v.get("display_name").is_none());
    assert!(AccountOut::writable_fields().contains(&"displayName"));
    assert!(!AccountOut::writable_fields().contains(&"display_name"));
    // The model column keeps its own name.
    assert!(AccountOut::writable_source_fields().contains(&"display_name"));
    let parsed = AccountOut::from_writable_json(&serde_json::json!({"displayName": "Bo"})).unwrap();
    assert_eq!(parsed.display_name, "Bo");
}

#[test]
fn skip_serializing_stays_writable_but_skip_does_not() {
    let w = AccountOut::writable_fields();
    assert!(w.contains(&"password_hash"));
    assert!(!w.contains(&"login_count"));
    assert!(!AccountOut::readable_source_fields().contains(&"password_hash"));
}

#[derive(Serializer, Default)]
#[serializer(model = Account)]
#[serde(rename_all = "camelCase")]
struct AccountCamel {
    display_name: String,
    #[serde(rename = "logins")]
    login_count: i64,
    #[serde(skip_deserializing)]
    nickname: Option<String>,
}

#[test]
fn rename_all_applies_to_every_field_and_rename_wins() {
    let v = AccountCamel::from_model(&account()).to_value();
    assert_eq!(v["displayName"], "Ann");
    assert_eq!(v["logins"], 3);
    assert!(v.get("nickname").is_some());
    let w = AccountCamel::writable_fields();
    assert_eq!(w, &["displayName", "logins"]);
}
