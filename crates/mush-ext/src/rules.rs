//! per-file rule auto-attachment
//!
//! discovers rule files from `.mush/rules/` with glob patterns in
//! yaml frontmatter. when a file path matches a rule's globs, the
//! rule content is injected into context.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use globset::{Glob, GlobSet, GlobSetBuilder};

/// a rule file with glob patterns and content
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub globs: Vec<String>,
    pub content: String,
    pub path: PathBuf,
}

/// index of rules for fast glob matching
pub struct RuleIndex {
    rules: Vec<Rule>,
    globset: GlobSet,
    /// which glob index maps to which rule index
    glob_to_rule: Vec<usize>,
    /// rules already injected (by index), avoids duplication
    injected: Mutex<Vec<bool>>,
}

impl RuleIndex {
    /// build from a list of rules, compiling all globs into a single set
    pub fn new(rules: Vec<Rule>) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut glob_to_rule = Vec::new();

        for (rule_idx, rule) in rules.iter().enumerate() {
            for pattern in &rule.globs {
                if let Ok(glob) = Glob::new(pattern) {
                    builder.add(glob);
                    glob_to_rule.push(rule_idx);
                }
            }
        }

        let globset = builder
            .build()
            .unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap());
        let injected = Mutex::new(vec![false; rules.len()]);

        Self {
            rules,
            globset,
            glob_to_rule,
            injected,
        }
    }

    /// check a file path and return any matching rules not yet injected
    pub fn match_file(&self, path: &Path) -> Vec<&Rule> {
        let matches = self.globset.matches(path);
        let mut injected = self.injected.lock().unwrap();
        let mut result = Vec::new();
        let mut seen = Vec::new();

        for &glob_idx in &matches {
            let rule_idx = self.glob_to_rule[glob_idx];
            if !injected[rule_idx] && !seen.contains(&rule_idx) {
                seen.push(rule_idx);
                result.push(&self.rules[rule_idx]);
                injected[rule_idx] = true;
            }
        }

        result
    }

    /// check whether any rules exist
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// parse a rule file's yaml frontmatter for glob patterns
///
/// expects `---` fenced yaml with a `globs` field:
/// ```text
/// ---
/// globs: ["*.py", "tests/**/*.py"]
/// ---
/// ```
/// parse a rule file's yaml frontmatter for glob patterns
pub fn parse_rule_frontmatter(content: &str) -> Option<Vec<String>> {
    let (frontmatter, _) = crate::frontmatter::extract(content)?;
    parse_globs(frontmatter)
}

/// extract globs from pre-extracted frontmatter text
fn parse_globs(frontmatter: &str) -> Option<Vec<String>> {
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("globs:") {
            let val = val.trim();
            if val.starts_with('[') && val.ends_with(']') {
                let inner = &val[1..val.len() - 1];
                let globs: Vec<String> = inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !globs.is_empty() {
                    return Some(globs);
                }
            }
        }
    }
    None
}

/// scan `.mush/rules/` directory for rule files
pub fn discover_rules(cwd: &Path) -> Vec<Rule> {
    let rules_dir = cwd.join(".mush/rules");
    scan_rules_dir(&rules_dir)
}

/// scan a directory for markdown rule files with glob frontmatter
fn scan_rules_dir(dir: &Path) -> Vec<Rule> {
    let mut rules = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return rules,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some((frontmatter, body_start)) = crate::frontmatter::extract(&content) else {
            continue;
        };
        let Some(globs) = parse_globs(frontmatter) else {
            continue;
        };

        let body = content[body_start..].trim().to_string();

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        rules.push(Rule {
            name,
            globs,
            content: body,
            path,
        });
    }

    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_basic() {
        let content = r#"---
globs: ["*.py", "*.pyi"]
---
# Python rules
use type hints
"#;
        let globs = parse_rule_frontmatter(content).unwrap();
        assert_eq!(globs, vec!["*.py", "*.pyi"]);
    }

    #[test]
    fn parse_frontmatter_single_quotes() {
        let content = "---\nglobs: ['*.rs']\n---\nrust rules\n";
        let globs = parse_rule_frontmatter(content).unwrap();
        assert_eq!(globs, vec!["*.rs"]);
    }

    #[test]
    fn parse_frontmatter_missing_or_empty() {
        assert!(parse_rule_frontmatter("no frontmatter here").is_none());
        assert!(parse_rule_frontmatter("---\ntitle: rules\n---\ncontent\n").is_none());
        assert!(parse_rule_frontmatter("---\nglobs: []\n---\ncontent\n").is_none());
    }

    #[test]
    fn rule_index_matches() {
        let rules = vec![
            Rule {
                name: "python".into(),
                globs: vec!["*.py".into()],
                content: "use type hints".into(),
                path: PathBuf::from("rules/python.md"),
            },
            Rule {
                name: "rust".into(),
                globs: vec!["*.rs".into()],
                content: "prefer Result".into(),
                path: PathBuf::from("rules/rust.md"),
            },
        ];
        let index = RuleIndex::new(rules);

        let matched = index.match_file(Path::new("src/main.py"));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "python");

        let matched = index.match_file(Path::new("src/lib.rs"));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "rust");
    }

    #[test]
    fn rule_index_no_match() {
        let rules = vec![Rule {
            name: "python".into(),
            globs: vec!["*.py".into()],
            content: "python rules".into(),
            path: PathBuf::from("rules/python.md"),
        }];
        let index = RuleIndex::new(rules);
        assert!(index.match_file(Path::new("main.rs")).is_empty());
    }

    #[test]
    fn rule_index_deduplicates() {
        let rules = vec![Rule {
            name: "python".into(),
            globs: vec!["*.py".into()],
            content: "python rules".into(),
            path: PathBuf::from("rules/python.md"),
        }];
        let index = RuleIndex::new(rules);

        // first match returns the rule
        let matched = index.match_file(Path::new("a.py"));
        assert_eq!(matched.len(), 1);

        // second match for same rule returns nothing
        let matched = index.match_file(Path::new("b.py"));
        assert!(matched.is_empty());
    }

    #[test]
    fn rule_index_multiple_globs_same_rule() {
        let rules = vec![Rule {
            name: "web".into(),
            globs: vec!["*.js".into(), "*.ts".into(), "*.tsx".into()],
            content: "web rules".into(),
            path: PathBuf::from("rules/web.md"),
        }];
        let index = RuleIndex::new(rules);

        let matched = index.match_file(Path::new("app.tsx"));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "web");

        // already injected
        let matched = index.match_file(Path::new("lib.js"));
        assert!(matched.is_empty());
    }

    #[test]
    fn rule_index_glob_directories() {
        let rules = vec![Rule {
            name: "tests".into(),
            globs: vec!["tests/**/*.py".into()],
            content: "test rules".into(),
            path: PathBuf::from("rules/tests.md"),
        }];
        let index = RuleIndex::new(rules);

        let matched = index.match_file(Path::new("tests/unit/test_foo.py"));
        assert_eq!(matched.len(), 1);

        // doesn't match files outside tests/
        assert!(index.match_file(Path::new("src/foo.py")).is_empty());
    }

    #[test]
    fn discover_rules_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".mush/rules");
        std::fs::create_dir_all(&rules_dir).unwrap();

        std::fs::write(
            rules_dir.join("python.md"),
            "---\nglobs: [\"*.py\"]\n---\n# Python\nuse type hints\n",
        )
        .unwrap();

        std::fs::write(
            rules_dir.join("no-frontmatter.md"),
            "# No frontmatter\njust content\n",
        )
        .unwrap();

        std::fs::write(
            rules_dir.join("not-markdown.txt"),
            "---\nglobs: [\"*.txt\"]\n---\nignored\n",
        )
        .unwrap();

        let rules = discover_rules(dir.path());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "python");
        assert_eq!(rules[0].globs, vec!["*.py"]);
        assert!(rules[0].content.contains("use type hints"));
        // frontmatter should be stripped
        assert!(!rules[0].content.contains("globs:"));
    }

    #[test]
    fn discover_rules_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let rules = discover_rules(dir.path());
        assert!(rules.is_empty());
    }
}
