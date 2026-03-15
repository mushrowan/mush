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

/// resolve the mush config directory (XDG-aware)
fn resolve_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("mush"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/mush"))
}

/// derive config dir from an explicit home (for testing, no env vars)
fn config_dir_from_home(home: &Path) -> PathBuf {
    home.join(".config/mush")
}

/// scan for AGENTS.md in the project directory and up to home
pub fn find_agents_md(cwd: &Path) -> Vec<AgentsMd> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_dir = resolve_config_dir();
    find_agents_md_inner(cwd, home.as_deref(), config_dir.as_deref())
}

/// inner implementation with explicit home and config dir
fn find_agents_md_inner(
    cwd: &Path,
    home: Option<&Path>,
    config_dir: Option<&Path>,
) -> Vec<AgentsMd> {
    let mut results = Vec::new();

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
        if home.is_some_and(|h| d == h) {
            break;
        }

        dir = d.parent().map(|p| p.to_path_buf());
    }

    // also check ~/.config/mush/AGENTS.md (user-global)
    if let Some(cd) = config_dir {
        let mush_agents = cd.join("AGENTS.md");
        if mush_agents.is_file()
            && !results.iter().any(|a| a.path == mush_agents)
            && let Ok(content) = std::fs::read_to_string(&mush_agents)
        {
            results.push(AgentsMd {
                path: mush_agents,
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
        let Some(skill_end) = block[skill_start..].find("</skill>") else {
            break;
        };
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

/// parse SKILL.md frontmatter (yaml between `---` fences)
///
/// extracts `name` and `description` fields
fn parse_skill_frontmatter(content: &str) -> Option<(String, String)> {
    let (frontmatter, _) = crate::frontmatter::extract(content)?;

    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().to_string());
        }
    }

    Some((name?, description?))
}

/// scan a directory for SKILL.md files in subdirectories
///
/// matches pi's discovery: each subdirectory containing a SKILL.md
/// with valid frontmatter (name + description) is a skill
fn scan_skills_dir(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&skill_md) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some((name, description)) = parse_skill_frontmatter(&content) {
            skills.push(Skill {
                name,
                description,
                path: skill_md,
            });
        }
    }

    skills
}

/// build the full project context for a working directory
///
/// discovers skills from:
/// 1. `<available_skills>` blocks in AGENTS.md files (explicit)
/// 2. `~/.config/mush/skills/` directory (user-global)
/// 3. `.mush/skills/` in the project directory (project-local)
pub fn discover_project_context(cwd: &Path) -> ProjectContext {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_dir = resolve_config_dir();
    discover_project_context_inner(cwd, home.as_deref(), config_dir.as_deref())
}

/// testable variant that accepts an explicit home directory
pub fn discover_project_context_with_home(cwd: &Path, home: Option<&Path>) -> ProjectContext {
    let config_dir = home.map(config_dir_from_home);
    discover_project_context_inner(cwd, home, config_dir.as_deref())
}

fn discover_project_context_inner(
    cwd: &Path,
    home: Option<&Path>,
    config_dir: Option<&Path>,
) -> ProjectContext {
    let agents_md = find_agents_md_inner(cwd, home, config_dir);

    let mut skills = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // explicit skills from AGENTS.md
    for agents in &agents_md {
        for skill in parse_skills_from_agents_md(&agents.content) {
            seen_names.insert(skill.name.clone());
            skills.push(skill);
        }
    }

    // user-global skills directory (~/.config/mush/skills/)
    if let Some(cd) = config_dir {
        for skill in scan_skills_dir(&cd.join("skills")) {
            if seen_names.insert(skill.name.clone()) {
                skills.push(skill);
            }
        }
    }

    // project-local skills (.mush/skills/)
    let project_skills = cwd.join(".mush/skills");
    for skill in scan_skills_dir(&project_skills) {
        if seen_names.insert(skill.name.clone()) {
            skills.push(skill);
        }
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
    <location>/home/user/.config/mush/skills/rust/SKILL.md</location>
  </skill>
  <skill>
    <name>nix</name>
    <description>Nix best practices</description>
    <location>/home/user/.config/mush/skills/nix/SKILL.md</location>
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

    #[test]
    fn discover_global_uses_mush_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // set up ~/.config/mush/AGENTS.md
        let mush_agents = home.join(".config/mush/AGENTS.md");
        std::fs::create_dir_all(mush_agents.parent().unwrap()).unwrap();
        std::fs::write(&mush_agents, "# mush agents").unwrap();

        // set up ~/.config/mush/skills/
        let mush_skills = home.join(".config/mush/skills/rust");
        std::fs::create_dir_all(&mush_skills).unwrap();
        std::fs::write(
            mush_skills.join("SKILL.md"),
            "---\nname: rust\ndescription: Rust conventions\n---\ncontent",
        )
        .unwrap();

        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let ctx = discover_project_context_with_home(&cwd, Some(home));

        assert!(
            ctx.agents_md.iter().any(|a| a.path == mush_agents),
            "expected to find ~/.config/mush/AGENTS.md, got: {:?}",
            ctx.agents_md.iter().map(|a| &a.path).collect::<Vec<_>>()
        );
        assert!(
            ctx.skills.iter().any(|s| s.name == "rust"),
            "expected to find rust skill from ~/.config/mush/skills/"
        );
    }

    #[test]
    fn discover_project_local_uses_mush_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        // set up .mush/skills/ (project-local)
        let skill_dir = cwd.join(".mush/skills/local-only");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: local-only\ndescription: test skill\n---\ncontent",
        )
        .unwrap();

        // use fake home so we don't pick up real skills
        let ctx = discover_project_context_with_home(&cwd, Some(home));
        assert!(
            ctx.skills.iter().any(|s| s.name == "local-only"),
            "expected to find local-only skill from .mush/skills/, got: {:?}",
            ctx.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_skill_frontmatter_valid() {
        let content = "---\nname: rust\ndescription: Rust conventions\n---\n\n# content";
        let (name, desc) = parse_skill_frontmatter(content).unwrap();
        assert_eq!(name, "rust");
        assert_eq!(desc, "Rust conventions");
    }

    #[test]
    fn parse_skill_frontmatter_missing_fields() {
        assert!(parse_skill_frontmatter("---\nname: rust\n---").is_none());
        assert!(parse_skill_frontmatter("no frontmatter").is_none());
    }

    #[test]
    fn scan_skills_dir_finds_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("rust");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: rust\ndescription: Rust conventions\n---\n\ncontent",
        )
        .unwrap();

        let skills = scan_skills_dir(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "rust");
        assert_eq!(skills[0].description, "Rust conventions");
    }

    #[test]
    fn scan_skills_dir_skips_invalid() {
        let dir = tempfile::tempdir().unwrap();
        // dir without SKILL.md
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        // SKILL.md without frontmatter
        let bad = dir.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "no frontmatter here").unwrap();

        let skills = scan_skills_dir(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn scan_skills_dir_nonexistent() {
        let skills = scan_skills_dir(Path::new("/does/not/exist"));
        assert!(skills.is_empty());
    }
}
