//! semantic context index for auto-loading relevant skills
//!
//! embeds skill descriptions using a local ONNX model (CodeRankEmbed
//! by default, with Gemma-300M fallback) and matches them against user
//! messages via cosine similarity. matched skills are auto-injected
//! into the system prompt.

mod documents;
mod embedding;
mod search;

use std::path::PathBuf;

pub use documents::{
    build_skill_documents, build_tool_documents, chunk_file_for_embedding, route_matches,
};
pub use embedding::EmbeddingModelChoice;
pub use search::{ContextIndex, deduplicate_matches, exclude_known_files};

/// what kind of document this is (for routing matches differently)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocumentKind {
    /// skill instruction file
    #[default]
    Skill,
    /// MCP or built-in tool description
    Tool,
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
    pub(crate) embed_text: String,
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

// benchmarks that span all submodules live here
#[cfg(test)]
mod tests {
    use super::*;

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
