//! Django-parity #337 — `GenericIPAddressField` equivalent via
//! `#[rustango(validators = "ip_address")]`. The validator accepts
//! either IPv4 or IPv6 strings; the alias `genericipaddress`
//! exists for callers translating verbatim from a Django field.
//!
//! IPv4-only / IPv6-only protocols are covered by the existing
//! `validate_ipv4` / `validate_ipv6` validators that shipped with #447.

use rustango::core::{validate_value, FieldSchema, Model, QueryError, SqlValue};
use rustango_macros::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "macro_ipaddr_host")]
#[allow(dead_code)]
pub struct Host {
    #[rustango(primary_key)]
    pub id: i64,

    /// Both families — Django's default `GenericIPAddressField`.
    #[rustango(max_length = 45, validators = "ip_address")]
    pub addr: String,

    /// IPv4 only.
    #[rustango(max_length = 15, validators = "ipv4")]
    pub v4_only: String,

    /// IPv6 only.
    #[rustango(max_length = 45, validators = "ipv6")]
    pub v6_only: String,

    /// Alias for Django-translated callers.
    #[rustango(max_length = 45, validators = "genericipaddress")]
    pub via_alias: String,
}

fn field<'a>(name: &str) -> &'a FieldSchema {
    Host::SCHEMA
        .field(name)
        .unwrap_or_else(|| panic!("no field {name:?}"))
}

#[test]
fn ip_address_validator_accepts_ipv4() {
    validate_value(
        "Host",
        field("addr"),
        &SqlValue::String("192.168.1.1".into()),
    )
    .unwrap();
}

#[test]
fn ip_address_validator_accepts_ipv6() {
    validate_value(
        "Host",
        field("addr"),
        &SqlValue::String("2001:db8::1".into()),
    )
    .unwrap();
    // Loopback.
    validate_value("Host", field("addr"), &SqlValue::String("::1".into())).unwrap();
}

#[test]
fn ip_address_validator_rejects_non_ip() {
    let err =
        validate_value("Host", field("addr"), &SqlValue::String("not-an-ip".into())).unwrap_err();
    match err {
        QueryError::ValidatorFailed {
            field: name,
            validator,
            ..
        } => {
            assert_eq!(name, "addr");
            assert_eq!(validator, "ip_address");
        }
        other => panic!("expected ValidatorFailed, got {other:?}"),
    }
}

#[test]
fn ipv4_validator_rejects_ipv6() {
    let err =
        validate_value("Host", field("v4_only"), &SqlValue::String("::1".into())).unwrap_err();
    match err {
        QueryError::ValidatorFailed { validator, .. } => assert_eq!(validator, "ipv4"),
        other => panic!("expected ValidatorFailed, got {other:?}"),
    }
}

#[test]
fn ipv6_validator_rejects_ipv4() {
    let err = validate_value(
        "Host",
        field("v6_only"),
        &SqlValue::String("192.168.1.1".into()),
    )
    .unwrap_err();
    match err {
        QueryError::ValidatorFailed { validator, .. } => assert_eq!(validator, "ipv6"),
        other => panic!("expected ValidatorFailed, got {other:?}"),
    }
}

#[test]
fn genericipaddress_alias_works_same_as_ip_address() {
    validate_value(
        "Host",
        field("via_alias"),
        &SqlValue::String("10.0.0.1".into()),
    )
    .unwrap();
    validate_value(
        "Host",
        field("via_alias"),
        &SqlValue::String("2001:db8::1".into()),
    )
    .unwrap();
    let err = validate_value(
        "Host",
        field("via_alias"),
        &SqlValue::String("invalid".into()),
    )
    .unwrap_err();
    match err {
        QueryError::ValidatorFailed { validator, .. } => {
            assert_eq!(validator, "genericipaddress");
        }
        other => panic!("expected ValidatorFailed, got {other:?}"),
    }
}
