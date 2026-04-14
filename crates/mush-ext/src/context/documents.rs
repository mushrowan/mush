//! building context documents from skills, tools, and files

use std::path::Path;

use crate::loader::Skill;

use super::{ContextDocument, ContextMatch, DocumentKind};

/// build context documents from discovered skills
///
/// reads each skill's SKILL.md and creates indexable documents.
/// for code files referenced by skills, uses AST-aware chunking
/// to produce better embeddings. for markdown/text, uses
/// paragraph-based chunking.
pub fn build_skill_documents(skills: &[Skill]) -> Vec<ContextDocument> {
    skills
        .iter()
        .flat_map(|s| {
            let content = match std::fs::read_to_string(&s.path) {
                Ok(c) => c,
                Err(_) => return vec![],
            };

            let sections = split_markdown_sections(&content);

            if sections.len() <= 1 {
                // single section or no headings: one document with overview embed
                let excerpt = first_paragraph(&content);
                let embed_text = format!("{}: {}\n\n{excerpt}", s.name, s.description);
                return vec![ContextDocument {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    content,
                    embed_text,
                    source_path: Some(s.path.clone()),
                    kind: DocumentKind::Skill,
                }];
            }

            // one document per section, all carrying the full skill content
            sections
                .into_iter()
                .map(|section| {
                    let embed_text = format!(
                        "{} ({}): {}\n\n{}",
                        s.name, section.heading, s.description, section.body
                    );
                    ContextDocument {
                        name: s.name.clone(),
                        description: s.description.clone(),
                        content: content.clone(),
                        embed_text,
                        source_path: Some(s.path.clone()),
                        kind: DocumentKind::Skill,
                    }
                })
                .collect()
        })
        .collect()
}

/// build context documents from tool name/description pairs (e.g. MCP tools)
pub fn build_tool_documents(tools: &[(String, String)]) -> Vec<ContextDocument> {
    tools
        .iter()
        .map(|(name, description)| {
            let embed_text = format!("{name}: {description}");
            ContextDocument {
                name: name.clone(),
                description: description.clone(),
                content: String::new(),
                embed_text,
                source_path: None,
                kind: DocumentKind::Tool,
            }
        })
        .collect()
}

/// chunk a file into text segments for embedding
///
/// uses AST-aware chunking for code files, paragraph-based
/// splitting for markdown/text
pub fn chunk_file_for_embedding(path: &Path) -> Vec<ContextDocument> {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let opts = mush_treesitter::ChunkOptions::default();
    let chunks = mush_treesitter::chunk_file(path, &opts);

    if chunks.is_empty() {
        if source.trim().is_empty() {
            return vec![];
        }
        return vec![ContextDocument {
            name: file_name.to_string(),
            description: path.display().to_string(),
            content: source.clone(),
            embed_text: source,
            source_path: Some(path.to_path_buf()),
            kind: DocumentKind::default(),
        }];
    }

    chunks
        .into_iter()
        .map(|chunk| {
            let name = chunk
                .symbol_name
                .as_deref()
                .map(|n| format!("{file_name}::{n}"))
                .unwrap_or_else(|| format!("{file_name}:{}", chunk.start_line + 1));
            let description = format!(
                "{}:{}..{}",
                path.display(),
                chunk.start_line + 1,
                chunk.end_line + 1
            );
            ContextDocument {
                name,
                description,
                embed_text: chunk.text.clone(),
                content: chunk.text,
                source_path: Some(path.to_path_buf()),
                kind: DocumentKind::default(),
            }
        })
        .collect()
}

/// route matches into auto-loaded content, skill hints, and tool hints
/// based on score and document kind.
///
/// skills above `auto_load_threshold` get full content injected.
/// when `include_hints` is true, skills below the threshold get a brief
/// hint and tools get a pointer to mcp_get_schemas. when false, only
/// auto-loaded content is returned (used by embed_inject strategy).
pub fn route_matches(
    matches: &[ContextMatch],
    auto_load_threshold: f32,
    include_hints: bool,
) -> String {
    if matches.is_empty() {
        return String::new();
    }

    let (tools, skills): (Vec<&ContextMatch>, Vec<&ContextMatch>) =
        matches.iter().partition(|m| m.kind == DocumentKind::Tool);

    let (auto_load, hint_only): (Vec<&ContextMatch>, Vec<&ContextMatch>) = skills
        .into_iter()
        .partition(|m| m.score >= auto_load_threshold);

    let mut parts = Vec::new();

    if !auto_load.is_empty() {
        let mut out = String::from("[Auto-loaded skill instructions]\n");
        for m in &auto_load {
            out.push_str(&format!(
                "\n## {}\n{}\n\n{}\n",
                m.name, m.description, m.content
            ));
        }
        parts.push(out);
    }

    if include_hints {
        if !hint_only.is_empty() {
            let names: Vec<&str> = hint_only.iter().map(|m| m.name.as_str()).collect();
            parts.push(format!(
                "[relevant skills: {}. follow their instructions from the system prompt.]",
                names.join(", ")
            ));
        }

        if !tools.is_empty() {
            let names: Vec<&str> = tools.iter().map(|m| m.name.as_str()).collect();
            let descs: Vec<String> = tools
                .iter()
                .map(|m| format!("  - {}: {}", m.name, m.description))
                .collect();
            parts.push(format!(
                "[relevant MCP tools: {}. use mcp_get_schemas to see their parameters.]\n{}",
                names.join(", "),
                descs.join("\n"),
            ));
        }
    }

    parts.join("\n")
}

/// a section extracted from a markdown document
struct MarkdownSection {
    heading: String,
    body: String,
}

/// split markdown into sections by headings (##, ###, etc)
///
/// each section includes its heading text and body up to the next heading.
/// content before the first heading becomes a section with heading "overview".
fn split_markdown_sections(content: &str) -> Vec<MarkdownSection> {
    let mut sections = Vec::new();
    let mut current_heading = String::from("overview");
    let mut current_body = String::new();

    for line in content.lines() {
        if line.starts_with('#') {
            // flush previous section
            let body = current_body.trim().to_string();
            if !body.is_empty() {
                sections.push(MarkdownSection {
                    heading: current_heading,
                    body,
                });
            }
            current_heading = line.trim_start_matches('#').trim().to_string();
            current_body = String::new();
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    // flush last section
    let body = current_body.trim().to_string();
    if !body.is_empty() {
        sections.push(MarkdownSection {
            heading: current_heading,
            body,
        });
    }

    sections
}

/// extract the first meaningful paragraph from markdown
fn first_paragraph(content: &str) -> &str {
    let trimmed = content.trim_start();

    // skip leading heading
    let rest = if trimmed.starts_with('#') {
        let newline = trimmed.find('\n').unwrap_or(trimmed.len());
        trimmed[newline..].trim_start()
    } else {
        trimmed
    };

    // take until double newline or end
    let end = rest.find("\n\n").unwrap_or(rest.len());
    rest[..end].trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    // first_paragraph

    #[test]
    fn first_paragraph_simple() {
        let content = "# Heading\n\nFirst paragraph here.\n\nSecond paragraph.";
        assert_eq!(first_paragraph(content), "First paragraph here.");
    }

    #[test]
    fn first_paragraph_no_heading() {
        let content = "Just a paragraph.\n\nAnother one.";
        assert_eq!(first_paragraph(content), "Just a paragraph.");
    }

    #[test]
    fn first_paragraph_single() {
        let content = "Only one paragraph";
        assert_eq!(first_paragraph(content), "Only one paragraph");
    }

    #[test]
    fn first_paragraph_with_leading_whitespace() {
        let content = "\n\n# Title\n\nContent here.\n\nMore.";
        assert_eq!(first_paragraph(content), "Content here.");
    }

    // document building

    #[test]
    fn build_documents_from_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("rust");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(&skill_path, "# rust\n\nuse cargo for builds\n\nmore stuff").unwrap();

        let skills = vec![Skill {
            name: "rust".into(),
            description: "rust conventions".into(),
            path: skill_path,
        }];

        let docs = build_skill_documents(&skills);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].name, "rust");
        assert!(docs[0].content.contains("cargo"));
        assert!(docs[0].embed_text.contains("rust: rust conventions"));
    }

    #[test]
    fn build_documents_skips_missing_files() {
        let skills = vec![Skill {
            name: "gone".into(),
            description: "missing".into(),
            path: "/does/not/exist/SKILL.md".into(),
        }];

        let docs = build_skill_documents(&skills);
        assert!(docs.is_empty());
    }

    #[test]
    fn build_skill_documents_uses_chunking() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_path,
            "# Test Skill\n\nThis skill provides guidance.\n\nMore details here.\n",
        )
        .unwrap();

        let skills = vec![Skill {
            name: "test".into(),
            description: "test skill".into(),
            path: skill_path,
        }];

        let docs = build_skill_documents(&skills);
        assert_eq!(docs.len(), 1);
        assert!(docs[0].embed_text.contains("test: test skill"));
    }

    // tool documents

    #[test]
    fn build_tool_documents_creates_entries() {
        let tools = vec![
            ("git_commit".to_string(), "create a git commit".to_string()),
            ("git_diff".to_string(), "show diff".to_string()),
        ];
        let docs = build_tool_documents(&tools);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].name, "git_commit");
        assert_eq!(docs[0].kind, DocumentKind::Tool);
        assert!(docs[0].embed_text.contains("create a git commit"));
        assert!(docs[0].content.is_empty());
    }

    #[test]
    fn build_tool_documents_empty() {
        assert!(build_tool_documents(&[]).is_empty());
    }

    // file chunking

    #[test]
    fn chunk_file_for_embedding_rust() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        std::fs::write(
            &path,
            r#"use std::io;

pub struct Config {
    pub port: u16,
}

pub fn run() {
    println!("running");
}
"#,
        )
        .unwrap();

        let docs = chunk_file_for_embedding(&path);
        assert!(!docs.is_empty(), "should produce chunks");
        assert!(
            docs.iter()
                .any(|d| d.name.contains("Config") || d.name.contains("run")),
            "chunks should reference symbols: {:?}",
            docs.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn chunk_file_for_embedding_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("README.md");
        std::fs::write(&path, "# Title\n\nSome content here.\n").unwrap();

        let docs = chunk_file_for_embedding(&path);
        assert!(!docs.is_empty());
        assert!(docs[0].content.contains("content"));
    }

    #[test]
    fn chunk_file_for_embedding_missing() {
        let docs = chunk_file_for_embedding(Path::new("/nope/gone.rs"));
        assert!(docs.is_empty());
    }

    // routing

    #[test]
    fn route_matches_splits_by_threshold() {
        let matches = vec![
            ContextMatch {
                name: "rust".into(),
                description: "rust conventions".into(),
                content: "use cargo for builds".into(),
                score: 0.7,
                source_path: None,
                kind: DocumentKind::default(),
            },
            ContextMatch {
                name: "nix".into(),
                description: "nix stuff".into(),
                content: "use flakes".into(),
                score: 0.4,
                source_path: None,
                kind: DocumentKind::default(),
            },
        ];

        let routed = route_matches(&matches, 0.5, true);
        // rust (0.7) should be auto-loaded, nix (0.4) should be hint-only
        assert!(routed.contains("use cargo for builds"));
        assert!(routed.contains("[relevant skills: nix"));
        assert!(!routed.contains("use flakes"));
    }

    #[test]
    fn route_matches_all_auto_loaded() {
        let matches = vec![ContextMatch {
            name: "rust".into(),
            description: "rust conventions".into(),
            content: "use cargo".into(),
            score: 0.8,
            source_path: None,
            kind: DocumentKind::default(),
        }];

        let routed = route_matches(&matches, 0.5, true);
        assert!(routed.contains("use cargo"));
        assert!(!routed.contains("[relevant skills:"));
    }

    #[test]
    fn route_matches_mixed_skills_and_tools() {
        let matches = vec![
            ContextMatch {
                name: "rust".into(),
                description: "rust conventions".into(),
                content: "use cargo".into(),
                score: 0.8,
                source_path: None,
                kind: DocumentKind::Skill,
            },
            ContextMatch {
                name: "mcp_git_commit".into(),
                description: "create a git commit".into(),
                content: "".into(),
                score: 0.6,
                source_path: None,
                kind: DocumentKind::Tool,
            },
        ];

        let routed = route_matches(&matches, 0.5, true);
        assert!(routed.contains("use cargo"), "skill content auto-loaded");
        assert!(routed.contains("mcp_git_commit"), "tool name in hint");
        assert!(
            routed.contains("mcp_get_schemas"),
            "tool hint directs to schemas"
        );
    }

    #[test]
    fn route_matches_all_hints() {
        let matches = vec![ContextMatch {
            name: "rust".into(),
            description: "rust conventions".into(),
            content: "use cargo".into(),
            score: 0.4,
            source_path: None,
            kind: DocumentKind::default(),
        }];

        let routed = route_matches(&matches, 0.5, true);
        assert!(!routed.contains("use cargo"));
        assert!(routed.contains("[relevant skills: rust"));
    }

    #[test]
    fn route_matches_inject_only_skips_hints() {
        let matches = vec![
            ContextMatch {
                name: "rust".into(),
                description: "rust conventions".into(),
                content: "use cargo for builds".into(),
                score: 0.7,
                source_path: None,
                kind: DocumentKind::Skill,
            },
            ContextMatch {
                name: "nix".into(),
                description: "nix stuff".into(),
                content: "use flakes".into(),
                score: 0.4,
                source_path: None,
                kind: DocumentKind::Skill,
            },
            ContextMatch {
                name: "mcp_git_commit".into(),
                description: "create a git commit".into(),
                content: "".into(),
                score: 0.6,
                source_path: None,
                kind: DocumentKind::Tool,
            },
        ];

        let routed = route_matches(&matches, 0.5, false);
        // auto-loaded skill still present
        assert!(routed.contains("use cargo for builds"));
        // below-threshold skill hint suppressed
        assert!(!routed.contains("nix"));
        // tool hint suppressed
        assert!(!routed.contains("mcp_git_commit"));
        assert!(!routed.contains("mcp_get_schemas"));
    }
}
