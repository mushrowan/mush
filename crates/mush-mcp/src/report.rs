use crate::catalogue::McpToolInfo;
use crate::server::{McpServerStatus, ServerState};

#[must_use]
pub fn format_status(statuses: &[McpServerStatus]) -> String {
    if statuses.is_empty() {
        return "no MCP servers configured".to_string();
    }

    let width = statuses
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or_default();

    let mut out = String::from("MCP servers:");
    for status in statuses {
        let detail = match &status.state {
            ServerState::Connected { tools } => format!("{tools} tools"),
            ServerState::Failed { error } => error.clone(),
            ServerState::Disabled => "enabled = false".to_string(),
        };
        out.push_str(&format!(
            "\n  {:width$}  {}  {}  {detail}",
            status.name,
            status.state.label(),
            status.transport,
        ));
    }
    out
}

#[must_use]
pub fn format_tools(tools: &[McpToolInfo], server: Option<&str>) -> String {
    if tools.is_empty() {
        return match server {
            Some(name) => format!("no tools from MCP server '{name}'"),
            None => "no MCP tools available".to_string(),
        };
    }

    let mut out = format!("{} MCP tools:", tools.len());
    for tool in tools {
        out.push_str(&format!("\n  {}  {}", tool.full_name, tool.description));
    }
    out
}

#[must_use]
pub fn format_schema(name: &str, schema: Option<&serde_json::Value>) -> String {
    match schema {
        Some(schema) => format!(
            "{name}:\n```json\n{}\n```",
            serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
        ),
        None => format!("no MCP tool named '{name}'. try /mcp tools"),
    }
}

#[must_use]
pub fn format_reconnect(name: &str, state: Option<&ServerState>) -> String {
    match state {
        Some(ServerState::Connected { tools }) => {
            format!("reconnected {name} ({tools} tools)")
        }
        Some(ServerState::Failed { error }) => format!("{name} still failing: {error}"),
        Some(ServerState::Disabled) => {
            format!("{name} is disabled in config, nothing to reconnect")
        }
        None => format!("no MCP server named '{name}'. try /mcp"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str, transport: &str, state: ServerState) -> McpServerStatus {
        McpServerStatus {
            name: name.to_string(),
            transport: transport.to_string(),
            state,
        }
    }

    fn tool(full_name: &str, description: &str) -> McpToolInfo {
        McpToolInfo {
            server: full_name.split('_').next().unwrap_or_default().to_string(),
            full_name: full_name.to_string(),
            tool_name: full_name.to_string(),
            description: description.to_string(),
        }
    }

    #[test]
    fn status_of_nothing_says_so_rather_than_printing_an_empty_table() {
        assert_eq!(format_status(&[]), "no MCP servers configured");
    }

    #[test]
    fn status_shows_tool_count_for_a_connected_server() {
        let out = format_status(&[status(
            "sweethome3d",
            "http://127.0.0.1:9877/mcp",
            ServerState::Connected { tools: 42 },
        )]);

        assert!(out.contains("sweethome3d"), "got {out}");
        assert!(out.contains("connected"), "got {out}");
        assert!(out.contains("42 tools"), "got {out}");
        assert!(
            out.contains("9877"),
            "the transport should be visible: {out}"
        );
    }

    #[test]
    fn status_surfaces_the_failure_reason() {
        let out = format_status(&[status(
            "git",
            "uvx mcp-server-git",
            ServerState::Failed {
                error: "connection refused".into(),
            },
        )]);

        assert!(out.contains("failed"), "got {out}");
        assert!(
            out.contains("connection refused"),
            "the reason is the whole point: {out}"
        );
    }

    #[test]
    fn status_explains_a_disabled_server() {
        let out = format_status(&[status("old", "uvx thing", ServerState::Disabled)]);
        assert!(out.contains("disabled"), "got {out}");
        assert!(
            out.contains("enabled = false"),
            "should point at the config key: {out}"
        );
    }

    #[test]
    fn status_aligns_names_into_a_column() {
        let out = format_status(&[
            status("a", "x", ServerState::Disabled),
            status("longer-name", "y", ServerState::Disabled),
        ]);

        let lines: Vec<&str> = out.lines().skip(1).collect();
        let disabled_at: Vec<_> = lines
            .iter()
            .map(|l| l.find("disabled").expect("state is present"))
            .collect();
        assert_eq!(
            disabled_at[0], disabled_at[1],
            "states should line up, got {out}"
        );
    }

    #[test]
    fn tools_of_nothing_names_the_server_that_was_asked_about() {
        assert_eq!(
            format_tools(&[], Some("sweethome3d")),
            "no tools from MCP server 'sweethome3d'"
        );
        assert_eq!(format_tools(&[], None), "no MCP tools available");
    }

    #[test]
    fn tools_are_listed_with_descriptions_and_a_count() {
        let out = format_tools(
            &[
                tool("sweethome3d_render_photo", "ray-traced render"),
                tool("sweethome3d_add_wall", "create a wall"),
            ],
            None,
        );

        assert!(out.starts_with("2 MCP tools:"), "got {out}");
        assert!(out.contains("sweethome3d_render_photo"), "got {out}");
        assert!(out.contains("ray-traced render"), "got {out}");
    }

    #[test]
    fn schema_is_pretty_printed() {
        let schema = serde_json::json!({ "type": "object" });
        let out = format_schema("sweethome3d_render_photo", Some(&schema));

        assert!(out.contains("```json"), "got {out}");
        assert!(out.contains("\"type\": \"object\""), "got {out}");
    }

    #[test]
    fn missing_schema_points_at_the_listing_command() {
        let out = format_schema("nope", None);
        assert!(out.contains("/mcp tools"), "got {out}");
    }

    #[test]
    fn reconnect_reports_each_outcome() {
        assert!(
            format_reconnect("sweethome3d", Some(&ServerState::Connected { tools: 42 }))
                .contains("42 tools")
        );
        assert!(
            format_reconnect(
                "git",
                Some(&ServerState::Failed {
                    error: "refused".into()
                })
            )
            .contains("refused")
        );
        assert!(format_reconnect("old", Some(&ServerState::Disabled)).contains("disabled"));
        assert!(format_reconnect("ghost", None).contains("no MCP server"));
    }
}
