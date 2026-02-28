//! prompt template loading and substitution
//!
//! templates are markdown files with optional YAML frontmatter.
//! they support positional arg substitution ($1, $2, $@).
//! loaded from ~/.config/mush/prompts/ and .mush/prompts/.

use std::path::{Path, PathBuf};

/// a loaded prompt template
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub content: String,
    pub source: TemplateSource,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateSource {
    User,
    Project,
}

/// substitute argument placeholders in template content
///
/// supports:
/// - `$1`, `$2`, ... for positional args
/// - `$@` or `$ARGUMENTS` for all args joined
pub fn substitute_args(content: &str, args: &[&str]) -> String {
    let mut result = content.to_string();

    // positional args first (before wildcards to avoid re-substitution)
    for (i, arg) in args.iter().enumerate() {
        let placeholder = format!("${}", i + 1);
        result = result.replace(&placeholder, arg);
    }

    let all_args = args.join(" ");
    result = result.replace("$ARGUMENTS", &all_args);
    result = result.replace("$@", &all_args);

    result
}

/// parse a markdown file with optional YAML frontmatter
fn parse_template(path: &Path, source: TemplateSource) -> Option<PromptTemplate> {
    let raw = std::fs::read_to_string(path).ok()?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string();

    let (description, content) = parse_frontmatter(&raw);

    Some(PromptTemplate {
        name,
        description,
        content,
        source,
        path: path.to_path_buf(),
    })
}

/// extract frontmatter description and body from a markdown string
fn parse_frontmatter(text: &str) -> (String, String) {
    if !text.starts_with("---") {
        // no frontmatter, use first non-empty line as description
        let desc = text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(80)
            .collect();
        return (desc, text.to_string());
    }

    // find closing ---
    let rest = &text[3..];
    if let Some(end) = rest.find("\n---") {
        let front = &rest[..end];
        let body = &rest[end + 4..].trim_start();

        // extract description from frontmatter (simple key: value parsing)
        let desc = front
            .lines()
            .find_map(|l| l.strip_prefix("description:").map(|v| v.trim().to_string()))
            .unwrap_or_default();

        (desc, body.to_string())
    } else {
        (String::new(), text.to_string())
    }
}

/// discover prompt templates from user and project directories
pub fn discover_templates(cwd: &Path) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();

    // user templates: ~/.config/mush/prompts/
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        load_templates_from(
            &PathBuf::from(config).join("mush/prompts"),
            TemplateSource::User,
            &mut templates,
        );
    } else if let Some(home) = std::env::var_os("HOME") {
        load_templates_from(
            &PathBuf::from(home).join(".config/mush/prompts"),
            TemplateSource::User,
            &mut templates,
        );
    }

    // project templates: .mush/prompts/
    let project_dir = cwd.join(".mush/prompts");
    load_templates_from(&project_dir, TemplateSource::Project, &mut templates);

    templates
}

fn load_templates_from(dir: &Path, source: TemplateSource, templates: &mut Vec<PromptTemplate>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md")
            && let Some(tmpl) = parse_template(&path, source.clone())
        {
            templates.push(tmpl);
        }
    }
}

/// find a template by name from a list
pub fn find_template<'a>(
    templates: &'a [PromptTemplate],
    name: &str,
) -> Option<&'a PromptTemplate> {
    templates.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_positional_args() {
        let result = substitute_args("fix $1 in $2", &["bug", "main.rs"]);
        assert_eq!(result, "fix bug in main.rs");
    }

    #[test]
    fn substitute_all_args() {
        let result = substitute_args("search for: $@", &["hello", "world"]);
        assert_eq!(result, "search for: hello world");
    }

    #[test]
    fn substitute_arguments_keyword() {
        let result = substitute_args("args: $ARGUMENTS", &["a", "b", "c"]);
        assert_eq!(result, "args: a b c");
    }

    #[test]
    fn substitute_missing_arg() {
        let result = substitute_args("$1 and $2", &["only-one"]);
        assert_eq!(result, "only-one and $2");
    }

    #[test]
    fn substitute_no_args() {
        let result = substitute_args("no args here", &[]);
        assert_eq!(result, "no args here");
    }

    #[test]
    fn parse_frontmatter_with_description() {
        let text = "---\ndescription: a helpful prompt\n---\n\ndo the thing $1";
        let (desc, body) = parse_frontmatter(text);
        assert_eq!(desc, "a helpful prompt");
        assert!(body.contains("do the thing"));
    }

    #[test]
    fn parse_frontmatter_without() {
        let text = "just some content\nmore content";
        let (desc, body) = parse_frontmatter(text);
        assert_eq!(desc, "just some content");
        assert_eq!(body, text);
    }

    #[test]
    fn load_template_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        std::fs::write(
            &path,
            "---\ndescription: code review\n---\n\nreview $1 for issues",
        )
        .unwrap();

        let tmpl = parse_template(&path, TemplateSource::Project).unwrap();
        assert_eq!(tmpl.name, "review");
        assert_eq!(tmpl.description, "code review");
        assert!(tmpl.content.contains("review $1"));
    }

    #[test]
    fn discover_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join(".mush/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(
            prompts_dir.join("test.md"),
            "---\ndescription: test prompt\n---\nhello $1",
        )
        .unwrap();

        let templates = discover_templates(dir.path());
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "test");
    }

    #[test]
    fn find_template_by_name() {
        let templates = vec![
            PromptTemplate {
                name: "review".into(),
                description: "review code".into(),
                content: "review $1".into(),
                source: TemplateSource::User,
                path: PathBuf::from("/tmp/review.md"),
            },
            PromptTemplate {
                name: "explain".into(),
                description: "explain code".into(),
                content: "explain $1".into(),
                source: TemplateSource::Project,
                path: PathBuf::from("/tmp/explain.md"),
            },
        ];
        assert!(find_template(&templates, "review").is_some());
        assert!(find_template(&templates, "missing").is_none());
    }
}
