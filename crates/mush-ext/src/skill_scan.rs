use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::loader::Skill;

pub const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillSource {
    Cli,
    Project,
    User,
    AgentsMd,
    Bundled,
}

impl SkillSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Project => "project",
            Self::User => "user",
            Self::AgentsMd => "agents.md",
            Self::Bundled => "bundled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillDiagnosticKind {
    Warning(String),
    Collision { name: String, winner: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub kind: SkillDiagnosticKind,
}

impl SkillDiagnostic {
    fn warning(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            kind: SkillDiagnosticKind::Warning(message.into()),
        }
    }
}

impl std::fmt::Display for SkillDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            SkillDiagnosticKind::Warning(message) => {
                write!(f, "{}: {message}", self.path.display())
            }
            SkillDiagnosticKind::Collision { name, winner } => write!(
                f,
                "{}: skill \"{name}\" already loaded from {}",
                self.path.display(),
                winner.display()
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

#[derive(Debug, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    disable_model_invocation: bool,
}

fn parse_frontmatter(content: &str) -> Option<Frontmatter> {
    let (frontmatter, _) = crate::frontmatter::extract(content)?;
    let mut parsed = Frontmatter::default();
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            parsed.name = Some(value.trim().trim_matches(['"', '\'']).to_string());
        } else if let Some(value) = line.strip_prefix("description:") {
            parsed.description = Some(value.trim().trim_matches(['"', '\'']).to_string());
        } else if let Some(value) = line.strip_prefix("disable-model-invocation:") {
            parsed.disable_model_invocation = value.trim() == "true";
        }
    }
    Some(parsed)
}

#[must_use]
pub fn validate_name(name: &str, parent_dir: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir}\""
        ));
    }
    if name.len() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push("name must be lowercase a-z, 0-9 and hyphens only".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

fn validate_description(description: Option<&str>) -> Vec<String> {
    match description.map(str::trim) {
        None | Some("") => vec!["description is required".to_string()],
        Some(description) if description.len() > MAX_DESCRIPTION_LENGTH => vec![format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            description.len()
        )],
        Some(_) => Vec::new(),
    }
}

pub fn load_skill_file(path: &Path, source: SkillSource) -> ScanResult {
    match std::fs::read_to_string(path) {
        Ok(content) => parse_skill(&content, path, source),
        Err(error) => ScanResult {
            skills: Vec::new(),
            diagnostics: vec![SkillDiagnostic::warning(path, error.to_string())],
        },
    }
}

pub fn parse_skill(content: &str, path: &Path, source: SkillSource) -> ScanResult {
    let mut result = ScanResult::default();
    let Some(frontmatter) = parse_frontmatter(content) else {
        result
            .diagnostics
            .push(SkillDiagnostic::warning(path, "missing frontmatter"));
        return result;
    };

    let parent_dir = path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    for error in validate_description(frontmatter.description.as_deref()) {
        result
            .diagnostics
            .push(SkillDiagnostic::warning(path, error));
    }
    let name = frontmatter
        .name
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| parent_dir.clone());
    for error in validate_name(&name, &parent_dir) {
        result
            .diagnostics
            .push(SkillDiagnostic::warning(path, error));
    }

    let Some(description) = frontmatter
        .description
        .filter(|description| !description.trim().is_empty())
    else {
        return result;
    };

    result.skills.push(Skill {
        name,
        description,
        path: path.to_path_buf(),
        source,
        disable_model_invocation: frontmatter.disable_model_invocation,
    });
    result
}

fn parent_holds_skill_md(entry: &ignore::DirEntry) -> bool {
    entry.depth() > 1
        && entry
            .path()
            .parent()
            .is_some_and(|parent| parent.join("SKILL.md").is_file())
}

pub fn scan_skills_root(dir: &Path, source: SkillSource) -> ScanResult {
    let mut result = ScanResult::default();
    if !dir.is_dir() {
        return result;
    }
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .ignore(true)
        .add_custom_ignore_filename(".fdignore")
        .follow_links(true)
        .filter_entry(|entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            if !is_dir {
                return true;
            }
            entry.file_name() != "node_modules" && !parent_holds_skill_md(entry)
        })
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result
                    .diagnostics
                    .push(SkillDiagnostic::warning(dir, error.to_string()));
                continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let is_skill_md = path.file_name().is_some_and(|name| name == "SKILL.md");
        let is_root_md =
            entry.depth() == 1 && path.extension().is_some_and(|extension| extension == "md");
        if !(is_skill_md || is_root_md) {
            continue;
        }
        if !is_skill_md && dir.join("SKILL.md").is_file() {
            continue;
        }
        let loaded = load_skill_file(path, source);
        result.skills.extend(loaded.skills);
        result.diagnostics.extend(loaded.diagnostics);
    }
    result
}

#[derive(Debug, Default)]
pub struct SkillSet {
    skills: Vec<Skill>,
    by_name: HashMap<String, usize>,
    seen_paths: HashSet<PathBuf>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

impl SkillSet {
    pub fn add(&mut self, result: ScanResult) {
        self.diagnostics.extend(result.diagnostics);
        for skill in result.skills {
            self.insert(skill);
        }
    }

    pub fn insert(&mut self, skill: Skill) {
        let canonical = std::fs::canonicalize(&skill.path).unwrap_or_else(|_| skill.path.clone());
        if !self.seen_paths.insert(canonical) {
            return;
        }
        let key = skill.name.to_lowercase();
        if let Some(&index) = self.by_name.get(&key) {
            self.diagnostics.push(SkillDiagnostic {
                path: skill.path,
                kind: SkillDiagnosticKind::Collision {
                    name: skill.name,
                    winner: self.skills[index].path.clone(),
                },
            });
            return;
        }
        self.by_name.insert(key, self.skills.len());
        self.skills.push(skill);
    }

    #[must_use]
    pub fn into_skills(self) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
        (self.skills, self.diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, frontmatter: &str) -> PathBuf {
        let skill_dir = root.join(dir);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, format!("---\n{frontmatter}\n---\nbody")).unwrap();
        path
    }

    fn warnings(result: &ScanResult) -> Vec<String> {
        result
            .diagnostics
            .iter()
            .filter_map(|d| match &d.kind {
                SkillDiagnosticKind::Warning(message) => Some(message.clone()),
                SkillDiagnosticKind::Collision { .. } => None,
            })
            .collect()
    }

    #[test]
    fn a_missing_description_drops_the_skill() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "quiet", "name: quiet");

        let result = load_skill_file(&path, SkillSource::User);

        assert!(result.skills.is_empty());
        assert_eq!(warnings(&result), ["description is required"]);
    }

    #[test]
    fn a_bad_name_warns_but_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "My_Skill", "name: My_Skill\ndescription: d");

        let result = load_skill_file(&path, SkillSource::User);

        assert_eq!(result.skills.len(), 1);
        assert_eq!(
            warnings(&result),
            ["name must be lowercase a-z, 0-9 and hyphens only"]
        );
    }

    #[test]
    fn the_name_defaults_to_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "from-dir", "description: d");

        let result = load_skill_file(&path, SkillSource::User);

        assert_eq!(result.skills[0].name, "from-dir");
        assert!(warnings(&result).is_empty());
    }

    #[test]
    fn name_rules_match_the_agent_skills_spec() {
        assert_eq!(
            validate_name("other", "dir"),
            ["name \"other\" does not match parent directory \"dir\""]
        );
        assert_eq!(
            validate_name("-lead", "-lead"),
            ["name must not start or end with a hyphen"]
        );
        assert_eq!(
            validate_name("a--b", "a--b"),
            ["name must not contain consecutive hyphens"]
        );
        let long = "a".repeat(65);
        assert_eq!(
            validate_name(&long, &long),
            ["name exceeds 64 characters (65)"]
        );
        assert!(validate_name("ok-name-2", "ok-name-2").is_empty());
    }

    #[test]
    fn a_long_description_warns_but_loads() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(1025);
        let path = write_skill(
            dir.path(),
            "wordy",
            &format!("name: wordy\ndescription: {long}"),
        );

        let result = load_skill_file(&path, SkillSource::User);

        assert_eq!(result.skills.len(), 1);
        assert_eq!(
            warnings(&result),
            ["description exceeds 1024 characters (1025)"]
        );
    }

    #[test]
    fn disable_model_invocation_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(
            dir.path(),
            "hidden",
            "name: hidden\ndescription: d\ndisable-model-invocation: true",
        );

        let result = load_skill_file(&path, SkillSource::User);

        assert!(result.skills[0].disable_model_invocation);
    }

    #[test]
    fn a_skill_dir_stops_the_recursion() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "outer", "name: outer\ndescription: d");
        write_skill(dir.path(), "outer/inner", "name: inner\ndescription: d");

        let result = scan_skills_root(dir.path(), SkillSource::User);

        let names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["outer"]);
    }

    #[test]
    fn nested_skills_are_found_through_plain_directories() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "group/deep", "name: deep\ndescription: d");

        let result = scan_skills_root(dir.path(), SkillSource::User);

        assert_eq!(result.skills[0].name, "deep");
    }

    #[test]
    fn dot_dirs_and_node_modules_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), ".hidden/secret", "name: secret\ndescription: d");
        write_skill(dir.path(), "node_modules/pkg", "name: pkg\ndescription: d");
        write_skill(dir.path(), "seen", "name: seen\ndescription: d");

        let result = scan_skills_root(dir.path(), SkillSource::User);

        let names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["seen"]);
    }

    #[test]
    fn gitignored_skills_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        write_skill(dir.path(), "ignored/gone", "name: gone\ndescription: d");
        write_skill(dir.path(), "kept", "name: kept\ndescription: d");

        let result = scan_skills_root(dir.path(), SkillSource::User);

        let names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["kept"]);
    }

    #[test]
    fn root_markdown_files_are_skills_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("loose.md"),
            "---\nname: loose\ndescription: d\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("group")).unwrap();
        std::fs::write(
            dir.path().join("group/notes.md"),
            "---\nname: notes\ndescription: d\n---\nbody",
        )
        .unwrap();

        let result = scan_skills_root(dir.path(), SkillSource::User);

        let names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["loose"]);
    }

    #[test]
    fn symlinked_skill_dirs_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("store/real");
        write_skill(dir.path(), "store/real", "name: linked\ndescription: d");
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&real, skills.join("linked")).unwrap();

        let result = scan_skills_root(&skills, SkillSource::User);

        assert_eq!(result.skills[0].name, "linked");
    }

    #[test]
    fn the_first_skill_with_a_name_wins() {
        let dir = tempfile::tempdir().unwrap();
        let project = write_skill(dir.path(), "project/shared", "name: shared\ndescription: p");
        let user = write_skill(dir.path(), "user/shared", "name: shared\ndescription: u");
        let mut set = SkillSet::default();

        set.add(load_skill_file(&project, SkillSource::Project));
        set.add(load_skill_file(&user, SkillSource::User));
        let (skills, diagnostics) = set.into_skills();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "p");
        assert_eq!(
            diagnostics,
            [SkillDiagnostic {
                path: user,
                kind: SkillDiagnosticKind::Collision {
                    name: "shared".into(),
                    winner: project,
                },
            }]
        );
    }

    #[test]
    fn the_same_file_reached_twice_loads_once_without_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let real = write_skill(dir.path(), "real/twice", "name: twice\ndescription: d");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        let mut set = SkillSet::default();

        set.add(load_skill_file(&real, SkillSource::User));
        set.add(load_skill_file(
            &dir.path().join("alias/twice/SKILL.md"),
            SkillSource::User,
        ));
        let (skills, diagnostics) = set.into_skills();

        assert_eq!(skills.len(), 1);
        assert!(diagnostics.is_empty());
    }
}
