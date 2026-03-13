//! lazy skill routing tools
//!
//! three tools that let the agent discover and load skills on demand
//! instead of eagerly injecting all skill content into the system prompt.
//!
//! - `list_skills`: returns names and one-line descriptions
//! - `describe_skill`: returns full metadata for a named skill
//! - `load_skill`: reads the full SKILL.md content

use std::path::PathBuf;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

/// lightweight skill info passed at construction time
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// shared skill registry for all three tools
type SkillRegistry = Arc<Vec<SkillInfo>>;

// -- list_skills --

pub struct ListSkillsTool {
    skills: SkillRegistry,
}

impl ListSkillsTool {
    pub fn new(skills: SkillRegistry) -> Self {
        Self { skills }
    }
}

impl AgentTool for ListSkillsTool {
    fn name(&self) -> &str {
        "list_skills"
    }
    fn label(&self) -> &str {
        "Skills"
    }
    fn description(&self) -> &str {
        "List available skills with their names and one-line descriptions. \
         Use this to discover what specialised instructions are available \
         before loading them."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn execute(
        &self,
        _args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            if self.skills.is_empty() {
                return ToolResult::text("no skills available");
            }
            let listing: String = self
                .skills
                .iter()
                .map(|s| format!("- {}: {}", s.name, s.description))
                .collect::<Vec<_>>()
                .join("\n");
            ToolResult::text(format!(
                "{} skills available:\n{listing}",
                self.skills.len()
            ))
        })
    }
}

// -- describe_skill --

/// shared args for describe_skill and load_skill
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    name: String,
}

/// schema for tools that take a skill name
fn name_param_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "skill name (from list_skills)"
            }
        },
        "required": ["name"]
    })
}

/// case-insensitive skill lookup, returns the skill or a not-found error
fn find_skill<'a>(skills: &'a [SkillInfo], name: &str) -> Result<&'a SkillInfo, ToolResult> {
    let query = name.to_lowercase();
    skills
        .iter()
        .find(|s| s.name.to_lowercase() == query)
        .ok_or_else(|| {
            let available: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
            ToolResult::error(format!(
                "skill '{name}' not found. available: {}",
                available.join(", ")
            ))
        })
}

pub struct DescribeSkillTool {
    skills: SkillRegistry,
}

impl DescribeSkillTool {
    pub fn new(skills: SkillRegistry) -> Self {
        Self { skills }
    }
}

impl AgentTool for DescribeSkillTool {
    fn name(&self) -> &str {
        "describe_skill"
    }
    fn label(&self) -> &str {
        "Skill Info"
    }
    fn description(&self) -> &str {
        "Get detailed information about a specific skill including its \
         description and file path. Use list_skills first to see available names."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        name_param_schema()
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<NameArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };

            match find_skill(&self.skills, &args.name) {
                Ok(s) => ToolResult::text(format!(
                    "name: {}\ndescription: {}\npath: {}\n\nuse load_skill to read the full content",
                    s.name,
                    s.description,
                    s.path.display()
                )),
                Err(e) => e,
            }
        })
    }
}

// -- load_skill --

pub struct LoadSkillTool {
    skills: SkillRegistry,
}

impl LoadSkillTool {
    pub fn new(skills: SkillRegistry) -> Self {
        Self { skills }
    }
}

impl AgentTool for LoadSkillTool {
    fn name(&self) -> &str {
        "load_skill"
    }
    fn label(&self) -> &str {
        "Skill"
    }
    fn description(&self) -> &str {
        "Load the full content of a skill's SKILL.md file. \
         Use list_skills to see available skills first."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        name_param_schema()
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<NameArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };

            let s = match find_skill(&self.skills, &args.name) {
                Ok(s) => s,
                Err(e) => return e,
            };

            match std::fs::read_to_string(&s.path) {
                Ok(content) => ToolResult::text(format!(
                    "# skill: {}\n\n{}",
                    s.name, content
                )),
                Err(e) => ToolResult::error(format!(
                    "failed to read {}: {e}",
                    s.path.display()
                )),
            }
        })
    }
}

/// create the three skill tools from a list of skill infos
pub fn skill_tools(skills: Vec<SkillInfo>) -> Vec<Arc<dyn AgentTool>> {
    let registry: SkillRegistry = Arc::new(skills);
    vec![
        Arc::new(ListSkillsTool::new(Arc::clone(&registry))),
        Arc::new(DescribeSkillTool::new(Arc::clone(&registry))),
        Arc::new(LoadSkillTool::new(registry)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use mush_ai::types::ToolOutcome;

    fn test_skills() -> Vec<SkillInfo> {
        vec![
            SkillInfo {
                name: "rust-idioms".into(),
                description: "Rust idioms and patterns reference".into(),
                path: PathBuf::from("/skills/rust-idioms/SKILL.md"),
            },
            SkillInfo {
                name: "nix".into(),
                description: "Nix flake conventions".into(),
                path: PathBuf::from("/skills/nix/SKILL.md"),
            },
        ]
    }

    #[tokio::test]
    async fn list_skills_shows_all() {
        let tools = skill_tools(test_skills());
        let result = tools[0].execute(serde_json::json!({})).await;
        let text = extract_text(&result);
        assert!(text.contains("2 skills available"));
        assert!(text.contains("rust-idioms: Rust idioms"));
        assert!(text.contains("nix: Nix flake"));
    }

    #[tokio::test]
    async fn list_skills_empty() {
        let tools = skill_tools(vec![]);
        let result = tools[0].execute(serde_json::json!({})).await;
        assert_eq!(extract_text(&result), "no skills available");
    }

    #[tokio::test]
    async fn describe_skill_found() {
        let tools = skill_tools(test_skills());
        let result = tools[1]
            .execute(serde_json::json!({"name": "nix"}))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("name: nix"));
        assert!(text.contains("Nix flake conventions"));
        assert!(text.contains("use load_skill"));
    }

    #[tokio::test]
    async fn describe_skill_case_insensitive() {
        let tools = skill_tools(test_skills());
        let result = tools[1]
            .execute(serde_json::json!({"name": "Rust-Idioms"}))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("name: rust-idioms"));
    }

    #[tokio::test]
    async fn describe_skill_not_found() {
        let tools = skill_tools(test_skills());
        let result = tools[1]
            .execute(serde_json::json!({"name": "docker"}))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("not found"));
        assert!(text.contains("rust-idioms, nix"));
    }

    #[tokio::test]
    async fn load_skill_not_found() {
        let tools = skill_tools(test_skills());
        let result = tools[2]
            .execute(serde_json::json!({"name": "missing"}))
            .await;
        assert_eq!(result.outcome, ToolOutcome::Error);
        assert!(extract_text(&result).contains("not found"));
    }

    #[tokio::test]
    async fn load_skill_missing_file() {
        let tools = skill_tools(test_skills());
        let result = tools[2]
            .execute(serde_json::json!({"name": "nix"}))
            .await;
        assert_eq!(result.outcome, ToolOutcome::Error);
        assert!(extract_text(&result).contains("failed to read"));
    }

    #[tokio::test]
    async fn load_skill_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(&skill_path, "# test skill\nsome content").unwrap();

        let skills = vec![SkillInfo {
            name: "test".into(),
            description: "a test skill".into(),
            path: skill_path,
        }];
        let tools = skill_tools(skills);
        let result = tools[2]
            .execute(serde_json::json!({"name": "test"}))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("# skill: test"));
        assert!(text.contains("some content"));
    }
}
