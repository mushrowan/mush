use std::path::{Component, Path, PathBuf};

use crate::skill_scan::{SkillDiagnostic, SkillSet, SkillSource, scan_skills_root};

#[derive(Debug, Default)]
pub struct ProjectContext {
    pub agents_md: Vec<AgentsMd>,
    pub skills: Vec<Skill>,
    pub skill_diagnostics: Vec<SkillDiagnostic>,
    pub system_prompt: Option<PromptFile>,
    pub append_system_prompt: Option<PromptFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptFile {
    pub path: PathBuf,
    pub content: String,
}

const CONTEXT_FILE_NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

fn read_context_file(dir: &Path) -> Option<AgentsMd> {
    CONTEXT_FILE_NAMES.iter().find_map(|name| {
        let path = dir.join(name);
        let content = std::fs::read_to_string(&path).ok()?;
        Some(AgentsMd { path, content })
    })
}

fn find_prompt_file(name: &str, cwd: &Path, config_dir: Option<&Path>) -> Option<PromptFile> {
    std::iter::once(cwd.join(".mush"))
        .chain(config_dir.map(Path::to_path_buf))
        .find_map(|dir| {
            let path = dir.join(name);
            let content = std::fs::read_to_string(&path).ok()?;
            Some(PromptFile { path, content })
        })
}

#[derive(Debug, Clone)]
pub struct AgentsMd {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub source: SkillSource,
    pub disable_model_invocation: bool,
}

fn resolve_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("mush"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/mush"))
}

fn config_dir_from_home(home: &Path) -> PathBuf {
    home.join(".config/mush")
}

pub fn find_agents_md(cwd: &Path) -> Vec<AgentsMd> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_dir = resolve_config_dir();
    find_agents_md_inner(cwd, home.as_deref(), config_dir.as_deref())
}

pub fn find_nested_agents_md(cwd: &Path, target: &Path) -> Vec<AgentsMd> {
    let target_dir = if target.is_dir() {
        target
    } else if let Some(parent) = target.parent() {
        parent
    } else {
        return Vec::new();
    };

    let Ok(relative) = target_dir.strip_prefix(cwd) else {
        return Vec::new();
    };

    let mut current = cwd.to_path_buf();
    let mut results = Vec::new();

    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        results.extend(read_context_file(&current));
    }

    results
}

fn find_agents_md_inner(
    cwd: &Path,
    home: Option<&Path>,
    config_dir: Option<&Path>,
) -> Vec<AgentsMd> {
    let mut results = Vec::new();

    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        results.extend(read_context_file(&d));

        if home.is_some_and(|h| d == h) {
            break;
        }

        dir = d.parent().map(|p| p.to_path_buf());
    }

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

    results.reverse();
    results
}

pub fn parse_skills_from_agents_md(content: &str) -> Vec<Skill> {
    let mut skills = Vec::new();

    let Some(start) = content.find("<available_skills>") else {
        return skills;
    };
    let Some(end) = content.find("</available_skills>") else {
        return skills;
    };

    let block = &content[start..end];

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
                source: SkillSource::AgentsMd,
                disable_model_invocation: false,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryOptions {
    pub skill_paths: Vec<PathBuf>,
    pub no_skills: bool,
    pub no_context_files: bool,
}

#[tracing::instrument(name = "discover_context", skip_all)]
pub fn discover_project_context(cwd: &Path) -> ProjectContext {
    discover_project_context_with(cwd, &DiscoveryOptions::default())
}

pub fn discover_project_context_with(cwd: &Path, options: &DiscoveryOptions) -> ProjectContext {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_dir = resolve_config_dir();
    discover_project_context_inner(cwd, home.as_deref(), config_dir.as_deref(), options)
}

pub fn discover_project_context_with_home(cwd: &Path, home: Option<&Path>) -> ProjectContext {
    let config_dir = home.map(config_dir_from_home);
    discover_project_context_inner(
        cwd,
        home,
        config_dir.as_deref(),
        &DiscoveryOptions::default(),
    )
}

fn ancestor_dirs_to_home(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(cwd.to_path_buf());

    while let Some(dir) = current {
        dirs.push(dir.clone());
        if home.is_some_and(|home| dir == home) {
            break;
        }
        current = dir.parent().map(Path::to_path_buf);
    }

    dirs.reverse();
    dirs
}

fn discover_project_context_inner(
    cwd: &Path,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    options: &DiscoveryOptions,
) -> ProjectContext {
    let agents_md = if options.no_context_files {
        Vec::new()
    } else {
        find_agents_md_inner(cwd, home, config_dir)
    };

    let mut set = SkillSet::default();

    for path in &options.skill_paths {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        if path.is_dir() {
            set.add(scan_skills_root(&path, SkillSource::Cli));
        } else if path.is_file() {
            set.add(crate::skill_scan::load_skill_file(&path, SkillSource::Cli));
        } else {
            set.diagnostics.push(SkillDiagnostic {
                path,
                kind: crate::skill_scan::SkillDiagnosticKind::Warning(
                    "skill path does not exist".into(),
                ),
            });
        }
    }

    if !options.no_skills {
        for dir in ancestor_dirs_to_home(cwd, home).iter().rev() {
            set.add(scan_skills_root(
                &dir.join(".mush/skills"),
                SkillSource::Project,
            ));
            set.add(scan_skills_root(
                &dir.join(".agents/skills"),
                SkillSource::Project,
            ));
        }

        if let Some(cd) = config_dir {
            set.add(scan_skills_root(&cd.join("skills"), SkillSource::User));
        }
        if let Some(home) = home {
            set.add(scan_skills_root(
                &home.join(".agents/skills"),
                SkillSource::User,
            ));
        }

        for agents in &agents_md {
            for skill in parse_skills_from_agents_md(&agents.content) {
                set.insert(skill);
            }
        }
    }

    let (skills, skill_diagnostics) = set.into_skills();
    ProjectContext {
        agents_md,
        skills,
        skill_diagnostics,
        system_prompt: find_prompt_file("SYSTEM.md", cwd, config_dir),
        append_system_prompt: find_prompt_file("APPEND_SYSTEM.md", cwd, config_dir),
    }
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
    fn find_nested_agents_md_walks_from_cwd_to_target() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let target_dir = cwd.join("src/lib");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(cwd.join("src/AGENTS.md"), "# src agents").unwrap();
        std::fs::write(target_dir.join("AGENTS.md"), "# lib agents").unwrap();

        let found = find_nested_agents_md(&cwd, &target_dir.join("main.rs"));
        let paths: Vec<_> = found.iter().map(|agents| agents.path.clone()).collect();

        assert_eq!(
            paths,
            vec![cwd.join("src/AGENTS.md"), target_dir.join("AGENTS.md")]
        );
    }

    #[test]
    fn discover_global_uses_mush_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        let mush_agents = home.join(".config/mush/AGENTS.md");
        std::fs::create_dir_all(mush_agents.parent().unwrap()).unwrap();
        std::fs::write(&mush_agents, "# mush agents").unwrap();

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

        let skill_dir = cwd.join(".mush/skills/local-only");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: local-only\ndescription: test skill\n---\ncontent",
        )
        .unwrap();

        let ctx = discover_project_context_with_home(&cwd, Some(home));
        assert!(
            ctx.skills.iter().any(|s| s.name == "local-only"),
            "expected to find local-only skill from .mush/skills/, got: {:?}",
            ctx.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn discover_project_local_walks_up_for_mush_skills() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let repo = home.join("repo");
        let cwd = repo.join("src/bin");
        std::fs::create_dir_all(&cwd).unwrap();

        let root_skill = repo.join(".mush/skills/root-only");
        std::fs::create_dir_all(&root_skill).unwrap();
        std::fs::write(
            root_skill.join("SKILL.md"),
            "---\nname: root-only\ndescription: root skill\n---\ncontent",
        )
        .unwrap();

        let nested_skill = repo.join("src/.mush/skills/nested-only");
        std::fs::create_dir_all(&nested_skill).unwrap();
        std::fs::write(
            nested_skill.join("SKILL.md"),
            "---\nname: nested-only\ndescription: nested skill\n---\ncontent",
        )
        .unwrap();

        let ctx = discover_project_context_with_home(&cwd, Some(home));
        let names: Vec<_> = ctx.skills.iter().map(|skill| skill.name.as_str()).collect();

        assert!(names.contains(&"root-only"));
        assert!(names.contains(&"nested-only"));
    }

    #[test]
    fn discover_project_local_prefers_nearest_duplicate_skill_name() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let repo = home.join("repo");
        let cwd = repo.join("src/bin");
        std::fs::create_dir_all(&cwd).unwrap();

        let root_skill = repo.join(".mush/skills/shared");
        std::fs::create_dir_all(&root_skill).unwrap();
        std::fs::write(
            root_skill.join("SKILL.md"),
            "---\nname: shared\ndescription: root version\n---\ncontent",
        )
        .unwrap();

        let nested_skill = repo.join("src/.mush/skills/shared");
        std::fs::create_dir_all(&nested_skill).unwrap();
        std::fs::write(
            nested_skill.join("SKILL.md"),
            "---\nname: shared\ndescription: nested version\n---\ncontent",
        )
        .unwrap();

        let ctx = discover_project_context_with_home(&cwd, Some(home));
        let skill = ctx
            .skills
            .iter()
            .find(|skill| skill.name == "shared")
            .unwrap();

        assert_eq!(skill.description, "nested version");
        assert_eq!(skill.path, nested_skill.join("SKILL.md"));
    }

    #[test]
    fn project_skills_beat_user_skills_and_the_loser_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let cwd = home.join("repo");
        let project = cwd.join(".mush/skills/shared");
        let user = home.join(".config/mush/skills/shared");
        for (path, description) in [(&project, "project"), (&user, "user")] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(
                path.join("SKILL.md"),
                format!("---\nname: shared\ndescription: {description}\n---\nbody"),
            )
            .unwrap();
        }

        let ctx = discover_project_context_with_home(&cwd, Some(home));

        let skill = ctx.skills.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(skill.description, "project");
        assert_eq!(skill.source, SkillSource::Project);
        assert_eq!(ctx.skill_diagnostics.len(), 1);
        assert_eq!(ctx.skill_diagnostics[0].path, user.join("SKILL.md"));
    }

    #[test]
    fn claude_md_stands_in_when_there_is_no_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let cwd = home.join("repo/sub");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("repo/CLAUDE.md"), "claude rules").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "agents rules").unwrap();
        std::fs::write(cwd.join("CLAUDE.md"), "ignored twin").unwrap();

        let ctx = discover_project_context_with_home(&cwd, Some(home));

        let contents: Vec<_> = ctx.agents_md.iter().map(|a| a.content.as_str()).collect();
        assert_eq!(contents, ["claude rules", "agents rules"]);
    }

    #[test]
    fn a_project_system_md_beats_the_global_one() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let cwd = home.join("repo");
        std::fs::create_dir_all(cwd.join(".mush")).unwrap();
        std::fs::create_dir_all(home.join(".config/mush")).unwrap();
        std::fs::write(cwd.join(".mush/SYSTEM.md"), "project prompt").unwrap();
        std::fs::write(home.join(".config/mush/SYSTEM.md"), "global prompt").unwrap();
        std::fs::write(home.join(".config/mush/APPEND_SYSTEM.md"), "global extra").unwrap();

        let ctx = discover_project_context_with_home(&cwd, Some(home));

        assert_eq!(ctx.system_prompt.unwrap().content, "project prompt");
        assert_eq!(ctx.append_system_prompt.unwrap().content, "global extra");
    }

    #[test]
    fn a_cli_skill_path_loads_even_with_no_skills_and_beats_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let cwd = home.join("repo");
        let project = cwd.join(".mush/skills/shared");
        let explicit = home.join("elsewhere/shared");
        for (path, description) in [(&project, "project"), (&explicit, "cli")] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(
                path.join("SKILL.md"),
                format!("---\nname: shared\ndescription: {description}\n---\nbody"),
            )
            .unwrap();
        }
        let config_dir = config_dir_from_home(home);

        let with_defaults = discover_project_context_inner(
            &cwd,
            Some(home),
            Some(&config_dir),
            &DiscoveryOptions {
                skill_paths: vec![explicit.clone()],
                ..Default::default()
            },
        );
        let without = discover_project_context_inner(
            &cwd,
            Some(home),
            Some(&config_dir),
            &DiscoveryOptions {
                skill_paths: vec![home.join("elsewhere")],
                no_skills: true,
                no_context_files: true,
            },
        );

        let shared = with_defaults
            .skills
            .iter()
            .find(|s| s.name == "shared")
            .unwrap();
        assert_eq!(shared.description, "cli");
        assert_eq!(shared.source, SkillSource::Cli);
        assert_eq!(without.skills.len(), 1);
        assert_eq!(without.skills[0].description, "cli");
    }

    #[test]
    fn a_missing_cli_skill_path_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let ctx = discover_project_context_inner(
            &cwd,
            Some(dir.path()),
            None,
            &DiscoveryOptions {
                skill_paths: vec![PathBuf::from("nope")],
                ..Default::default()
            },
        );

        assert_eq!(ctx.skill_diagnostics.len(), 1);
        assert_eq!(ctx.skill_diagnostics[0].path, cwd.join("nope"));
    }

    #[test]
    fn agents_skills_dirs_are_discovered_for_project_and_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let cwd = home.join("repo");
        for (path, name) in [
            (cwd.join(".agents/skills/repo-skill"), "repo-skill"),
            (home.join(".agents/skills/home-skill"), "home-skill"),
        ] {
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\n---\nbody"),
            )
            .unwrap();
        }

        let ctx = discover_project_context_with_home(&cwd, Some(home));

        let mut names: Vec<_> = ctx.skills.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["home-skill", "repo-skill"]);
    }
}
