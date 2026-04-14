//! search index, similarity computation, and result processing

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fastembed::TextEmbedding;

use super::embedding::EmbeddingModelChoice;
use super::{ContextDocument, ContextError, ContextMatch};

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
        let mut model = super::embedding::create_embedding_model(choice)?;

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
        let mut seen = HashSet::new();
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

/// deduplicate matches by source file path, keeping the highest-scored
/// result per file. results without a source path (e.g. skills) are
/// kept as-is. output is sorted by descending score.
pub fn deduplicate_matches(matches: Vec<ContextMatch>) -> Vec<ContextMatch> {
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

#[cfg(test)]
mod tests {
    use super::*;

    // cosine similarity

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

    // deduplication

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
        assert_eq!(deduped[0].score, 0.9);
        assert_eq!(deduped[1].score, 0.8);
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

    // integration tests (require model download)

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
}
