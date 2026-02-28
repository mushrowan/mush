//! extension and skill discovery
//!
//! finds AGENTS.md files and skill directories to build the
//! system prompt with project context.

use std::path::{Path, PathBuf};

/// discovered project context from AGENTS.md and skills
#[derive(Debug, Default)]
pub struct ProjectContext {
    /// content of AGENTS.md files found
    pub agents_md: Vec<AgentsMd>,
    /// skill files available
    pub skills: Vec<Skill>,
}

#[derive(Debug, Clone)]
pub struct AgentsMd {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// scan for AGENTS.md in the project directory and up to home
pub fn find_agents_md(cwd: &Path) -> Vec<AgentsMd> {
    let mut results = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);

    // walk up from cwd to home
    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        let agents_path = d.join("AGENTS.md");
        if agents_path.is_file()
            && let Ok(content) = std::fs::read_to_string(&agents_path)
        {
            results.push(AgentsMd {
                path: agents_path,
                content,
            });
        }

        // stop at home directory
        if home.as_ref().is_some_and(|h| &d == h) {
            break;
        }

        dir = d.parent().map(|p| p.to_path_buf());
    }

    // also check ~/.pi/agent/AGENTS.md (pi compat)
    if let Some(ref h) = home {
        let pi_agents = h.join(".pi/agent/AGENTS.md");
        if pi_agents.is_file()
            && !results.iter().any(|a| a.path == pi_agents)
            && let Ok(content) = std::fs::read_to_string(&pi_agents)
        {
            results.push(AgentsMd {
                path: pi_agents,
                content,
            });
        }
    }

    // reverse so most specific (project-level) comes last
    results.reverse();
    results
}

/// find skills from the available_skills block in AGENTS.md content
pub fn parse_skills_from_agents_md(content: &str) -> Vec<Skill> {
    let mut skills = Vec::new();

    // look for <available_skills> block
    let Some(start) = content.find("<available_skills>") else {
        return skills;
    };
    let Some(end) = content.find("</available_skills>") else {
        return skills;
    };

    let block = &content[start..end];

    // parse each <skill> block
    let mut pos = 0;
    while let Some(skill_start) = block[pos..].find("<skill>") {
        let skill_start = pos + skill_start;
        let Some(skill_end) = block[skill_start..].find("</skill>") else { break };
        let skill_end = skill_start + skill_end + "</skill>".len();
        let skill_block = &block[skill_start..skill_end];

        let name = extract_tag(skill_block, "name").unwrap_or_default();
        let description = extract_tag(skill_block, "description").unwrap_or_default();
        let location = extract_tag(skill_block, "location").unwrap_or_default();

        if !name.is_empty() && !location.is_empty() {
            skills.push(Skill {
                name,
                description,
                path: PathBuf::from(location),
            });
        }

        pos = skill_end;
    }

    skills
}

fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text.find(&close)?;
    if start <= end {
        Some(text[start..end].trim().to_string())
    } else {
        None
    }
}

/// build the full project context for a working directory
pub fn discover_project_context(cwd: &Path) -> ProjectContext {
    let agents_md = find_agents_md(cwd);

    let mut skills = Vec::new();
    for agents in &agents_md {
        skills.extend(parse_skills_from_agents_md(&agents.content));
    }

    ProjectContext { agents_md, skills }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skills_from_content() {
        let content = r#"
some preamble

<available_skills>
  <skill>
    <name>rust</name>
    <description>Rust conventions</description>
    <location>/home/user/.pi/agent/skills/rust/SKILL.md</location>
  </skill>
  <skill>
    <name>nix</name>
    <description>Nix best practices</description>
    <location>/home/user/.pi/agent/skills/nix/SKILL.md</location>
  </skill>
</available_skills>

more content
"#;

        let skills = parse_skills_from_agents_md(content);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "rust");
        assert_eq!(skills[1].name, "nix");
        assert!(skills[0].path.to_str().unwrap().contains("rust"));
    }

    #[test]
    fn parse_skills_no_block() {
        let content = "just some regular markdown";
        let skills = parse_skills_from_agents_md(content);
        assert!(skills.is_empty());
    }

    #[test]
    fn extract_tag_works() {
        assert_eq!(
            extract_tag("<name>hello</name>", "name"),
            Some("hello".into())
        );
        assert_eq!(extract_tag("no tags here", "name"), None);
    }

    #[test]
    fn find_agents_md_in_temp() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# test agents").unwrap();

        let results = find_agents_md(dir.path());
        assert!(!results.is_empty());
        assert!(results.last().unwrap().content.contains("test agents"));
    }
}
