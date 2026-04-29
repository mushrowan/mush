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
    /// optional one-line label shown next to the name in the
    /// slash-menu / `@`-picker. set via the yaml frontmatter
    /// `description: ...` field. when `None`, pickers show only the name
    pub description: Option<String>,
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

/// parse a markdown file as a prompt template. yaml frontmatter is
/// stripped from the body so the model never sees it. an optional
/// `description: ...` line in the frontmatter populates the picker label
fn parse_template(path: &Path, source: TemplateSource) -> Option<PromptTemplate> {
    let raw = std::fs::read_to_string(path).ok()?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string();

    let (description, body) = parse_frontmatter(&raw);

    Some(PromptTemplate {
        name,
        description,
        content: body,
        source,
        path: path.to_path_buf(),
    })
}

/// split an optional `---`-delimited yaml frontmatter block off the
/// front of `text`. returns `(description, body)` where `description`
/// is the frontmatter's `description: ...` value when present, and
/// `body` is the text minus the frontmatter
fn parse_frontmatter(text: &str) -> (Option<String>, String) {
    let Some(rest) = text.strip_prefix("---") else {
        return (None, text.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text.to_string());
    };
    let front = &rest[..end];
    let body = rest[end + 4..]
        .trim_start_matches(['\n', '\r'])
        .trim_end()
        .to_string();
    let description = front
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty());
    (description, body)
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
    fn parse_frontmatter_extracts_description() {
        let text = "---\ndescription: a helpful prompt\n---\n\ndo the thing $1";
        let (desc, body) = parse_frontmatter(text);
        assert_eq!(desc.as_deref(), Some("a helpful prompt"));
        assert_eq!(body, "do the thing $1");
    }

    #[test]
    fn parse_frontmatter_without_returns_none_and_full_text() {
        let text = "just some content\nmore content";
        let (desc, body) = parse_frontmatter(text);
        assert!(desc.is_none());
        assert_eq!(body, text);
    }

    #[test]
    fn parse_frontmatter_without_description_returns_none() {
        // frontmatter without a description: line still strips the
        // frontmatter from the body but leaves the picker label unset
        let text = "---\nother: value\n---\n\nactual body";
        let (desc, body) = parse_frontmatter(text);
        assert!(desc.is_none());
        assert_eq!(body, "actual body");
    }

    #[test]
    fn load_template_from_disk_without_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        std::fs::write(&path, "review $1 for issues").unwrap();

        let tmpl = parse_template(&path, TemplateSource::Project).unwrap();
        assert_eq!(tmpl.name, "review");
        assert_eq!(tmpl.content, "review $1 for issues");
        assert!(
            tmpl.description.is_none(),
            "no frontmatter should yield no description"
        );
    }

    #[test]
    fn load_template_from_disk_with_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        std::fs::write(
            &path,
            "---\ndescription: code review\n---\n\nreview $1 for issues",
        )
        .unwrap();

        let tmpl = parse_template(&path, TemplateSource::Project).unwrap();
        assert_eq!(tmpl.description.as_deref(), Some("code review"));
        assert_eq!(tmpl.content, "review $1 for issues");
    }

    #[test]
    fn discover_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join(".mush/prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("test.md"), "hello $1").unwrap();

        let templates = discover_templates(dir.path());
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "test");
    }

    #[test]
    fn find_template_by_name() {
        let templates = vec![
            PromptTemplate {
                name: "review".into(),
                description: Some("review code".into()),
                content: "review $1".into(),
                source: TemplateSource::User,
                path: PathBuf::from("/tmp/review.md"),
            },
            PromptTemplate {
                name: "explain".into(),
                description: None,
                content: "explain $1".into(),
                source: TemplateSource::Project,
                path: PathBuf::from("/tmp/explain.md"),
            },
        ];
        assert!(find_template(&templates, "review").is_some());
        assert!(find_template(&templates, "missing").is_none());
    }
}
