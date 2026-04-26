//! display helpers for agent output

/// truncate text to at most `max_chars` (unicode-aware), appending an ellipsis
/// if anything was cut. consumers that render in narrow, non-wrapping widgets
/// (like pane tool summaries) use this to keep output to a single line
pub fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
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

/// summarise tool arguments for compact display.
///
/// returns a tool-specific one-line preview (file path for `read`, the
/// command for `bash`, etc). when the model sends args that don't carry
/// the expected field (wrong field name, or an unknown tool), falls
/// back to a compact `{key: value, ...}` preview of the raw args so the
/// title bar always shows what was attempted instead of going blank.
/// the same fallback is what [`crate::tool::parse_tool_args`] embeds in
/// its error body, so the user sees a consistent shape either way
pub fn summarise_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    extract_summary(tool_name, args)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::tool::preview_args(args))
}

/// per-tool summary extraction. returns `None` when the expected field
/// is missing so the caller can apply a uniform fallback. tool-specific
/// formatting (the `pat in path` shape for grep, the && collapse for
/// bash) lives here, but the missing-field fallback does NOT
fn extract_summary(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    match tool_name.to_lowercase().replace('_', "").as_str() {
        "read" | "write" | "edit" => args["path"].as_str().map(str::to_string),
        "bash" => args["command"].as_str().map(collapse_command_newlines),
        "grep" => {
            let pattern = args["pattern"].as_str()?;
            let path = args["path"].as_str().unwrap_or(".");
            Some(format!("{pattern} in {path}"))
        }
        "glob" | "find" => args["pattern"].as_str().map(str::to_string),
        "ls" => Some(args["path"].as_str().unwrap_or(".").to_string()),
        "websearch" => args["query"].as_str().map(|q| {
            if q.chars().count() > 60 {
                truncate_with_ellipsis(q, 60)
            } else {
                q.to_string()
            }
        }),
        "webfetch" => args["url"].as_str().map(str::to_string),
        "batch" => {
            let count = args["tool_calls"].as_array().map(|a| a.len()).unwrap_or(0);
            Some(format!("{count} tool calls"))
        }
        _ => None,
    }
}

/// collapse newlines in a bash command into ` && ` so the summary stays
/// single-line. ratatui swallows `\n` inside a Span otherwise
fn collapse_command_newlines(raw: &str) -> String {
    if raw.contains('\n') {
        raw.split('\n')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" && ")
    } else {
        raw.to_string()
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
    fn bash_tool_returns_full_command() {
        // `summarise_tool_args` no longer truncates; consumers that need a
        // one-line preview (e.g. pane tool summaries) truncate explicitly
        let long_cmd = "—".repeat(100);
        let long = serde_json::json!({"command": long_cmd.clone()});
        let summary = summarise_tool_args("bash", &long);
        assert_eq!(summary, long_cmd);
    }

    #[test]
    fn truncate_with_ellipsis_is_multibyte_safe() {
        let long = "—".repeat(100);
        let truncated = truncate_with_ellipsis(&long, 20);
        assert!(truncated.ends_with("..."));
        assert!(truncated.chars().count() <= 20);
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
    fn read_tool_shows_received_args_when_path_missing() {
        // when a model sends the wrong field name (e.g. `file_path` instead
        // of `path`), the tool will fail to parse its args. the summary
        // should still show what was attempted so the user (and the model
        // on the next turn) can see the mistake instead of staring at an
        // empty title bar
        let args = serde_json::json!({"file_path": "src/main.rs"});
        let summary = summarise_tool_args("read", &args);
        assert!(
            summary.contains("file_path"),
            "summary should fall back to received args when path is missing: {summary}"
        );
        assert!(
            summary.contains("src/main.rs"),
            "summary should include the attempted value: {summary}"
        );
    }

    #[test]
    fn edit_tool_shows_received_args_when_path_missing() {
        let args = serde_json::json!({"filepath": "x.rs", "old": "a", "new": "b"});
        let summary = summarise_tool_args("edit", &args);
        assert!(summary.contains("filepath"), "summary: {summary}");
    }

    #[test]
    fn bash_tool_shows_received_args_when_command_missing() {
        let args = serde_json::json!({"cmd": "ls"});
        let summary = summarise_tool_args("bash", &args);
        assert!(
            summary.contains("cmd") && summary.contains("ls"),
            "bash summary should fall back to received args when command is missing: {summary}"
        );
    }

    #[test]
    fn grep_tool_shows_received_args_when_pattern_missing() {
        let args = serde_json::json!({"query": "TODO"});
        let summary = summarise_tool_args("grep", &args);
        assert!(
            summary.contains("query") && summary.contains("TODO"),
            "grep summary should fall back when pattern is missing: {summary}"
        );
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
