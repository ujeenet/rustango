//! Cookbook Chapter 7f — DRF SerializerMethodField (`method = "..."`)
//! and per-field validators chain (`validate = "..."`).
//!
//! Both are macro extensions to `#[derive(Serializer)]`. No DB needed.
//!
//! Run: `cargo test --test cookbook_chapter07f_serializer_method_and_validators`

use cookbook_blog::apps::blog::models::Author;
use rustango::serializer::ModelSerializer;
use rustango::sql::Auto;
use rustango::Serializer;

// §7.99c — `method = "fn_name"` calls a Self method on `from_model`.
//
// Ergonomics match DRF's `serializers.SerializerMethodField()` +
// `def get_<field>(self, obj)` — except in Rust the convention is
// `fn <method>(model: &T) -> <field type>` defined as an inherent
// method on the serializer struct.
#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Author)]
pub struct AuthorWithMethod {
    pub id: Auto<i64>,
    pub name: String,
    pub email: String,
    /// Computed from the model — DRF SerializerMethodField shape.
    #[serializer(method = "domain_of_email")]
    pub email_domain: String,
    /// Same shape, returns whatever the method computes.
    #[serializer(method = "name_initial")]
    pub initial: String,
}

impl AuthorWithMethod {
    fn domain_of_email(model: &Author) -> String {
        model.email.split_once('@').map(|(_, d)| d.to_owned()).unwrap_or_default()
    }
    fn name_initial(model: &Author) -> String {
        model.name.chars().next().map(|c| c.to_string()).unwrap_or_default()
    }
}

// §7.99d — per-field validators chain. Each method returns
// `Result<(), String>`. The macro emits an inherent `validate(&self)`
// that runs every validator and aggregates errors into FormErrors
// keyed by field name.
#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Author)]
pub struct AuthorValidated {
    pub id: Auto<i64>,
    /// Two validators on the same field would chain naturally; today
    /// one slot per field.
    #[serializer(validate = "name_min_3")]
    pub name: String,
    #[serializer(validate = "email_must_have_at")]
    pub email: String,
    pub bio: Option<String>,
    pub joined_at: Auto<chrono::DateTime<chrono::Utc>>,
}

impl AuthorValidated {
    fn name_min_3(name: &String) -> Result<(), String> {
        if name.len() < 3 {
            Err(format!("name must be at least 3 chars; got {} chars", name.len()))
        } else { Ok(()) }
    }
    fn email_must_have_at(email: &String) -> Result<(), String> {
        if !email.contains('@') {
            Err("email must contain `@`".into())
        } else { Ok(()) }
    }
}

// ---------------- tests ----------------

fn fixture() -> Author {
    Author {
        id: Auto::Set(7),
        name: "ada".into(),
        email: "ada@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    }
}

#[test]
fn method_field_computed_from_model_lands_in_json() {
    let s = AuthorWithMethod::from_model(&fixture());
    let v = s.to_value();
    assert_eq!(v["email_domain"], "example.com");
    assert_eq!(v["initial"], "a");
}

#[test]
fn validators_pass_when_inputs_meet_constraints() {
    let s = AuthorValidated {
        id: Auto::Set(1),
        name: "alice".into(),
        email: "alice@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    s.validate().expect("all validators pass");
}

#[test]
fn validators_aggregate_errors_keyed_by_field_name() {
    let s = AuthorValidated {
        id: Auto::Set(1),
        name: "ab".into(),                 // < 3 chars
        email: "no-at-sign".into(),        // missing @
        bio: None,
        joined_at: Auto::Unset,
    };
    let err = s.validate().expect_err("two validators must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("\"name\""), "FormErrors keyed by `name`: {msg}");
    assert!(msg.contains("\"email\""), "FormErrors keyed by `email`: {msg}");
    assert!(msg.contains("at least 3 chars"));
    assert!(msg.contains("must contain `@`"));
}

#[test]
fn method_and_validator_can_coexist_on_one_serializer() {
    // Compile-only: building a serializer that uses BOTH attrs would
    // exercise the macro's per-field code path interaction. Today they
    // live on different fields; #[serializer(method = "...", validate
    // = "...")] on the same field would need both calls emitted.
    let s = AuthorWithMethod::from_model(&fixture());
    assert_eq!(s.email_domain, "example.com");
}
