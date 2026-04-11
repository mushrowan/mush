//! progressive skill loading tool

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mush_agent::tool::{AgentTool, OutputLimit, ToolResult, parse_tool_args};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

type SkillRegistry = Arc<Vec<SkillInfo>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    name: String,
}

fn name_param_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "skill name from the available skills list"
            }
        },
        "required": ["name"]
    })
}

fn find_skill<'a>(skills: &'a [SkillInfo], name: &str) -> Result<&'a SkillInfo, ToolResult> {
    let query = name.to_lowercase();
    skills
        .iter()
        .find(|skill| skill.name.to_lowercase() == query)
        .ok_or_else(|| {
            let available: Vec<_> = skills.iter().map(|skill| skill.name.as_str()).collect();
            ToolResult::error(format!(
                "skill '{name}' not found. available: {}",
                available.join(", ")
            ))
        })
}

fn skill_base_dir(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

fn sample_skill_files(root: &Path, limit: usize) -> Vec<String> {
    let mut queue = VecDeque::from([root.to_path_buf()]);
    let mut files = Vec::new();

    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };

        let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        entries.sort();

        for path in entries {
            if files.len() >= limit {
                return files;
            }

            if path.is_dir() {
                queue.push_back(path);
                continue;
            }

            if path.file_name().is_some_and(|name| name == "SKILL.md") {
                continue;
            }

            if let Ok(relative) = path.strip_prefix(root) {
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    files
}

fn format_skill_tool_description(skills: &[SkillInfo]) -> String {
    if skills.is_empty() {
        return "Load a specialised skill with task-specific instructions and bundled resources"
            .into();
    }

    let mut listed = skills.to_vec();
    listed.sort_by(|left, right| left.name.cmp(&right.name));

    let mut description = String::from(
        "Load a specialised skill with task-specific instructions and bundled resources\n\navailable skills:\n",
    );

    for skill in listed {
        description.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }

    description.trim_end().to_string()
}

pub struct SkillTool {
    skills: SkillRegistry,
    description: String,
}

impl SkillTool {
    pub fn new(skills: SkillRegistry) -> Self {
        let description = format_skill_tool_description(skills.as_ref());
        Self {
            skills,
            description,
        }
    }
}

impl AgentTool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn label(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        name_param_schema()
    }

    fn output_limit(&self) -> OutputLimit {
        OutputLimit::SelfManaged
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

            let skill = match find_skill(&self.skills, &args.name) {
                Ok(skill) => skill,
                Err(error) => return error,
            };

            let content = match std::fs::read_to_string(&skill.path) {
                Ok(content) => content,
                Err(error) => {
                    return ToolResult::error(format!(
                        "failed to read {}: {error}",
                        skill.path.display()
                    ));
                }
            };

            let base_dir = skill_base_dir(&skill.path);
            let files = sample_skill_files(&base_dir, 10);

            let mut output = format!(
                "<skill_content name=\"{}\">\n# Skill: {}\n\n{}\n\nBase directory for this skill: {}\nRelative paths in this skill are resolved from this directory",
                skill.name,
                skill.name,
                content.trim(),
                base_dir.display()
            );

            if !files.is_empty() {
                output.push_str("\n\n<skill_files>\n");
                output.push_str(&files.join("\n"));
                output.push_str("\n</skill_files>");
            }

            output.push_str("\n</skill_content>");

            ToolResult::text(output)
        })
    }
}

pub fn skill_tools(skills: Vec<SkillInfo>) -> Vec<Arc<dyn AgentTool>> {
    let registry: SkillRegistry = Arc::new(skills);
    vec![Arc::new(SkillTool::new(registry))]
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
    async fn skill_tools_register_single_skill_tool() {
        let tools = skill_tools(test_skills());
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "skill");
    }

    #[tokio::test]
    async fn skill_tool_description_lists_available_skills() {
        let tools = skill_tools(test_skills());
        let description = tools[0].description();

        assert!(description.contains("available skills"));
        assert!(description.contains("nix: Nix flake conventions"));
        assert!(description.contains("rust-idioms: Rust idioms and patterns reference"));
        assert!(description.find("nix:").unwrap() < description.find("rust-idioms:").unwrap());
    }

    #[tokio::test]
    async fn skill_tool_not_found_lists_available_skills() {
        let tools = skill_tools(test_skills());
        let result = tools[0]
            .execute(serde_json::json!({"name": "docker"}))
            .await;
        assert_eq!(result.outcome, ToolOutcome::Error);
        let text = extract_text(&result);
        assert!(text.contains("not found"));
        assert!(text.contains("rust-idioms, nix"));
    }

    #[tokio::test]
    async fn skill_tool_loads_content_with_base_dir_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("tool-skill");
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            &skill_path,
            "---\nname: tool-skill\ndescription: Skill for tool tests\n---\n\n# Tool Skill\n\nUse this skill\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("scripts/demo.txt"), "demo").unwrap();
        std::fs::write(skill_dir.join("references/notes.md"), "notes").unwrap();

        let tools = skill_tools(vec![SkillInfo {
            name: "tool-skill".into(),
            description: "Skill for tool tests".into(),
            path: skill_path,
        }]);
        let result = tools[0]
            .execute(serde_json::json!({"name": "tool-skill"}))
            .await;
        let text = extract_text(&result);

        assert!(text.contains("<skill_content name=\"tool-skill\">"));
        assert!(text.contains("# Skill: tool-skill"));
        assert!(text.contains("Base directory for this skill:"));
        assert!(text.contains("<skill_files>"));
        assert!(text.contains("scripts/demo.txt"));
        assert!(text.contains("references/notes.md"));
    }
}
