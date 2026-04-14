//! shared state store for inter-agent coordination
//!
//! a typed key-value store accessible by all panes, with configurable
//! merge strategies (reducers) per field. inspired by langgraph's
//! state management pattern

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// how values are merged when multiple agents write to the same key
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reducer {
    /// last write wins (default)
    #[default]
    Overwrite,
    /// append to a JSON array (creates one if key doesn't exist)
    Append,
}

#[derive(Debug, Error)]
pub enum SharedStateError {
    #[error("failed to serialise shared state value")]
    Serialise(#[source] serde_json::Error),
    #[error("failed to deserialise shared state value")]
    Deserialise(#[source] serde_json::Error),
}

/// shared state store accessible by all panes
#[derive(Clone)]
pub struct SharedState {
    fields: Arc<Mutex<HashMap<String, Value>>>,
    reducers: Arc<Mutex<HashMap<String, Reducer>>>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            fields: Arc::new(Mutex::new(HashMap::new())),
            reducers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// set the reducer for a key
    pub fn set_reducer(&self, key: &str, reducer: Reducer) {
        self.reducers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), reducer);
    }

    /// read a value by key
    pub fn get(&self, key: &str) -> Option<Value> {
        self.fields
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    pub fn get_typed<T>(&self, key: &str) -> Result<Option<T>, SharedStateError>
    where
        T: DeserializeOwned,
    {
        self.get(key)
            .map(|value| serde_json::from_value(value).map_err(SharedStateError::Deserialise))
            .transpose()
    }

    /// write a value, applying the configured reducer
    pub fn set(&self, key: &str, value: Value) {
        let reducer = self
            .reducers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .copied()
            .unwrap_or_default();

        let mut fields = self.fields.lock().unwrap_or_else(|e| e.into_inner());
        match reducer {
            Reducer::Overwrite => {
                fields.insert(key.to_string(), value);
            }
            Reducer::Append => {
                let entry = fields
                    .entry(key.to_string())
                    .or_insert_with(|| Value::Array(vec![]));
                if let Value::Array(arr) = entry {
                    match value {
                        Value::Array(items) => arr.extend(items),
                        other => arr.push(other),
                    }
                } else {
                    // key exists but isn't an array, convert
                    let old = entry.take();
                    *entry = Value::Array(vec![old, value]);
                }
            }
        }
    }

    pub fn set_typed<T>(&self, key: &str, value: T) -> Result<(), SharedStateError>
    where
        T: Serialize,
    {
        let value = serde_json::to_value(value).map_err(SharedStateError::Serialise)?;
        self.set(key, value);
        Ok(())
    }

    /// delete a key
    pub fn remove(&self, key: &str) -> Option<Value> {
        self.fields
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key)
    }

    /// list all keys
    pub fn keys(&self) -> Vec<String> {
        self.fields
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// snapshot of all state (for debugging / display)
    pub fn snapshot(&self) -> HashMap<String, Value> {
        self.fields
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadStateArgs {
    key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteStateArgs {
    key: String,
    value: Value,
    reducer: Option<Reducer>,
}

/// tool for reading shared state
pub struct ReadStateTool {
    pub state: SharedState,
}

#[async_trait::async_trait]
impl AgentTool for ReadStateTool {
    fn name(&self) -> &str {
        "read_state"
    }

    fn label(&self) -> &str {
        "Read State"
    }

    fn description(&self) -> &str {
        "read a value from shared state. all agents can read and write \
         to shared state for coordination. use `key` to read a specific \
         field, or omit it to list all keys"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "state key to read (omit to list all keys)"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<ReadStateArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        match args.key.as_deref() {
            Some(key) => match self.state.get(key) {
                Some(val) => ToolResult::text(
                    serde_json::to_string_pretty(&val).unwrap_or_else(|_| format!("{val:?}")),
                ),
                None => ToolResult::text(format!("key \"{key}\" not found")),
            },
            None => {
                let keys = self.state.keys();
                if keys.is_empty() {
                    ToolResult::text("shared state is empty")
                } else {
                    ToolResult::text(format!("keys: {}", keys.join(", ")))
                }
            }
        }
    }
}

/// tool for writing shared state
pub struct WriteStateTool {
    pub state: SharedState,
}

#[async_trait::async_trait]
impl AgentTool for WriteStateTool {
    fn name(&self) -> &str {
        "write_state"
    }

    fn label(&self) -> &str {
        "Write State"
    }

    fn description(&self) -> &str {
        "write a value to shared state. all agents can read and write \
         to shared state for coordination. values are JSON. set `reducer` \
         to control how concurrent writes merge: \"overwrite\" (default, \
         last write wins) or \"append\" (adds to a JSON array)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "state key to write"
                },
                "value": {
                    "description": "JSON value to store"
                },
                "reducer": {
                    "type": "string",
                    "enum": ["overwrite", "append"],
                    "description": "merge strategy for this key (default: overwrite)"
                }
            },
            "required": ["key", "value"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<WriteStateArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        if let Some(reducer) = args.reducer {
            self.state.set_reducer(&args.key, reducer);
        }

        self.state.set(&args.key, args.value);
        ToolResult::text(format!("wrote to \"{}\"", args.key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Progress {
        percent: u8,
        label: String,
    }

    #[test]
    fn overwrite_reducer() {
        let state = SharedState::new();
        state.set("count", serde_json::json!(1));
        state.set("count", serde_json::json!(2));
        assert_eq!(state.get("count"), Some(serde_json::json!(2)));
    }

    #[test]
    fn append_reducer() {
        let state = SharedState::new();
        state.set_reducer("log", Reducer::Append);
        state.set("log", serde_json::json!("first"));
        state.set("log", serde_json::json!("second"));
        assert_eq!(
            state.get("log"),
            Some(serde_json::json!(["first", "second"]))
        );
    }

    #[test]
    fn append_array_into_array() {
        let state = SharedState::new();
        state.set_reducer("items", Reducer::Append);
        state.set("items", serde_json::json!([1, 2]));
        state.set("items", serde_json::json!([3, 4]));
        assert_eq!(state.get("items"), Some(serde_json::json!([1, 2, 3, 4])));
    }

    #[test]
    fn append_converts_non_array() {
        let state = SharedState::new();
        // set without reducer first, so it's a plain value
        state.set("x", serde_json::json!("old"));
        // now switch to append
        state.set_reducer("x", Reducer::Append);
        state.set("x", serde_json::json!("new"));
        assert_eq!(state.get("x"), Some(serde_json::json!(["old", "new"])));
    }

    #[test]
    fn get_typed_round_trips() {
        let state = SharedState::new();
        state
            .set_typed(
                "progress",
                Progress {
                    percent: 42,
                    label: "halfway".into(),
                },
            )
            .unwrap();

        let progress = state.get_typed::<Progress>("progress").unwrap();
        assert_eq!(
            progress,
            Some(Progress {
                percent: 42,
                label: "halfway".into(),
            })
        );
    }

    #[test]
    fn get_missing_key() {
        let state = SharedState::new();
        assert_eq!(state.get("nope"), None);
        assert_eq!(state.get_typed::<Progress>("nope").unwrap(), None);
    }

    #[test]
    fn get_typed_reports_shape_mismatch() {
        let state = SharedState::new();
        state.set("progress", serde_json::json!(42));
        assert!(matches!(
            state.get_typed::<Progress>("progress"),
            Err(SharedStateError::Deserialise(_))
        ));
    }

    #[test]
    fn remove_key() {
        let state = SharedState::new();
        state.set("tmp", serde_json::json!(true));
        assert!(state.remove("tmp").is_some());
        assert_eq!(state.get("tmp"), None);
    }

    #[test]
    fn keys_listing() {
        let state = SharedState::new();
        state.set("a", serde_json::json!(1));
        state.set("b", serde_json::json!(2));
        let mut keys = state.keys();
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn read_state_tool_list_keys() {
        let state = SharedState::new();
        state.set("foo", serde_json::json!("bar"));

        let tool = ReadStateTool {
            state: state.clone(),
        };
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.outcome.is_success());
        let text = result
            .content
            .iter()
            .find_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(text.contains("foo"));
    }

    #[tokio::test]
    async fn write_state_tool_sets_value() {
        let state = SharedState::new();
        let tool = WriteStateTool {
            state: state.clone(),
        };
        let result = tool
            .execute(serde_json::json!({
                "key": "progress",
                "value": 42
            }))
            .await;
        assert!(result.outcome.is_success());
        assert_eq!(state.get("progress"), Some(serde_json::json!(42)));
    }

    #[tokio::test]
    async fn write_state_tool_with_append_reducer() {
        let state = SharedState::new();
        let tool = WriteStateTool {
            state: state.clone(),
        };

        let _ = tool
            .execute(serde_json::json!({
                "key": "findings",
                "value": "bug in auth",
                "reducer": "append"
            }))
            .await;
        let _ = tool
            .execute(serde_json::json!({
                "key": "findings",
                "value": "perf issue in render"
            }))
            .await;

        assert_eq!(
            state.get("findings"),
            Some(serde_json::json!(["bug in auth", "perf issue in render"]))
        );
    }
}
