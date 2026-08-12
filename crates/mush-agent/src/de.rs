//! lenient deserialisation helpers for tool arguments.
//!
//! tool-calling models routinely emit arguments as JSON strings: an integer
//! as `"1030"`, a bool as `"true"`, an array as `"[{...}]"`. strict serde
//! parsing rejects those with `invalid type: string`, which surfaces to the
//! model as a confusing failure it tends to repeat.
//!
//! [`coerce_to_schema`] is the general answer: every tool advertises a JSON
//! schema, so the types the model was told about are the types to coerce to.
//! it runs on every tool call.
//!
//! the rest are per-field escape hatches for the same problem, via
//! `#[serde(deserialize_with = "...")]`. pair [`opt_uint`] and [`json_seq`]
//! with `#[serde(default)]` so an absent key stays empty.

use std::fmt::Display;

use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer, Error};

#[derive(Deserialize)]
#[serde(untagged)]
enum NumberOrString {
    Number(u64),
    String(String),
}

fn to_u64<E: Error>(value: NumberOrString) -> Result<u64, E> {
    match value {
        NumberOrString::Number(n) => Ok(n),
        NumberOrString::String(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| E::custom(format!("expected an unsigned integer, got string {s:?}"))),
    }
}

fn convert<T, E>(n: u64) -> Result<T, E>
where
    T: TryFrom<u64>,
    <T as TryFrom<u64>>::Error: Display,
    E: Error,
{
    T::try_from(n).map_err(E::custom)
}

/// deserialize an unsigned integer that may arrive as a JSON number or a
/// numeric string.
pub fn uint<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64>,
    <T as TryFrom<u64>>::Error: Display,
{
    let value = NumberOrString::deserialize(deserializer)?;
    convert(to_u64(value)?)
}

/// deserialize an optional unsigned integer that may arrive as a JSON
/// number, a numeric string, or null. pair with `#[serde(default)]` so an
/// absent key becomes `None`.
pub fn opt_uint<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64>,
    <T as TryFrom<u64>>::Error: Display,
{
    match Option::<NumberOrString>::deserialize(deserializer)? {
        None => Ok(None),
        Some(value) => Ok(Some(convert(to_u64(value)?)?)),
    }
}

/// deserialize an array that may arrive as a JSON string holding one.
/// pair with `#[serde(default)]` so an absent key becomes an empty vec.
pub fn json_seq<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => serde_json::from_str(&s)
            .map_err(|e| D::Error::custom(format!("expected an array, got string {s:?}: {e}")))?,
        other => other,
    };
    serde_json::from_value(value).map_err(D::Error::custom)
}

/// take the structured value out of a JSON string spelling of it
fn unstring(args: &mut serde_json::Value) {
    if let serde_json::Value::String(text) = args
        && let Ok(parsed) = serde_json::from_str(text)
    {
        *args = parsed;
    }
}

/// rewrite `args` in place to the types `schema` advertises.
///
/// anything that won't coerce is left where it is, so the parse error the
/// caller raises describes what the model sent
pub fn coerce_to_schema(args: &mut serde_json::Value, schema: &serde_json::Value) {
    use serde_json::Value;

    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            unstring(args);
            let Some(fields) = args.as_object_mut() else {
                return;
            };
            for (key, property) in properties {
                if let Some(value) = fields.get_mut(key) {
                    coerce_to_schema(value, property);
                }
            }
        }
        Some("array") => {
            unstring(args);
            let (Some(items), Some(entries)) = (schema.get("items"), args.as_array_mut()) else {
                return;
            };
            for entry in entries {
                coerce_to_schema(entry, items);
            }
        }
        Some("integer" | "number") => {
            if let Value::String(text) = args
                && let Ok(number) = text.trim().parse::<serde_json::Number>()
            {
                *args = Value::Number(number);
            }
        }
        Some("boolean") => match args.as_str().map(str::trim) {
            Some("true") => *args = Value::Bool(true),
            Some("false") => *args = Value::Bool(false),
            _ => {}
        },
        Some("string") if args.is_number() || args.is_boolean() => {
            *args = Value::String(args.to_string());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Sample {
        #[serde(default, deserialize_with = "opt_uint")]
        opt: Option<usize>,
        #[serde(default, deserialize_with = "uint")]
        plain: u64,
    }

    fn parse(value: serde_json::Value) -> Sample {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn accepts_json_numbers() {
        let s = parse(serde_json::json!({ "opt": 5, "plain": 7 }));
        assert_eq!(s.opt, Some(5));
        assert_eq!(s.plain, 7);
    }

    #[test]
    fn accepts_numeric_strings() {
        let s = parse(serde_json::json!({ "opt": "5", "plain": "7" }));
        assert_eq!(s.opt, Some(5));
        assert_eq!(s.plain, 7);
    }

    #[test]
    fn tolerates_surrounding_whitespace_in_strings() {
        let s = parse(serde_json::json!({ "opt": " 42 " }));
        assert_eq!(s.opt, Some(42));
    }

    #[test]
    fn absent_and_null_become_defaults() {
        let absent = parse(serde_json::json!({}));
        assert_eq!(absent.opt, None);
        assert_eq!(absent.plain, 0);

        let null = parse(serde_json::json!({ "opt": null }));
        assert_eq!(null.opt, None);
    }

    #[test]
    fn rejects_non_numeric_strings() {
        let err = serde_json::from_value::<Sample>(serde_json::json!({ "opt": "abc" }));
        assert!(err.is_err());
    }

    #[derive(Deserialize, Debug, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Item {
        name: String,
    }

    #[derive(Deserialize, Debug)]
    struct Listed {
        #[serde(default, deserialize_with = "json_seq")]
        items: Vec<Item>,
    }

    #[test]
    fn accepts_a_real_array() {
        let listed: Listed =
            serde_json::from_value(serde_json::json!({ "items": [{ "name": "a" }] })).unwrap();
        assert_eq!(listed.items, vec![Item { name: "a".into() }]);
    }

    #[test]
    fn accepts_an_array_sent_as_a_json_string() {
        let listed: Listed =
            serde_json::from_value(serde_json::json!({ "items": r#"[{"name":"a"}]"# })).unwrap();
        assert_eq!(listed.items, vec![Item { name: "a".into() }]);
    }

    #[test]
    fn absent_seq_is_empty() {
        let listed: Listed = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(listed.items.is_empty());
    }

    #[test]
    fn a_bad_entry_in_a_real_array_names_the_field() {
        let err =
            serde_json::from_value::<Listed>(serde_json::json!({ "items": [{ "nmae": "a" }] }))
                .unwrap_err()
                .to_string();
        assert!(err.contains("nmae"), "{err}");
    }

    #[test]
    fn a_string_that_isnt_an_array_says_so() {
        let err = serde_json::from_value::<Listed>(serde_json::json!({ "items": "nope" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("array"), "{err}");
    }

    #[test]
    fn a_stringified_bool_reaches_a_bool_field() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "loud": { "type": "boolean" } }
        });
        let mut args = serde_json::json!({ "loud": "true" });
        coerce_to_schema(&mut args, &schema);
        assert_eq!(args, serde_json::json!({ "loud": true }));
    }

    #[test]
    fn a_stringified_number_reaches_a_number_field() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" },
                "ratio": { "type": "number" }
            }
        });
        let mut args = serde_json::json!({ "count": "12", "ratio": "0.5" });
        coerce_to_schema(&mut args, &schema);
        assert_eq!(args, serde_json::json!({ "count": 12, "ratio": 0.5 }));
    }

    #[test]
    fn a_stringified_array_reaches_an_array_field() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "n": { "type": "integer" } }
                    }
                }
            }
        });
        let mut args = serde_json::json!({ "items": r#"[{"n":"3"}]"# });
        coerce_to_schema(&mut args, &schema);
        assert_eq!(args, serde_json::json!({ "items": [{ "n": 3 }] }));
    }

    #[test]
    fn a_number_reaches_a_string_field() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } }
        });
        let mut args = serde_json::json!({ "path": 42 });
        coerce_to_schema(&mut args, &schema);
        assert_eq!(args, serde_json::json!({ "path": "42" }));
    }

    #[test]
    fn a_value_the_schema_already_fits_is_left_alone() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "limit": { "type": "integer" }
            }
        });
        let mut args = serde_json::json!({ "pattern": "true", "limit": 3, "extra": "kept" });
        coerce_to_schema(&mut args, &schema);
        assert_eq!(
            args,
            serde_json::json!({ "pattern": "true", "limit": 3, "extra": "kept" })
        );
    }

    #[test]
    fn a_value_that_cannot_coerce_is_left_for_the_parse_error() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "count": { "type": "integer" } }
        });
        let mut args = serde_json::json!({ "count": "lots" });
        coerce_to_schema(&mut args, &schema);
        assert_eq!(args, serde_json::json!({ "count": "lots" }));
    }
}
