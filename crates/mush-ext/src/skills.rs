//! on-demand skill loader with caching
//!
//! skills are SKILL.md files discovered from AGENTS.md. their content
//! is loaded lazily when requested and cached for the session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::loader::Skill;

/// loads and caches skill file content
#[derive(Debug, Default)]
pub struct SkillLoader {
    /// known skills (from AGENTS.md discovery)
    skills: Vec<Skill>,
    /// cached content keyed by skill name
    cache: HashMap<String, String>,
}

impl SkillLoader {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills,
            cache: HashMap::new(),
        }
    }

    /// get a skill by name (case-insensitive)
    pub fn find(&self, name: &str) -> Option<&Skill> {
        let lower = name.to_lowercase();
        self.skills.iter().find(|s| s.name.to_lowercase() == lower)
    }

    /// load skill content, returning cached if available
    pub fn load(&mut self, name: &str) -> Option<String> {
        if let Some(cached) = self.cache.get(name) {
            return Some(cached.clone());
        }

        let skill = self.find(name)?.clone();
        let content = read_skill_file(&skill.path)?;
        self.cache.insert(name.to_string(), content.clone());
        Some(content)
    }

    /// load skill content and resolve any relative path references
    /// against the skill directory
    pub fn load_with_context(&mut self, name: &str) -> Option<SkillContent> {
        let skill = self.find(name)?.clone();
        let content = self.load(name)?;
        let skill_dir = skill.path.parent().map(|p| p.to_path_buf());

        Some(SkillContent {
            name: skill.name.clone(),
            description: skill.description.clone(),
            content,
            skill_dir,
        })
    }

    /// list all available skill names and descriptions
    pub fn list(&self) -> Vec<(&str, &str)> {
        self.skills
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str()))
            .collect()
    }

    /// whether a skill has been loaded into cache
    pub fn is_cached(&self, name: &str) -> bool {
        self.cache.contains_key(name)
    }
}

/// loaded skill with resolved context
#[derive(Debug, Clone)]
pub struct SkillContent {
    pub name: String,
    pub description: String,
    pub content: String,
    /// directory containing the SKILL.md, for resolving relative paths
    pub skill_dir: Option<PathBuf>,
}

fn read_skill_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill(name: &str, desc: &str, path: &Path) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            path: path.to_path_buf(),
        }
    }

    #[test]
    fn find_by_name() {
        let skills = vec![
            make_skill("rust", "rust stuff", Path::new("/skills/rust/SKILL.md")),
            make_skill("nix", "nix stuff", Path::new("/skills/nix/SKILL.md")),
        ];
        let loader = SkillLoader::new(skills);

        assert!(loader.find("rust").is_some());
        assert!(loader.find("Rust").is_some()); // case-insensitive
        assert!(loader.find("python").is_none());
    }

    #[test]
    fn list_skills() {
        let skills = vec![
            make_skill("rust", "rust stuff", Path::new("/a")),
            make_skill("nix", "nix stuff", Path::new("/b")),
        ];
        let loader = SkillLoader::new(skills);

        let list = loader.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0, "rust");
    }

    #[test]
    fn load_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("rust");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(&skill_path, "# rust skill\nuse cargo").unwrap();

        let skills = vec![make_skill("rust", "rust stuff", &skill_path)];
        let mut loader = SkillLoader::new(skills);

        assert!(!loader.is_cached("rust"));
        let content = loader.load("rust").unwrap();
        assert!(content.contains("cargo"));
        assert!(loader.is_cached("rust"));

        // second load uses cache
        let cached = loader.load("rust").unwrap();
        assert_eq!(content, cached);
    }

    #[test]
    fn load_with_context_resolves_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("nix");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(&skill_path, "# nix\nflake stuff").unwrap();

        let skills = vec![make_skill("nix", "nix help", &skill_path)];
        let mut loader = SkillLoader::new(skills);

        let ctx = loader.load_with_context("nix").unwrap();
        assert_eq!(ctx.name, "nix");
        assert_eq!(ctx.skill_dir.unwrap(), skill_dir);
    }

    #[test]
    fn load_missing_skill_returns_none() {
        let mut loader = SkillLoader::new(vec![]);
        assert!(loader.load("nonexistent").is_none());
    }

    #[test]
    fn load_missing_file_returns_none() {
        let skills = vec![make_skill(
            "gone",
            "missing",
            Path::new("/does/not/exist/SKILL.md"),
        )];
        let mut loader = SkillLoader::new(skills);
        assert!(loader.load("gone").is_none());
    }
}
