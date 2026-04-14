//! semantic context index for auto-loading relevant skills
//!
//! embeds skill descriptions using a local ONNX model (CodeRankEmbed
//! by default, with Gemma-300M fallback) and matches them against user
//! messages via cosine similarity. matched skills are auto-injected
//! into the system prompt.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};

use crate::loader::Skill;

/// what kind of document this is (for routing matches differently)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocumentKind {
    /// skill instruction file
    #[default]
    Skill,
    /// MCP or built-in tool description
    Tool,
}

/// shared model cache under the mush data dir
fn model_cache_dir() -> PathBuf {
    mush_session::data_dir().join("models")
}

/// which local embedding model to use
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingModelChoice {
    /// nomic CodeRankEmbed-137M, 768-dim, code-specialised
    /// INT8 ONNX from mrsladoje/CodeRankEmbed-onnx-int8 (132MB)
    CodeRankEmbed,
    /// google EmbeddingGemma-300M, 768-dim, general purpose
    /// built-in fastembed model (~274MB)
    Gemma300M,
}

impl Default for EmbeddingModelChoice {
    fn default() -> Self {
        Self::CodeRankEmbed
    }
}

impl EmbeddingModelChoice {
    /// apply model-specific query prefix for search
    fn prefix_query(&self, query: &str) -> String {
        match self {
            Self::CodeRankEmbed => {
                format!("Represent this query for searching relevant code: {query}")
            }
            Self::Gemma300M => query.to_string(),
        }
    }
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
    /// file path this document came from (for deduplication across tiers)
    pub source_path: Option<PathBuf>,
    /// what kind of document this is
    pub kind: DocumentKind,
}

impl ContextDocument {
    /// create a new document with auto-generated embed text
    pub fn new(
        name: String,
        description: String,
        content: String,
        source_path: Option<PathBuf>,
        kind: DocumentKind,
    ) -> Self {
        let excerpt = first_paragraph(&content);
        let embed_text = format!("{name}: {description}\n\n{excerpt}");
        Self {
            name,
            description,
            content,
            embed_text,
            source_path,
            kind,
        }
    }
}

/// a search result with relevance score
#[derive(Debug, Clone)]
pub struct ContextMatch {
    pub name: String,
    pub description: String,
    pub content: String,
    /// cosine similarity (0.0 to 1.0)
    pub score: f32,
    /// file path this result came from (for deduplication)
    pub source_path: Option<PathBuf>,
    /// what kind of result this is
    pub kind: DocumentKind,
}

/// errors from context index operations
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("failed to initialise embedding model: {0}")]
    ModelInit(String),
    #[error("failed to generate embeddings: {0}")]
    Embedding(String),
    #[error("failed to download model files: {0}")]
    Download(String),
}

/// semantic search index over context documents
///
/// holds a local embedding model and pre-computed document embeddings.
/// created once at startup, queried per user message. the model is
/// behind `std::sync::Mutex` (not `tokio::sync::Mutex`) because
/// embedding is CPU-bound work that blocks the thread regardless.
/// `tokio::sync::Mutex` is for protecting IO-bound critical sections
/// where you need to hold the lock across `.await` points.
pub struct ContextIndex {
    model: Mutex<TextEmbedding>,
    model_choice: EmbeddingModelChoice,
    documents: Vec<ContextDocument>,
    embeddings: Vec<Vec<f32>>,
}

impl std::fmt::Debug for ContextIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextIndex")
            .field("model", &self.model_choice)
            .field("document_count", &self.documents.len())
            .finish()
    }
}

impl ContextIndex {
    /// build an index from a set of documents
    ///
    /// tries CodeRankEmbed first (code-specialised, faster).
    /// falls back to EmbeddingGemma-300M if download/init fails.
    pub fn build(documents: Vec<ContextDocument>) -> Result<Self, ContextError> {
        Self::build_with_model(documents, EmbeddingModelChoice::default())
    }

    /// build with a specific model choice
    pub fn build_with_model(
        documents: Vec<ContextDocument>,
        choice: EmbeddingModelChoice,
    ) -> Result<Self, ContextError> {
        let mut model = create_embedding_model(choice)?;

        let embeddings = if documents.is_empty() {
            vec![]
        } else {
            let texts: Vec<&str> = documents.iter().map(|d| d.embed_text.as_str()).collect();
            model
                .embed(texts, None)
                .map_err(|e| ContextError::Embedding(e.to_string()))?
        };

        Ok(Self {
            model: Mutex::new(model),
            model_choice: choice,
            documents,
            embeddings,
        })
    }

    /// search for documents relevant to a query
    ///
    /// returns up to `top_k` matches with cosine similarity >= `threshold`.
    /// applies model-specific query prefix (e.g. CodeRankEmbed needs a
    /// task instruction prefix for best results).
    pub fn search(&self, query: &str, top_k: usize, threshold: f32) -> Vec<ContextMatch> {
        if self.documents.is_empty() {
            return vec![];
        }

        let query_str = self.model_choice.prefix_query(query);

        let mut model = match self.model.lock() {
            Ok(m) => m,
            Err(_) => return vec![],
        };
        let query_emb = match model.embed(vec![query_str.as_str()], None) {
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

        // deduplicate: keep the highest-scoring chunk per document name
        // (skill sections share the same name, we want the best match only)
        let mut seen = std::collections::HashSet::new();
        let mut deduped = Vec::new();
        for (i, score) in &scored {
            let doc = &self.documents[*i];
            if seen.insert(&doc.name) {
                deduped.push((*i, *score));
                if deduped.len() == top_k {
                    break;
                }
            }
        }

        deduped
            .iter()
            .map(|(i, score)| {
                let doc = &self.documents[*i];
                ContextMatch {
                    name: doc.name.clone(),
                    description: doc.description.clone(),
                    content: doc.content.clone(),
                    score: *score,
                    source_path: doc.source_path.clone(),
                    kind: doc.kind,
                }
            })
            .collect()
    }

    /// which model this index was built with
    pub fn model_choice(&self) -> EmbeddingModelChoice {
        self.model_choice
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

// -- model creation --

/// create a TextEmbedding from the chosen model
fn create_embedding_model(choice: EmbeddingModelChoice) -> Result<TextEmbedding, ContextError> {
    match choice {
        EmbeddingModelChoice::CodeRankEmbed => create_coderank_model(),
        EmbeddingModelChoice::Gemma300M => create_gemma_model(),
    }
}

/// load CodeRankEmbed INT8 ONNX via hf-hub download + UserDefinedEmbeddingModel
fn create_coderank_model() -> Result<TextEmbedding, ContextError> {
    let api = hf_hub::api::sync::ApiBuilder::new()
        .with_cache_dir(model_cache_dir())
        .build()
        .map_err(|e| ContextError::Download(e.to_string()))?;

    let repo = api.model("mrsladoje/CodeRankEmbed-onnx-int8".to_string());
    let fetch = |name: &str| -> Result<Vec<u8>, ContextError> {
        let path = repo
            .get(name)
            .map_err(|e| ContextError::Download(format!("{name}: {e}")))?;
        std::fs::read(&path)
            .map_err(|e| ContextError::Download(format!("read {}: {e}", path.display())))
    };

    let user_model = UserDefinedEmbeddingModel::new(
        fetch("onnx/model.onnx")?,
        TokenizerFiles {
            tokenizer_file: fetch("tokenizer.json")?,
            config_file: fetch("config.json")?,
            special_tokens_map_file: fetch("special_tokens_map.json")?,
            tokenizer_config_file: fetch("tokenizer_config.json")?,
        },
    )
    .with_pooling(Pooling::Mean);

    TextEmbedding::try_new_from_user_defined(user_model, InitOptionsUserDefined::new())
        .map_err(|e| ContextError::ModelInit(e.to_string()))
}

/// load built-in EmbeddingGemma-300M via fastembed
fn create_gemma_model() -> Result<TextEmbedding, ContextError> {
    use fastembed::{EmbeddingModel, InitOptions};

    TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::EmbeddingGemma300M)
            .with_cache_dir(model_cache_dir())
            .with_show_download_progress(true),
    )
    .map_err(|e| ContextError::ModelInit(e.to_string()))
}

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

/// deduplicate matches by source file path, keeping the highest-scored
/// result per file. results without a source path (e.g. skills) are
/// kept as-is. output is sorted by descending score.
pub fn deduplicate_matches(matches: Vec<ContextMatch>) -> Vec<ContextMatch> {
    use std::collections::HashMap;

    let mut best_by_path: HashMap<PathBuf, ContextMatch> = HashMap::new();
    let mut pathless: Vec<ContextMatch> = Vec::new();

    for m in matches {
        match m.source_path {
            Some(ref path) => {
                let entry = best_by_path.entry(path.clone());
                entry
                    .and_modify(|existing| {
                        if m.score > existing.score {
                            *existing = m.clone();
                        }
                    })
                    .or_insert(m);
            }
            None => pathless.push(m),
        }
    }

    let mut results: Vec<ContextMatch> = best_by_path.into_values().chain(pathless).collect();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// remove matches whose source path is in a known file set.
/// useful for excluding files already represented in the repo map.
pub fn exclude_known_files(matches: Vec<ContextMatch>, known: &[PathBuf]) -> Vec<ContextMatch> {
    use std::collections::HashSet;

    if known.is_empty() {
        return matches;
    }

    let known_set: HashSet<&Path> = known.iter().map(|p| p.as_path()).collect();
    matches
        .into_iter()
        .filter(|m| {
            m.source_path
                .as_deref()
                .map_or(true, |p| !known_set.contains(p))
        })
        .collect()
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

    // -- model choice tests --

    #[test]
    fn default_model_is_coderank() {
        assert_eq!(
            EmbeddingModelChoice::default(),
            EmbeddingModelChoice::CodeRankEmbed
        );
    }

    #[test]
    fn coderank_prefixes_query() {
        let result = EmbeddingModelChoice::CodeRankEmbed.prefix_query("find the auth module");
        assert!(result.starts_with("Represent this query"));
        assert!(result.ends_with("find the auth module"));
    }

    #[test]
    fn gemma_passes_query_through() {
        let result = EmbeddingModelChoice::Gemma300M.prefix_query("find the auth module");
        assert_eq!(result, "find the auth module");
    }

    // -- cosine similarity tests --

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

    // -- first_paragraph tests --

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

    // -- document building tests --

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

    // -- format tests --

    // -- routing tests --

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

    // -- integration tests (require model download) --

    /// requires onnxruntime available (skipped in CI without it)
    #[test]
    fn integration_build_and_search() {
        let docs = vec![
            ContextDocument {
                name: "rust".into(),
                description: "rust conventions and cargo usage".into(),
                content: "use cargo for building rust projects".into(),
                embed_text: "rust: rust conventions and cargo usage\n\nuse cargo for builds".into(),
                source_path: None,
                kind: DocumentKind::default(),
            },
            ContextDocument {
                name: "nix".into(),
                description: "nix flakes and nixos configuration".into(),
                content: "use flake.nix for reproducible builds".into(),
                embed_text: "nix: nix flakes and nixos configuration\n\nflake.nix for builds"
                    .into(),
                source_path: None,
                kind: DocumentKind::default(),
            },
            ContextDocument {
                name: "git".into(),
                description: "git version control workflows".into(),
                content: "use git for version control".into(),
                embed_text: "git: git version control workflows\n\nuse git for version control"
                    .into(),
                source_path: None,
                kind: DocumentKind::default(),
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

    // -- deduplication tests --

    #[test]
    fn deduplicate_removes_same_file_keeps_highest() {
        let matches = vec![
            ContextMatch {
                name: "lib.rs::Config".into(),
                description: "src/lib.rs:1..10".into(),
                content: "struct Config".into(),
                score: 0.9,
                source_path: Some(PathBuf::from("src/lib.rs")),
                kind: DocumentKind::default(),
            },
            ContextMatch {
                name: "lib.rs::run".into(),
                description: "src/lib.rs:12..20".into(),
                content: "fn run()".into(),
                score: 0.7,
                source_path: Some(PathBuf::from("src/lib.rs")),
                kind: DocumentKind::default(),
            },
            ContextMatch {
                name: "main.rs::main".into(),
                description: "src/main.rs:1..5".into(),
                content: "fn main()".into(),
                score: 0.8,
                source_path: Some(PathBuf::from("src/main.rs")),
                kind: DocumentKind::default(),
            },
        ];

        let deduped = deduplicate_matches(matches);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].score, 0.9); // lib.rs highest
        assert_eq!(deduped[1].score, 0.8); // main.rs
    }

    #[test]
    fn deduplicate_preserves_pathless_results() {
        let matches = vec![
            ContextMatch {
                name: "rust-skill".into(),
                description: "rust conventions".into(),
                content: "use cargo".into(),
                score: 0.8,
                source_path: None,
                kind: DocumentKind::default(),
            },
            ContextMatch {
                name: "nix-skill".into(),
                description: "nix stuff".into(),
                content: "use flakes".into(),
                score: 0.6,
                source_path: None,
                kind: DocumentKind::default(),
            },
        ];

        let deduped = deduplicate_matches(matches);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn deduplicate_excludes_known_files() {
        let matches = vec![
            ContextMatch {
                name: "lib.rs::Config".into(),
                description: "src/lib.rs:1..10".into(),
                content: "struct Config".into(),
                score: 0.9,
                source_path: Some(PathBuf::from("src/lib.rs")),
                kind: DocumentKind::default(),
            },
            ContextMatch {
                name: "rust-skill".into(),
                description: "rust conventions".into(),
                content: "use cargo".into(),
                score: 0.7,
                source_path: None,
                kind: DocumentKind::default(),
            },
        ];

        let known = [PathBuf::from("src/lib.rs")];
        let filtered = exclude_known_files(matches, &known);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "rust-skill");
    }

    #[test]
    fn deduplicate_empty_input() {
        assert!(deduplicate_matches(vec![]).is_empty());
        let empty: [PathBuf; 0] = [];
        assert!(exclude_known_files(vec![], &empty).is_empty());
    }

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

    // -- embedding quality benchmarks --
    // run with: cargo test -p mush-ext --features embeddings -- --ignored bench

    /// a query/expected-match pair for measuring retrieval quality
    struct BenchCase {
        query: &'static str,
        /// document names that should appear in top results
        expected: &'static [&'static str],
    }

    /// build a set of realistic tool/skill documents for benchmarking
    fn bench_documents() -> Vec<ContextDocument> {
        let tools: Vec<(&str, &str)> = vec![
            ("read", "read a file's contents, supports text and images"),
            ("write", "create or overwrite a file with new content"),
            ("edit", "replace exact text in a file with new text"),
            ("bash", "execute a shell command and return stdout/stderr"),
            (
                "web_search",
                "search the web and return results with titles and snippets",
            ),
            (
                "web_fetch",
                "fetch a URL and return its content as markdown or text",
            ),
            (
                "lsp_diagnostics",
                "get compiler errors and warnings from the language server",
            ),
            ("send_message", "send a message to another agent pane"),
            (
                "read_state",
                "read from shared key-value state across panes",
            ),
            (
                "write_state",
                "write to shared key-value state across panes",
            ),
        ];

        let skills: Vec<(&str, &str)> = vec![
            (
                "jj",
                "jujutsu version control system reference and workflows",
            ),
            (
                "nix",
                "nix, nixos, and flake conventions and best practices",
            ),
            (
                "rust-idioms",
                "rust idioms, antipatterns, type-driven design, async patterns",
            ),
            (
                "nix-docker",
                "building docker/OCI images with nix dockerTools",
            ),
            (
                "azure-devops",
                "azure devops CLI for repos, pipelines, boards, PRs",
            ),
            (
                "pr-draft",
                "draft a pull request description respecting repo guidelines",
            ),
            (
                "remote-rebuild",
                "build and deploy nixos configurations to remote hosts",
            ),
            (
                "email-tidy",
                "organise and clean up protonmail inbox via himalaya CLI",
            ),
        ];

        let mut docs = Vec::new();
        for (name, desc) in &tools {
            docs.push(ContextDocument {
                name: name.to_string(),
                description: desc.to_string(),
                content: String::new(),
                embed_text: format!("{name}: {desc}"),
                source_path: None,
                kind: DocumentKind::Tool,
            });
        }
        for (name, desc) in &skills {
            docs.push(ContextDocument {
                name: name.to_string(),
                description: desc.to_string(),
                content: format!("# {name}\n\n{desc}\n\ndetailed instructions here..."),
                embed_text: format!("{name}: {desc}"),
                source_path: None,
                kind: DocumentKind::Skill,
            });
        }
        docs
    }

    fn bench_cases() -> Vec<BenchCase> {
        vec![
            BenchCase {
                query: "how do I commit with jujutsu?",
                expected: &["jj"],
            },
            BenchCase {
                query: "write a nix flake for my project",
                expected: &["nix"],
            },
            BenchCase {
                query: "build a docker image with nix",
                expected: &["nix-docker"],
            },
            BenchCase {
                query: "read the contents of main.rs",
                expected: &["read"],
            },
            BenchCase {
                query: "run cargo test",
                expected: &["bash"],
            },
            BenchCase {
                query: "search for rust async patterns",
                expected: &["rust-idioms", "web_search"],
            },
            BenchCase {
                query: "fix the compiler error on line 42",
                expected: &["lsp_diagnostics", "edit"],
            },
            BenchCase {
                query: "create a new file called lib.rs",
                expected: &["write"],
            },
            BenchCase {
                query: "deploy nixos to the server",
                expected: &["remote-rebuild"],
            },
            BenchCase {
                query: "draft the PR description",
                expected: &["pr-draft"],
            },
            BenchCase {
                query: "clean up my email inbox",
                expected: &["email-tidy"],
            },
            BenchCase {
                query: "set up an azure devops pipeline",
                expected: &["azure-devops"],
            },
            BenchCase {
                query: "ask another agent to review the code",
                expected: &["send_message"],
            },
        ]
    }

    fn run_benchmark(model: EmbeddingModelChoice) -> (usize, usize, Vec<String>) {
        let docs = bench_documents();
        let index = ContextIndex::build_with_model(docs, model).expect("failed to build index");

        let cases = bench_cases();
        let mut hits = 0;
        let mut total = 0;
        let mut report = Vec::new();

        for case in &cases {
            let results = index.search(case.query, 5, 0.0);
            let top_names: Vec<&str> = results.iter().map(|m| m.name.as_str()).collect();

            let mut line = format!("  query: {:50} top: [", format!("\"{}\"", case.query));
            for (i, r) in results.iter().take(5).enumerate() {
                if i > 0 {
                    line.push_str(", ");
                }
                line.push_str(&format!("{} ({:.3})", r.name, r.score));
            }
            line.push(']');

            for &expected in case.expected {
                total += 1;
                if top_names.contains(&expected) {
                    hits += 1;
                } else {
                    line.push_str(&format!("  MISS: {expected}"));
                }
            }
            report.push(line);
        }

        report.insert(0, format!("model: {model:?} — recall@5: {hits}/{total}"));
        (hits, total, report)
    }

    #[test]
    #[ignore]
    fn bench_coderank_retrieval() {
        let (hits, total, report) = run_benchmark(EmbeddingModelChoice::CodeRankEmbed);
        for line in &report {
            eprintln!("{line}");
        }
        // expect at least 60% recall as a baseline
        assert!(
            hits * 100 / total >= 60,
            "CodeRankEmbed recall too low: {hits}/{total}"
        );
    }

    #[test]
    #[ignore]
    fn bench_gemma_retrieval() {
        let (hits, total, report) = run_benchmark(EmbeddingModelChoice::Gemma300M);
        for line in &report {
            eprintln!("{line}");
        }
        assert!(
            hits * 100 / total >= 60,
            "Gemma300M recall too low: {hits}/{total}"
        );
    }

    #[test]
    #[ignore]
    fn bench_lazy_vs_eager_tokens() {
        let docs = bench_documents();
        let index = ContextIndex::build_with_model(docs, EmbeddingModelChoice::CodeRankEmbed)
            .expect("failed to build index");

        let cases = bench_cases();

        eprintln!("\n=== lazy vs eager token usage ===");
        eprintln!(
            "{:50} {:>10} {:>10} {:>7}",
            "query", "eager", "lazy", "saved"
        );
        eprintln!("{}", "-".repeat(80));

        let mut total_eager = 0usize;
        let mut total_lazy = 0usize;

        for case in &cases {
            let results = index.search(case.query, 5, 0.0);

            // eager: inject full content for all matches
            let eager = route_matches(&results, 0.0, true);
            // lazy: use the configured threshold (0.5)
            let lazy = route_matches(&results, 0.5, true);

            // rough token estimate: chars / 4
            let eager_tokens = eager.len() / 4;
            let lazy_tokens = lazy.len() / 4;
            let saved = eager_tokens.saturating_sub(lazy_tokens);

            total_eager += eager_tokens;
            total_lazy += lazy_tokens;

            eprintln!(
                "{:50} {:>10} {:>10} {:>6}",
                format!("\"{}\"", case.query),
                eager_tokens,
                lazy_tokens,
                saved,
            );
        }

        let total_saved = total_eager.saturating_sub(total_lazy);
        let pct = if total_eager > 0 {
            total_saved as f64 / total_eager as f64 * 100.0
        } else {
            0.0
        };
        eprintln!("{}", "-".repeat(80));
        eprintln!(
            "{:50} {:>10} {:>10} {:>6} ({:.0}% saved)",
            "TOTAL", total_eager, total_lazy, total_saved, pct
        );
    }

    #[test]
    #[ignore]
    fn bench_compare_models() {
        let (cr_hits, cr_total, cr_report) = run_benchmark(EmbeddingModelChoice::CodeRankEmbed);
        eprintln!();
        let (gm_hits, gm_total, gm_report) = run_benchmark(EmbeddingModelChoice::Gemma300M);

        eprintln!("\n=== comparison ===");
        for line in &cr_report {
            eprintln!("{line}");
        }
        eprintln!();
        for line in &gm_report {
            eprintln!("{line}");
        }
        eprintln!(
            "\nCodeRankEmbed: {cr_hits}/{cr_total} ({:.0}%)",
            cr_hits as f64 / cr_total as f64 * 100.0
        );
        eprintln!(
            "Gemma300M:     {gm_hits}/{gm_total} ({:.0}%)",
            gm_hits as f64 / gm_total as f64 * 100.0
        );
    }
}
