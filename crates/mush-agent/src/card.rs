use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub version: String,
    pub model: String,
    pub tools: Vec<ToolEntry>,
    pub capabilities: Capabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Capabilities {
    pub streaming: bool,
    pub multi_pane: bool,
    pub messaging: bool,
    pub lsp: bool,
}

impl AgentCard {
    pub fn build(model_id: &str, tools: &crate::tool::ToolRegistry) -> Self {
        let tool_entries: Vec<ToolEntry> = tools
            .iter()
            .map(|t| ToolEntry {
                name: t.name().to_string(),
                description: t.description().to_string(),
            })
            .collect();

        Self {
            name: "mush".to_string(),
            description: "minimal, extensible coding agent".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            model: model_id.to_string(),
            tools: tool_entries,
            capabilities: Capabilities::default(),
            url: None,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "AgentCard fields are all simple serialisable types"
    )]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("AgentCard is always serialisable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistry;

    #[test]
    fn build_card_from_empty_registry() {
        let card = AgentCard::build("claude-sonnet-4-20250514", &ToolRegistry::new());

        assert_eq!(card.name, "mush");
        assert_eq!(card.model, "claude-sonnet-4-20250514");
        assert!(card.tools.is_empty());
        assert!(card.url.is_none());
        assert!(!card.to_json().contains("url"));
    }

    #[test]
    fn build_card_with_tools() {
        use crate::tool::{AgentTool, ToolResult};

        struct DummyTool;
        #[async_trait::async_trait]
        impl AgentTool for DummyTool {
            fn name(&self) -> &str {
                "read"
            }
            fn label(&self) -> &str {
                "Read"
            }
            fn description(&self) -> &str {
                "read a file"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn output_limit(&self) -> crate::tool::OutputLimit {
                crate::tool::OutputLimit::Head
            }
            async fn execute(&self, _: serde_json::Value) -> ToolResult {
                ToolResult::text("ok")
            }
        }

        let tools = ToolRegistry::from_boxed(vec![Box::new(DummyTool)]);
        let card = AgentCard::build("gpt-4o", &tools);

        assert_eq!(card.tools.len(), 1);
        assert_eq!(card.tools[0].name, "read");
        assert_eq!(card.tools[0].description, "read a file");
    }

    #[test]
    fn card_roundtrips_through_json() {
        let card = AgentCard {
            name: "mush".into(),
            description: "test agent".into(),
            version: "0.1.0".into(),
            model: "test-model".into(),
            tools: vec![ToolEntry {
                name: "bash".into(),
                description: "run commands".into(),
            }],
            capabilities: Capabilities {
                streaming: true,
                multi_pane: true,
                messaging: true,
                lsp: false,
            },
            url: Some("unix:///tmp/mush/abc.sock".into()),
        };

        let json = card.to_json();
        let parsed: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(card, parsed);
    }
}
