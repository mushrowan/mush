//! emit the nix-facing JSON Schema for mush's `Config` type
//!
//! run via `cargo run -p mush-cli --bin mush-config-schema`. the output
//! is checked in at `nix/config-schema.json` and consumed by
//! `nix/module.nix` through `nixcfg.lib.mkModule`. a flake check
//! re-runs this binary and diffs against the committed file to catch
//! drift when `Config` or any nested type changes.
//!
//! the schema follows the nixcfg extension spec (v1):
//! - `x-nixcfg-name` = "mush" (set via `#[schemars(extend(...))]` on
//!   the `Config` struct)
//! - `x-nixcfg-config-format` = "toml"
//! - `x-nixcfg-secret` on api-key fields so they turn into
//!   `nullOr path` in nix with a `_path` suffix
//!
//! defaults are merged in from `Config::default()` so nix options
//! carry the same defaults the rust side already uses

use mush_cli::config::Config;

fn main() {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(Config)).expect("Config schema serialises");
    let defaults = serde_json::to_value(Config::default()).expect("Config default serialises");

    if let serde_json::Value::Object(map) = defaults {
        merge_defaults(&mut schema, &map);
    }

    let pretty = serde_json::to_string_pretty(&schema).expect("schema serialises to json");
    println!("{pretty}");
}

/// walk a schema and set `default` on each property using the
/// serialised default values. recurses into nested object schemas.
/// fields marked `x-nixcfg-secret` are skipped so secret defaults
/// (usually `null` or an empty string) don't appear in the nix
/// module's option docs. mirrors the `NixSchema::with_defaults`
/// implementation from the nixcfg-rs crate
fn merge_defaults(
    schema: &mut serde_json::Value,
    defaults: &serde_json::Map<String, serde_json::Value>,
) {
    let obj = match schema.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let props = match obj.get_mut("properties") {
        Some(serde_json::Value::Object(p)) => p,
        _ => return,
    };

    for (key, default_val) in defaults {
        let Some(serde_json::Value::Object(prop)) = props.get_mut(key) else {
            continue;
        };

        // skip secrets (they shouldn't carry defaults)
        if prop.get("x-nixcfg-secret") == Some(&serde_json::Value::Bool(true)) {
            continue;
        }

        // recurse into nested object schemas
        if prop.get("type") == Some(&serde_json::Value::String("object".into()))
            && let serde_json::Value::Object(sub) = default_val
        {
            let mut sub_schema = serde_json::Value::Object(prop.clone());
            merge_defaults(&mut sub_schema, sub);
            if let serde_json::Value::Object(updated) = sub_schema {
                *prop = updated;
            }
            continue;
        }

        // for anyOf (Option<T> etc), recurse into the object variant if one exists
        if let Some(serde_json::Value::Array(any_of)) = prop.get_mut("anyOf")
            && let serde_json::Value::Object(sub) = default_val
        {
            for variant in any_of {
                if let serde_json::Value::Object(v) = variant
                    && v.get("type") == Some(&serde_json::Value::String("object".into()))
                {
                    let mut sub_schema = serde_json::Value::Object(v.clone());
                    merge_defaults(&mut sub_schema, sub);
                    if let serde_json::Value::Object(updated) = sub_schema {
                        *v = updated;
                    }
                    break;
                }
            }
        }

        // annotation defaults (from `#[schemars(default)]`) take priority
        prop.entry("default").or_insert_with(|| default_val.clone());
    }
}
