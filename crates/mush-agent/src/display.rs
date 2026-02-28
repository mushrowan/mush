//! display helpers for agent output

/// summarise tool arguments for compact display
pub fn summarise_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "Read" | "Write" | "Edit" | "read" | "write" | "edit" => {
            args["path"].as_str().unwrap_or("").to_string()
        }
        "Bash" | "bash" => {
            let cmd = args["command"].as_str().unwrap_or("");
            if cmd.len() > 60 {
                format!("{}...", &cmd[..57])
            } else {
                cmd.to_string()
            }
        }
        "Grep" | "grep" => {
            let pattern = args["pattern"].as_str().unwrap_or("");
            let path = args["path"].as_str().unwrap_or(".");
            format!("{pattern} in {path}")
        }
        "Glob" | "Find" | "find" => args["pattern"].as_str().unwrap_or("").to_string(),
        "Ls" | "ls" => args["path"].as_str().unwrap_or(".").to_string(),
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
}
