//! Strict JSON for v2 files: a key written twice in one object is an error.
//! `serde_json::Value` keeps the last one silently, so a checksum would
//! cover a file whose meaning depends on the parser.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Why a text is not strict JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JsonError {
    /// Not JSON at all.
    Syntax(String),
    /// A key twice in one object, at this JSON pointer.
    Duplicate { pointer: String },
}

/// Parse `text`, refusing a duplicate key in any object.
///
/// # Errors
/// [`JsonError`].
pub(crate) fn parse(text: &str) -> Result<Value, JsonError> {
    let dup = RefCell::new(None);
    let mut de = serde_json::Deserializer::from_str(text);
    let seed = Node {
        pointer: String::new(),
        dup: &dup,
    };
    let value = seed.deserialize(&mut de).and_then(|v| de.end().map(|()| v));
    match (value, dup.into_inner()) {
        (_, Some(pointer)) => Err(JsonError::Duplicate { pointer }),
        (Ok(v), None) => Ok(v),
        (Err(e), None) => Err(JsonError::Syntax(e.to_string())),
    }
}

/// One value at `pointer`; records the first duplicate key in `dup`.
struct Node<'a> {
    pointer: String,
    dup: &'a RefCell<Option<String>>,
}

impl<'de> DeserializeSeed<'de> for Node<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Node<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(Node {
            pointer: format!("{}/{}", self.pointer, out.len()),
            dup: self.dup,
        })? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let at = format!(
                "{}/{}",
                self.pointer,
                key.replace('~', "~0").replace('/', "~1")
            );
            if !seen.insert(key.clone()) {
                *self.dup.borrow_mut() = Some(at);
                return Err(serde::de::Error::custom("duplicate key"));
            }
            let v = map.next_value_seed(Node {
                pointer: at,
                dup: self.dup,
            })?;
            out.insert(key, v);
        }
        Ok(Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duplicate_key_is_refused_with_its_pointer() {
        let text = r#"{"a": 1, "ops": [{"op": "x"}, {"op": "y", "t": 1, "t": 2}]}"#;
        assert_eq!(
            parse(text),
            Err(JsonError::Duplicate {
                pointer: "/ops/1/t".into()
            })
        );
        assert_eq!(
            parse(r#"{"a": 1, "a": 1}"#),
            Err(JsonError::Duplicate {
                pointer: "/a".into()
            })
        );
    }

    #[test]
    fn strict_parse_matches_serde_json() {
        let text = r#"{"a": [1, -2, 3.5, true, null, "s"], "b": {"c": {}}}"#;
        assert_eq!(
            parse(text).unwrap(),
            serde_json::from_str::<Value>(text).unwrap()
        );
        assert!(matches!(parse("{"), Err(JsonError::Syntax(_))));
        assert!(matches!(parse("{} x"), Err(JsonError::Syntax(_))));
    }
}
