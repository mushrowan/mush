use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldStatus {
    Consumed(&'static str),
    Ignored(&'static str),
}

impl FieldStatus {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Consumed(n) | Self::Ignored(n) => n,
        }
    }
}

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
        let value = json!({"a": 1, "b": 2, "z": 99});
        let manifest = &[FieldStatus::Consumed("a"), FieldStatus::Consumed("b")];
        assert_eq!(unknown_fields(&value, manifest), vec!["z".to_string()]);
    }

    #[test]
    fn unknown_fields_treats_ignored_entries_as_acknowledged() {
        let value = json!({"a": 1, "deprecated": "old"});
        let manifest = &[
            FieldStatus::Consumed("a"),
            FieldStatus::Ignored("deprecated"),
        ];
        assert!(unknown_fields(&value, manifest).is_empty());
    }

    #[test]
    fn unknown_fields_returns_empty_for_non_objects() {
        assert!(unknown_fields(&json!(null), &[]).is_empty());
        assert!(unknown_fields(&json!([1, 2, 3]), &[]).is_empty());
        assert!(unknown_fields(&json!("scalar"), &[]).is_empty());
        assert!(unknown_fields(&json!(42), &[]).is_empty());
    }

    #[test]
    fn missing_consumed_fields_flags_consumed_entries_absent_from_response() {
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
