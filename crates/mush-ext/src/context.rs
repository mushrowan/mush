//! semantic context index for auto-loading relevant skills
//!
//! embeds skill descriptions using EmbeddingGemma-300M (768-dim, ONNX)
//! and matches them against user messages via cosine similarity.
//! matched skills are auto-injected into the system prompt.

use std::path::PathBuf;
use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::loader::Skill;

/// shared model cache under ~/.local/share/mush/models/
///
/// avoids re-downloading the onnx model into every working directory
fn model_cache_dir() -> PathBuf {
    let base = if let Ok(dir) = std::env::var("MUSH_DATA_DIR") {
        PathBuf::from(dir)
    } else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/mush")
    } else {
        PathBuf::from(".mush")
    };
    base.join("models")
}

/// a document indexed for semantic search
#[derive(Debug, Clone)]
pub struct ContextDocument {
    /// display name
    pub name: String,
    /// brief description
    pub description: String,
    /// full content to inject when matched
    pub content: String,
    /// text used for embedding (name + description + excerpt)
    embed_text: String,
}

/// a search result with relevance score
#[derive(Debug, Clone)]
pub struct ContextMatch {
    pub name: String,
    pub description: String,
    pub content: String,
    /// cosine similarity (0.0 to 1.0)
    pub score: f32,
}

/// errors from context index operations
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("failed to initialise embedding model: {0}")]
    ModelInit(String),
    #[error("failed to generate embeddings: {0}")]
    Embedding(String),
}

/// semantic search index over context documents
///
/// holds a local embedding model and pre-computed document embeddings.
/// created once at startup, queried per user message. the model is
/// behind a mutex because embed() requires &mut self.
pub struct ContextIndex {
    model: Mutex<TextEmbedding>,
    documents: Vec<ContextDocument>,
    embeddings: Vec<Vec<f32>>,
}

impl std::fmt::Debug for ContextIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextIndex")
            .field("document_count", &self.documents.len())
            .finish()
    }
}

impl ContextIndex {
    /// build an index from a set of documents
    ///
    /// initialises the embedding model and embeds all documents.
    /// uses EmbeddingGemma-300M (768-dim), downloaded on first use and cached.
    pub fn build(documents: Vec<ContextDocument>) -> Result<Self, ContextError> {
        let mut model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::EmbeddingGemma300M)
                .with_cache_dir(model_cache_dir())
                .with_show_download_progress(true),
        )
        .map_err(|e| ContextError::ModelInit(e.to_string()))?;

        if documents.is_empty() {
            return Ok(Self {
                model: Mutex::new(model),
                documents,
                embeddings: vec![],
            });
        }

        let texts: Vec<&str> = documents.iter().map(|d| d.embed_text.as_str()).collect();
        let embeddings = model
            .embed(texts, None)
            .map_err(|e| ContextError::Embedding(e.to_string()))?;

        Ok(Self {
            model: Mutex::new(model),
            documents,
            embeddings,
        })
    }

    /// search for documents relevant to a query
    ///
    /// returns up to `top_k` matches with cosine similarity >= `threshold`
    pub fn search(&self, query: &str, top_k: usize, threshold: f32) -> Vec<ContextMatch> {
        if self.documents.is_empty() {
            return vec![];
        }

        let mut model = match self.model.lock() {
            Ok(m) => m,
            Err(_) => return vec![],
        };
        let query_emb = match model.embed(vec![query], None) {
            Ok(mut embs) if !embs.is_empty() => embs.swap_remove(0),
            _ => return vec![],
        };
        drop(model);

        let mut scored: Vec<(usize, f32)> = self
            .embeddings
            .iter()
            .enumerate()
            .map(|(i, emb)| (i, cosine_similarity(&query_emb, emb)))
            .filter(|(_, score)| *score >= threshold)
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);

        scored
            .iter()
            .map(|(i, score)| {
                let doc = &self.documents[*i];
                ContextMatch {
                    name: doc.name.clone(),
                    description: doc.description.clone(),
                    content: doc.content.clone(),
                    score: *score,
                }
            })
            .collect()
    }

    /// number of indexed documents
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// whether the index is empty
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }
}

/// build context documents from discovered skills
///
/// reads each skill's SKILL.md and creates an indexable document
/// with name + description + first paragraph as embedding text
pub fn build_skill_documents(skills: &[Skill]) -> Vec<ContextDocument> {
    skills
        .iter()
        .filter_map(|s| {
            let content = std::fs::read_to_string(&s.path).ok()?;
            let excerpt = first_paragraph(&content);
            let embed_text = format!("{}: {}\n\n{}", s.name, s.description, excerpt);

            Some(ContextDocument {
                name: s.name.clone(),
                description: s.description.clone(),
                content,
                embed_text,
            })
        })
        .collect()
}

/// format matched skills for injection into the system prompt (full content)
pub fn format_matches(matches: &[ContextMatch]) -> String {
    if matches.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "\n\n## Auto-loaded skills\n\n\
         The following skill instructions were automatically loaded based on your request.\n",
    );

    for m in matches {
        out.push_str(&format!(
            "\n### {}\n{}\n\n{}\n",
            m.name, m.description, m.content
        ));
    }

    out
}

/// format a brief relevance hint to prepend to the user message.
/// all skill content is already in the system prompt, this just nudges
/// the model toward the most relevant ones without changing the cached prefix.
pub fn format_relevance_hint(matches: &[ContextMatch]) -> String {
    if matches.is_empty() {
        return String::new();
    }

    let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
    format!(
        "[relevant skills: {}. follow their instructions from the system prompt.]",
        names.join(", ")
    )
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
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

    #[test]
    fn cosine_identical_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_opposite_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_empty_vectors() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_mismatched_lengths() {
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn cosine_zero_vectors() {
        let a = vec![0.0, 0.0];
        let b = vec![0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

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
    fn format_matches_empty() {
        assert!(format_matches(&[]).is_empty());
    }

    #[test]
    fn format_matches_produces_output() {
        let matches = vec![ContextMatch {
            name: "rust".into(),
            description: "rust stuff".into(),
            content: "use cargo".into(),
            score: 0.8,
        }];

        let output = format_matches(&matches);
        assert!(output.contains("Auto-loaded skills"));
        assert!(output.contains("### rust"));
        assert!(output.contains("use cargo"));
    }

    #[test]
    fn format_relevance_hint_produces_names() {
        let matches = vec![
            ContextMatch {
                name: "rust".into(),
                description: "rust stuff".into(),
                content: "full content".into(),
                score: 0.8,
            },
            ContextMatch {
                name: "nix".into(),
                description: "nix stuff".into(),
                content: "full content".into(),
                score: 0.6,
            },
        ];

        let hint = format_relevance_hint(&matches);
        assert!(hint.contains("rust"));
        assert!(hint.contains("nix"));
        assert!(!hint.contains("full content")); // no content leaking into hint
    }

    #[test]
    fn format_relevance_hint_empty() {
        assert!(format_relevance_hint(&[]).is_empty());
    }

    /// requires onnxruntime available (skipped in CI without it)
    #[test]
    fn integration_build_and_search() {
        let docs = vec![
            ContextDocument {
                name: "rust".into(),
                description: "rust conventions and cargo usage".into(),
                content: "use cargo for building rust projects".into(),
                embed_text: "rust: rust conventions and cargo usage\n\nuse cargo for builds".into(),
            },
            ContextDocument {
                name: "nix".into(),
                description: "nix flakes and nixos configuration".into(),
                content: "use flake.nix for reproducible builds".into(),
                embed_text: "nix: nix flakes and nixos configuration\n\nflake.nix for builds"
                    .into(),
            },
            ContextDocument {
                name: "git".into(),
                description: "git version control workflows".into(),
                content: "use git for version control".into(),
                embed_text: "git: git version control workflows\n\nuse git for version control"
                    .into(),
            },
        ];

        let index = match ContextIndex::build(docs) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!("skipping integration test (model unavailable): {e}");
                return;
            }
        };

        assert_eq!(index.len(), 3);
        assert!(!index.is_empty());

        // "how do I build with cargo?" should match rust most strongly
        let matches = index.search("how do I build with cargo?", 2, 0.0);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].name, "rust");

        // "configure nixos system" should match nix
        let matches = index.search("configure nixos system", 2, 0.0);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].name, "nix");

        // empty index returns empty
        let empty = ContextIndex::build(vec![]).unwrap();
        assert!(empty.search("anything", 5, 0.0).is_empty());
    }
}
