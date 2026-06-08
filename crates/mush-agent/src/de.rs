//! lenient deserialisation helpers for tool arguments.
//!
//! tool-calling models routinely emit integer arguments as JSON strings
//! (e.g. `"1030"` instead of `1030`), especially for fields the advertised
//! schema doesn't type as integers. strict serde parsing rejects those with
//! `invalid type: string, expected usize`, which surfaces to the model as a
//! confusing failure it tends to repeat. these helpers accept either a JSON
//! number or a numeric string so the call still succeeds.
//!
//! use via `#[serde(deserialize_with = "...")]` on unsigned-integer fields.
//! pair [`opt_uint`] with `#[serde(default)]` so an absent key stays `None`.

use std::fmt::Display;

use serde::Deserialize;
use serde::de::{Deserializer, Error};

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
}
