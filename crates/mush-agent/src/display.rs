//! display helpers for agent output

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    let ellipsis = if max_chars >= 3 { "..." } else { "" };
    let keep = max_chars.saturating_sub(ellipsis.len());
    let mut iter = text.char_indices();

    for _ in 0..keep {
        if iter.next().is_none() {
            return text.to_string();
        }
    }

    let Some((end, _)) = iter.next() else {
        return text.to_string();
    };

    let mut truncated = text[..end].to_string();
    truncated.push_str(ellipsis);
    truncated
}

/// summarise tool arguments for compact display
pub fn summarise_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name.to_lowercase().replace('_', "").as_str() {
        "read" | "write" | "edit" => args["path"].as_str().unwrap_or("").to_string(),
        "bash" => {
            let raw = args["command"].as_str().unwrap_or("");
            // collapse newlines into " && " so the summary stays single-line
            // (ratatui swallows \n inside a Span, concatenating text with no separator)
            let cmd = if raw.contains('\n') {
                raw.split('\n')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" && ")
            } else {
                raw.to_string()
            };
            if cmd.chars().count() > 60 {
                truncate_with_ellipsis(&cmd, 60)
            } else {
                cmd
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
            if query.chars().count() > 60 {
                truncate_with_ellipsis(query, 60)
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
            if s.chars().count() > 60 {
                truncate_with_ellipsis(&s, 60)
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
    fn bash_tool_truncates_multibyte_safely() {
        let long_cmd = "—".repeat(100);
        let long = serde_json::json!({"command": long_cmd});
        let summary = summarise_tool_args("bash", &long);
        assert!(summary.ends_with("..."));
        assert!(summary.chars().count() <= 60);
    }

    #[test]
    fn bash_tool_keeps_short_command() {
        let short = serde_json::json!({"command": "ls -la"});
        assert_eq!(summarise_tool_args("bash", &short), "ls -la");
    }

    #[test]
    fn bash_tool_collapses_newlines() {
        // models sometimes send multi-line commands like cd + echo
        // newlines in the summary cause display glitches in the TUI
        // because ratatui swallows \n inside a Span
        let multiline = serde_json::json!({"command": "cd /home/user/dev/project\necho hello"});
        let summary = summarise_tool_args("bash", &multiline);
        assert!(
            !summary.contains('\n'),
            "summary should not contain newlines: {summary}"
        );
        assert_eq!(summary, "cd /home/user/dev/project && echo hello");
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
