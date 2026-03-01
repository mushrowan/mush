//! display helpers for agent output

/// summarise tool arguments for compact display
pub fn summarise_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name.to_lowercase().replace('_', "").as_str() {
        "read" | "write" | "edit" => args["path"].as_str().unwrap_or("").to_string(),
        "bash" => {
            let cmd = args["command"].as_str().unwrap_or("");
            if cmd.len() > 60 {
                format!("{}...", &cmd[..57])
            } else {
                cmd.to_string()
            }
        }
        "grep" => {
            let pattern = args["pattern"].as_str().unwrap_or("");
            let path = args["path"].as_str().unwrap_or(".");
            format!("{pattern} in {path}")
        }
        "glob" | "find" => args["pattern"].as_str().unwrap_or("").to_string(),
        "ls" => args["path"].as_str().unwrap_or(".").to_string(),
        "websearch" => {
            let query = args["query"].as_str().unwrap_or("");
            if query.len() > 60 {
                format!("{}...", &query[..57])
            } else {
                query.to_string()
            }
        }
        "webfetch" => args["url"].as_str().unwrap_or("").to_string(),
        "batch" => {
            let count = args["tool_calls"].as_array().map(|a| a.len()).unwrap_or(0);
            format!("{count} tool calls")
        }
        _ => {
            let s = args.to_string();
            if s.len() > 60 {
                format!("{}...", &s[..57])
            } else {
                s
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_tool_shows_path() {
        let args = serde_json::json!({"path": "src/main.rs"});
        assert_eq!(summarise_tool_args("read", &args), "src/main.rs");
        assert_eq!(summarise_tool_args("Read", &args), "src/main.rs");
    }

    #[test]
    fn bash_tool_truncates() {
        let short = serde_json::json!({"command": "ls -la"});
        assert_eq!(summarise_tool_args("bash", &short), "ls -la");

        let long_cmd = "x".repeat(100);
        let long = serde_json::json!({"command": long_cmd});
        let summary = summarise_tool_args("bash", &long);
        assert!(summary.len() <= 63);
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn grep_tool_shows_pattern_and_path() {
        let args = serde_json::json!({"pattern": "TODO", "path": "src/"});
        assert_eq!(summarise_tool_args("grep", &args), "TODO in src/");
    }

    #[test]
    fn unknown_tool_shows_args() {
        let args = serde_json::json!({"key": "val"});
        let summary = summarise_tool_args("custom", &args);
        assert!(summary.contains("key"));
    }

    #[test]
    fn web_search_shows_query() {
        let args = serde_json::json!({"query": "rust async runtime"});
        assert_eq!(
            summarise_tool_args("web_search", &args),
            "rust async runtime"
        );
        assert_eq!(
            summarise_tool_args("WebSearch", &args),
            "rust async runtime"
        );
    }

    #[test]
    fn web_fetch_shows_url() {
        let args = serde_json::json!({"url": "https://docs.rs/tokio"});
        assert_eq!(
            summarise_tool_args("web_fetch", &args),
            "https://docs.rs/tokio"
        );
    }

    #[test]
    fn batch_shows_count() {
        let args = serde_json::json!({
            "tool_calls": [
                {"tool": "read", "parameters": {"path": "a.rs"}},
                {"tool": "read", "parameters": {"path": "b.rs"}},
            ]
        });
        assert_eq!(summarise_tool_args("batch", &args), "2 tool calls");
    }
}
