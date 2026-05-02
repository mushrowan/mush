//! drift detection harness for openrouter responses.
//!
//! the openrouter SDK and our own type definitions both decide which
//! fields to surface from each endpoint's JSON response. anything not
//! surfaced is silently dropped, so when openrouter ships a new field
//! (cache pricing, billing details, supported parameters) we never
//! find out without manual inspection.
//!
//! the helpers here let an integration test compare a live response
//! against a hand-curated **field manifest** and report any keys not
//! on the manifest. the manifest is three-state: each entry is either
//! [`FieldStatus::Consumed`] (we read this field and act on it) or
//! [`FieldStatus::Ignored`] (we know it exists, deliberately don't
//! surface it). a field present in the response and absent from the
//! manifest is **unknown** — the test fails so a human decides
//! between consumed and ignored before merging.

use std::collections::BTreeSet;

/// per-field acknowledgement entry in a manifest. `Consumed` means
/// the corresponding endpoint wrapper deserialises and uses the field.
/// `Ignored` means we know it exists but deliberately don't surface
/// it (e.g. legacy fields, fields for features we've decided not to
/// expose). a field present in the response that matches neither is
/// flagged as unknown by [`unknown_fields`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldStatus {
    Consumed(&'static str),
    Ignored(&'static str),
}

impl FieldStatus {
    /// the field name regardless of acknowledgement kind
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Consumed(n) | Self::Ignored(n) => n,
        }
    }
}

/// names in `value` not present in the manifest. `value` must be a
/// JSON object; arrays / scalars / null all return empty since they
/// have no top-level fields to inspect. result is sorted for stable
/// error messages
#[must_use]
pub fn unknown_fields(value: &serde_json::Value, manifest: &[FieldStatus]) -> Vec<String> {
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    let known: BTreeSet<&str> = manifest.iter().map(FieldStatus::name).collect();
    obj.keys()
        .filter(|k| !known.contains(k.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `Consumed` field names declared in the manifest but missing from
/// `value`. catches the inverse drift: openrouter removes a field we
/// were depending on. `Ignored` entries are not flagged here since
/// we already opted out of relying on them. result is sorted for
/// stable error messages
#[must_use]
pub fn missing_consumed_fields(value: &serde_json::Value, manifest: &[FieldStatus]) -> Vec<String> {
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    let present: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
    manifest
        .iter()
        .filter_map(|s| match s {
            FieldStatus::Consumed(n) if !present.contains(n) => Some((*n).to_owned()),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// fail the test with a structured message when `value` has fields
/// the manifest doesn't acknowledge or when consumed fields are
/// absent from the response. intended as the assertion call inside
/// live-API drift tests; no-op on clean responses
#[track_caller]
pub fn assert_no_drift(label: &str, value: &serde_json::Value, manifest: &[FieldStatus]) {
    let unknown = unknown_fields(value, manifest);
    let missing = missing_consumed_fields(value, manifest);
    if unknown.is_empty() && missing.is_empty() {
        return;
    }
    let mut report = format!("openrouter drift in {label}:");
    if !unknown.is_empty() {
        report.push_str(&format!(
            "\n  unknown fields (decide Consumed or Ignored): {unknown:?}"
        ));
    }
    if !missing.is_empty() {
        report.push_str(&format!(
            "\n  consumed fields missing from response: {missing:?}"
        ));
    }
    panic!("{report}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_fields_returns_keys_not_on_manifest() {
        // boundary: response has three keys, manifest acknowledges two,
        // the third is the drift signal. result is sorted so error
        // messages don't churn between runs
        let value = json!({"a": 1, "b": 2, "z": 99});
        let manifest = &[FieldStatus::Consumed("a"), FieldStatus::Consumed("b")];
        assert_eq!(unknown_fields(&value, manifest), vec!["z".to_string()]);
    }

    #[test]
    fn unknown_fields_treats_ignored_entries_as_acknowledged() {
        // Ignored is a real ack — fields marked ignored shouldn't show
        // up as drift even though we don't read them
        let value = json!({"a": 1, "deprecated": "old"});
        let manifest = &[
            FieldStatus::Consumed("a"),
            FieldStatus::Ignored("deprecated"),
        ];
        assert!(unknown_fields(&value, manifest).is_empty());
    }

    #[test]
    fn unknown_fields_returns_empty_for_non_objects() {
        // the manifest pattern only applies to JSON objects. arrays,
        // scalars, and null have no top-level fields; report empty so
        // callers can blanket-call the helper without pre-checking
        assert!(unknown_fields(&json!(null), &[]).is_empty());
        assert!(unknown_fields(&json!([1, 2, 3]), &[]).is_empty());
        assert!(unknown_fields(&json!("scalar"), &[]).is_empty());
        assert!(unknown_fields(&json!(42), &[]).is_empty());
    }

    #[test]
    fn missing_consumed_fields_flags_consumed_entries_absent_from_response() {
        // inverse drift: openrouter removed a field we depend on. only
        // Consumed entries flag as missing; Ignored ones already opted
        // out so we don't care if they vanish
        let value = json!({"a": 1});
        let manifest = &[
            FieldStatus::Consumed("a"),
            FieldStatus::Consumed("b"),
            FieldStatus::Ignored("c"),
        ];
        assert_eq!(
            missing_consumed_fields(&value, manifest),
            vec!["b".to_string()]
        );
    }

    #[test]
    fn missing_consumed_fields_returns_empty_when_all_consumed_present() {
        let value = json!({"a": 1, "b": 2, "extra": 3});
        let manifest = &[FieldStatus::Consumed("a"), FieldStatus::Consumed("b")];
        assert!(missing_consumed_fields(&value, manifest).is_empty());
    }

    #[test]
    fn assert_no_drift_passes_silently_on_clean_response() {
        let value = json!({"a": 1, "b": 2});
        let manifest = &[FieldStatus::Consumed("a"), FieldStatus::Consumed("b")];
        assert_no_drift("clean", &value, manifest);
    }

    #[test]
    #[should_panic(expected = "unknown fields")]
    fn assert_no_drift_panics_on_unknown_fields() {
        let value = json!({"a": 1, "z": 2});
        let manifest = &[FieldStatus::Consumed("a")];
        assert_no_drift("test endpoint", &value, manifest);
    }

    #[test]
    #[should_panic(expected = "consumed fields missing")]
    fn assert_no_drift_panics_on_missing_consumed_fields() {
        let value = json!({"a": 1});
        let manifest = &[FieldStatus::Consumed("a"), FieldStatus::Consumed("missing")];
        assert_no_drift("test endpoint", &value, manifest);
    }

    #[test]
    fn field_status_name_extracts_underlying_str_from_either_variant() {
        assert_eq!(FieldStatus::Consumed("foo").name(), "foo");
        assert_eq!(FieldStatus::Ignored("bar").name(), "bar");
    }
}
