//! eval harness for comparing skill injection strategies
//!
//! run: cargo run -p mush-eval
//!
//! strategies:
//!   none           - no skill info at all
//!   prepended      - all skill bodies dumped into system prompt
//!   summaries      - names + descriptions in prompt, load_skill tool available
//!   embedded       - just names, embedding hints which to load, load_skill tool
//!   embed+summary  - embedded hints + summaries + load_skill tool
//!   embed_inject   - no tool, embedding auto-injects full skill body

use mush_eval::collect;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use mush_ai::providers;
use mush_ai::registry::{ApiRegistry, LlmContext, ToolDefinition};
use mush_ai::types::*;

// ── config ──────────────────────────────────────────────────────────────────

const MAX_TOKENS: u64 = 2048;
const SKILL_DIR: &str = env!("HOME");

fn skill_base() -> PathBuf {
    PathBuf::from(SKILL_DIR).join(".config/mush/skills")
}

// ── backend ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum Backend {
    Anthropic {
        model_id: String,
    },
    OpenAICompat {
        label: String,
        model_id: String,
        url: String,
    },
}

impl Backend {
    fn model_id(&self) -> &str {
        match self {
            Self::Anthropic { model_id } => model_id,
            Self::OpenAICompat { model_id, .. } => model_id,
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Anthropic { .. } => "anthropic",
            Self::OpenAICompat { label, .. } => label,
        }
    }

    /// build a Model struct for this backend
    fn to_model(&self) -> Model {
        let (id, api, provider, base_url) = match self {
            Self::Anthropic { model_id } => (
                model_id.clone(),
                Api::AnthropicMessages,
                Provider::Anthropic,
                BaseUrl::new("https://api.anthropic.com"),
            ),
            Self::OpenAICompat {
                model_id,
                url,
                label,
            } => (
                model_id.clone(),
                Api::OpenaiCompletions,
                Provider::Custom(label.clone()),
                BaseUrl::new(url.trim_end_matches("/v1")),
            ),
        };

        Model {
            id: id.into(),
            name: self.label().into(),
            api,
            provider,
            base_url,
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(128_000),
            max_output_tokens: TokenCount::new(MAX_TOKENS),
        }
    }
}

fn parse_args() -> Backend {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut backend = None;
    let mut model = None;
    let mut url = None;

    while i < args.len() {
        match args[i].as_str() {
            "--ollama" => backend = Some("ollama"),
            "--anthropic" => backend = Some("anthropic"),
            "--llamacpp" => backend = Some("llamacpp"),
            "--vllm" => backend = Some("vllm"),
            "--model" if i + 1 < args.len() => {
                i += 1;
                model = Some(args[i].clone());
            }
            "--url" if i + 1 < args.len() => {
                i += 1;
                url = Some(args[i].clone());
            }
            _ => {
                eprintln!("usage: mush-eval [BACKEND] [--model NAME] [--url URL]");
                eprintln!();
                eprintln!("backends:");
                eprintln!("  --ollama     local ollama (default, model: qwen2.5-coder:3b)");
                eprintln!("  --anthropic  anthropic API via oauth (model: claude-haiku-4-5)");
                eprintln!("  --llamacpp   llama.cpp server (model: default)");
                eprintln!("  --vllm       vLLM server (model: default)");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    match backend.unwrap_or("ollama") {
        "ollama" => Backend::OpenAICompat {
            label: "ollama".into(),
            model_id: model.unwrap_or_else(|| "qwen2.5-coder:3b".into()),
            url: url.unwrap_or_else(|| "http://localhost:11434/v1".into()),
        },
        "anthropic" => Backend::Anthropic {
            model_id: model.unwrap_or_else(|| "claude-haiku-4-5".into()),
        },
        "llamacpp" => Backend::OpenAICompat {
            label: "llamacpp".into(),
            model_id: model.unwrap_or_else(|| "default".into()),
            url: url.unwrap_or_else(|| "http://localhost:8080/v1".into()),
        },
        "vllm" => Backend::OpenAICompat {
            label: "vllm".into(),
            model_id: model.unwrap_or_else(|| "default".into()),
            url: url.unwrap_or_else(|| "http://localhost:8000/v1".into()),
        },
        _ => unreachable!(),
    }
}

// ── types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Skill {
    name: String,
    description: String,
    body: String,
}

#[derive(Debug, Clone)]
struct Problem {
    id: &'static str,
    question: &'static str,
    #[allow(dead_code)]
    relevant_skills: &'static [&'static str],
    pass_patterns: &'static [&'static str],
    fail_patterns: &'static [&'static str],
}

struct EvalResult {
    problem_id: String,
    strategy: String,
    passed: bool,
    response: String,
    #[allow(dead_code)]
    matched_pattern: Option<String>,
    failed_pattern: Option<String>,
    latency_ms: u64,
    usage: Usage,
}

// ── strategy trait ──────────────────────────────────────────────────────────

/// what a strategy produces before calling the API
struct Prepared {
    context: LlmContext,
}

trait Strategy: Send {
    fn name(&self) -> &str;
    fn prepare(&self, problem: &Problem, skills: &[Skill]) -> Prepared;

    /// handle a tool call from the model (multi-turn)
    fn handle_tool_call(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
        _skills: &[Skill],
    ) -> Option<String> {
        None
    }

    fn uses_tools(&self) -> bool {
        false
    }
}

// ── tool definition ─────────────────────────────────────────────────────────

fn load_skill_tool() -> ToolDefinition {
    ToolDefinition {
        name: "load_skill".into(),
        description: "load the full instructions for a skill by name. \
                       call this when you need detailed reference material \
                       to answer a question accurately."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "skill name to load"
                }
            },
            "required": ["name"]
        }),
    }
}

fn handle_load_skill(tool_input: &serde_json::Value, skill_map: &HashMap<String, Skill>) -> String {
    let name = tool_input
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    skill_map
        .get(name)
        .map(|s| s.body.clone())
        .unwrap_or_else(|| format!("unknown skill: {name}"))
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp_ms: Timestamp::now(),
    })
}

fn make_context(
    system: Option<String>,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
) -> LlmContext {
    LlmContext {
        system_prompt: system,
        messages,
        tools,
    }
}

// ── strategies ──────────────────────────────────────────────────────────────

struct NoneStrategy;

impl Strategy for NoneStrategy {
    fn name(&self) -> &str {
        "none"
    }

    fn prepare(&self, problem: &Problem, _skills: &[Skill]) -> Prepared {
        Prepared {
            context: make_context(
                Some("you are a coding assistant. answer concisely.".into()),
                vec![user_msg(problem.question)],
                vec![],
            ),
        }
    }
}

struct PrependedStrategy;

impl Strategy for PrependedStrategy {
    fn name(&self) -> &str {
        "prepended"
    }

    fn prepare(&self, problem: &Problem, skills: &[Skill]) -> Prepared {
        let mut system = String::from(
            "you are a coding assistant. answer concisely.\n\n\
             the following skill references are available to you:\n",
        );
        for skill in skills {
            system.push_str(&format!(
                "\n---\n## {}\n{}\n\n{}\n",
                skill.name, skill.description, skill.body
            ));
        }
        Prepared {
            context: make_context(Some(system), vec![user_msg(problem.question)], vec![]),
        }
    }
}

struct SummariesStrategy {
    skill_map: HashMap<String, Skill>,
}

impl Strategy for SummariesStrategy {
    fn name(&self) -> &str {
        "summaries"
    }

    fn prepare(&self, problem: &Problem, skills: &[Skill]) -> Prepared {
        let mut system = String::from(
            "you are a coding assistant. answer concisely.\n\n\
             you have skill references available. if you need detailed instructions \
             for a skill, call the load_skill tool with its name before answering.\n\n\
             available skills:\n",
        );
        for skill in skills {
            system.push_str(&format!("\n- **{}**: {}", skill.name, skill.description));
        }
        Prepared {
            context: make_context(
                Some(system),
                vec![user_msg(problem.question)],
                vec![load_skill_tool()],
            ),
        }
    }

    fn handle_tool_call(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        _skills: &[Skill],
    ) -> Option<String> {
        (tool_name == "load_skill").then(|| handle_load_skill(tool_input, &self.skill_map))
    }

    fn uses_tools(&self) -> bool {
        true
    }
}

struct EmbeddedStrategy {
    index: mush_ext::context::ContextIndex,
    skill_map: HashMap<String, Skill>,
}

impl Strategy for EmbeddedStrategy {
    fn name(&self) -> &str {
        "embedded"
    }

    fn prepare(&self, problem: &Problem, skills: &[Skill]) -> Prepared {
        let matches = self.index.search(problem.question, 3, 0.1);

        let mut system = String::from(
            "you are a coding assistant. answer concisely.\n\n\
             you have skill references available via the load_skill tool.\n\n\
             available skills: ",
        );
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        system.push_str(&names.join(", "));

        if !matches.is_empty() {
            system.push_str(
                "\n\n[retrieval hint: for this query, these skills are likely relevant: ",
            );
            let hints: Vec<String> = matches
                .iter()
                .map(|m| format!("{} ({:.0}% match)", m.name, m.score * 100.0))
                .collect();
            system.push_str(&hints.join(", "));
            system.push_str(". consider loading them with load_skill before answering.]");
        }

        Prepared {
            context: make_context(
                Some(system),
                vec![user_msg(problem.question)],
                vec![load_skill_tool()],
            ),
        }
    }

    fn handle_tool_call(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        _skills: &[Skill],
    ) -> Option<String> {
        (tool_name == "load_skill").then(|| handle_load_skill(tool_input, &self.skill_map))
    }

    fn uses_tools(&self) -> bool {
        true
    }
}

struct EmbeddedSummariesStrategy {
    index: mush_ext::context::ContextIndex,
    skill_map: HashMap<String, Skill>,
}

impl Strategy for EmbeddedSummariesStrategy {
    fn name(&self) -> &str {
        "embed+summary"
    }

    fn prepare(&self, problem: &Problem, skills: &[Skill]) -> Prepared {
        let matches = self.index.search(problem.question, 3, 0.1);

        let mut system = String::from(
            "you are a coding assistant. answer concisely.\n\n\
             you have skill references available. if you need detailed instructions \
             for a skill, call the load_skill tool with its name before answering.\n\n\
             available skills:\n",
        );
        for skill in skills {
            system.push_str(&format!("\n- **{}**: {}", skill.name, skill.description));
        }

        if !matches.is_empty() {
            system.push_str(
                "\n\n[retrieval hint: for this query, these skills are likely relevant: ",
            );
            let hints: Vec<String> = matches
                .iter()
                .map(|m| format!("{} ({:.0}% match)", m.name, m.score * 100.0))
                .collect();
            system.push_str(&hints.join(", "));
            system.push_str(". consider loading them with load_skill before answering.]");
        }

        Prepared {
            context: make_context(
                Some(system),
                vec![user_msg(problem.question)],
                vec![load_skill_tool()],
            ),
        }
    }

    fn handle_tool_call(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        _skills: &[Skill],
    ) -> Option<String> {
        (tool_name == "load_skill").then(|| handle_load_skill(tool_input, &self.skill_map))
    }

    fn uses_tools(&self) -> bool {
        true
    }
}

struct EmbeddedNoToolStrategy {
    index: mush_ext::context::ContextIndex,
}

impl Strategy for EmbeddedNoToolStrategy {
    fn name(&self) -> &str {
        "embed_inject"
    }

    fn prepare(&self, problem: &Problem, _skills: &[Skill]) -> Prepared {
        let matches = self.index.search(problem.question, 3, 0.1);
        for m in &matches {
            eprintln!(
                "    embed_inject: q={:40} match={:20} score={:.3} kind={:?}",
                &problem.question[..problem.question.len().min(40)],
                m.name,
                m.score,
                m.kind
            );
        }
        let routed = mush_ext::context::route_matches(&matches, 0.3, false);

        let mut system = String::from("you are a coding assistant. answer concisely.");
        if !routed.is_empty() {
            system.push_str("\n\n");
            system.push_str(&routed);
        }

        Prepared {
            context: make_context(Some(system), vec![user_msg(problem.question)], vec![]),
        }
    }
}

// ── eval runner ─────────────────────────────────────────────────────────────

async fn run_eval(
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    strategy: &dyn Strategy,
    problem: &Problem,
    skills: &[Skill],
) -> EvalResult {
    let start = Instant::now();
    let prepared = strategy.prepare(problem, skills);
    let mut context = prepared.context;
    let mut final_text = String::new();
    let mut total_usage = Usage::default();
    let max_turns = if strategy.uses_tools() { 3 } else { 1 };

    let provider = match registry.get(model.api) {
        Some(p) => p,
        None => {
            return EvalResult {
                problem_id: problem.id.into(),
                strategy: strategy.name().into(),
                passed: false,
                response: format!("no provider for {:?}", model.api),
                matched_pattern: None,
                failed_pattern: None,
                latency_ms: start.elapsed().as_millis() as u64,
                usage: Usage::default(),
            };
        }
    };

    for _turn in 0..max_turns {
        let resp = match collect::collect_response(provider, model, &context, options).await {
            Ok(r) => r,
            Err(e) => {
                return EvalResult {
                    problem_id: problem.id.into(),
                    strategy: strategy.name().into(),
                    passed: false,
                    response: format!("API ERROR: {e}"),
                    matched_pattern: None,
                    failed_pattern: None,
                    latency_ms: start.elapsed().as_millis() as u64,
                    usage: total_usage,
                };
            }
        };

        total_usage.input_tokens = total_usage.input_tokens + resp.usage.input_tokens;
        total_usage.output_tokens = total_usage.output_tokens + resp.usage.output_tokens;
        total_usage.cache_read_tokens =
            total_usage.cache_read_tokens + resp.usage.cache_read_tokens;
        total_usage.cache_write_tokens =
            total_usage.cache_write_tokens + resp.usage.cache_write_tokens;

        // collect text and tool calls
        let mut tool_calls: Vec<&ToolCall> = Vec::new();
        for part in &resp.content {
            match part {
                AssistantContentPart::Text(t) => final_text.push_str(&t.text),
                AssistantContentPart::ToolCall(tc) => tool_calls.push(tc),
                AssistantContentPart::Thinking(_) => {}
            }
        }

        if tool_calls.is_empty() {
            break;
        }

        // add assistant message to context
        context.messages.push(Message::Assistant(resp.clone()));

        // handle each tool call
        for tc in &tool_calls {
            let result = strategy
                .handle_tool_call(&tc.name, &tc.arguments, skills)
                .unwrap_or_else(|| "unknown tool".into());

            context
                .messages
                .push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    content: vec![ToolResultContentPart::Text(TextContent { text: result })],
                    outcome: ToolOutcome::Success,
                    timestamp_ms: Timestamp::now(),
                }));
        }
    }

    // grade
    let lower = final_text.to_lowercase();
    let matched = problem
        .pass_patterns
        .iter()
        .find(|p| lower.contains(&p.to_lowercase()))
        .map(|p| p.to_string());
    let failed = problem
        .fail_patterns
        .iter()
        .find(|p| lower.contains(&p.to_lowercase()))
        .map(|p| p.to_string());
    let passed = matched.is_some() && failed.is_none();

    EvalResult {
        problem_id: problem.id.into(),
        strategy: strategy.name().into(),
        passed,
        response: final_text,
        matched_pattern: matched,
        failed_pattern: failed,
        latency_ms: start.elapsed().as_millis() as u64,
        usage: total_usage,
    }
}

// ── skill loading ───────────────────────────────────────────────────────────

fn load_skills() -> Vec<Skill> {
    let base = skill_base();
    let mut skills = Vec::new();
    let Ok(entries) = std::fs::read_dir(&base) else {
        eprintln!("warning: cannot read skill dir {}", base.display());
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path().join("SKILL.md");
        if !path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };

        // parse frontmatter
        let (name, description, body) = if content.starts_with("---") {
            if let Some(end) = content[3..].find("---") {
                let front = &content[3..3 + end];
                let body = content[3 + end + 3..].trim().to_string();
                let mut name = entry.file_name().to_string_lossy().to_string();
                let mut desc = String::new();
                for line in front.lines() {
                    if let Some(n) = line.strip_prefix("name:") {
                        name = n.trim().to_string();
                    }
                    if let Some(d) = line.strip_prefix("description:") {
                        desc = d.trim().to_string();
                    }
                }
                (name, desc, body)
            } else {
                (
                    entry.file_name().to_string_lossy().to_string(),
                    String::new(),
                    content,
                )
            }
        } else {
            (
                entry.file_name().to_string_lossy().to_string(),
                String::new(),
                content,
            )
        };

        skills.push(Skill {
            name,
            description,
            body,
        });
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

// ── problems ────────────────────────────────────────────────────────────────

fn problems() -> Vec<Problem> {
    vec![
        Problem {
            id: "jj-push-change",
            question: "i'm in a jj repo. i want to push my current working copy's parent \
                       directly to a remote without manually creating a bookmark first. \
                       what single jj command does this? just the command.",
            relevant_skills: &["jj"],
            pass_patterns: &["--change"],
            fail_patterns: &["git push origin"],
        },
        Problem {
            id: "jj-split-nointeractive",
            question: "i'm in a jj repo inside a CI pipeline (no TTY). i need to split my \
                       current commit so that only src/lib.rs and src/main.rs go into the \
                       first commit. how do i do this without interactive mode? \
                       just the command.",
            relevant_skills: &["jj"],
            pass_patterns: &["jj split"],
            fail_patterns: &["--interactive", "git"],
        },
        Problem {
            id: "jj-resolve-nointeractive",
            question: "i have a conflict in config.toml in my jj repo. i want to resolve it \
                       by taking the other side's version, non-interactively. \
                       what's the exact command?",
            relevant_skills: &["jj"],
            pass_patterns: &["internal:other"],
            fail_patterns: &["git checkout --theirs"],
        },
        Problem {
            id: "jj-revset-files",
            question: "in jj, i want to find all commits that modified any file under \
                       src/parser/. what revset expression do i use? just the expression.",
            relevant_skills: &["jj"],
            pass_patterns: &["files("],
            fail_patterns: &["git log"],
        },
        Problem {
            id: "jj-evolog",
            question: "in jj, how do i see how my current change evolved over time \
                       (all the snapshots)? just the command.",
            relevant_skills: &["jj"],
            pass_patterns: &["jj evolog"],
            fail_patterns: &["git reflog"],
        },
        Problem {
            id: "nix-docker-stream",
            question: "in nix, i want to build a docker image that streams to stdout without \
                       materialising on disk. which dockerTools function should i use? \
                       just the function name.",
            relevant_skills: &["nix-docker"],
            pass_patterns: &["streamLayeredImage"],
            fail_patterns: &[],
        },
        Problem {
            id: "nix-docker-nonroot",
            question: "i'm writing a nix docker image with streamLayeredImage. i want to run \
                       as a non-root user. how do i set this up? show the key nix attributes \
                       needed (fakeRootCommands, runAsRoot, or config).",
            relevant_skills: &["nix-docker"],
            pass_patterns: &["fakeRootCommands", "runAsRoot", "config"],
            fail_patterns: &[],
        },
        Problem {
            id: "rust-let-chains",
            question: "in nightly rust, how do i combine an if-let with an extra boolean \
                       condition in a single if? show a short example.",
            relevant_skills: &["rust-idioms"],
            pass_patterns: &["if let", "&&"],
            fail_patterns: &[],
        },
        Problem {
            id: "jj-describe-agent",
            question: "i'm building a CI script that uses jj. i need to set a commit message \
                       without spawning an editor (no TTY available). what flag must i use \
                       with jj describe? just the flag.",
            relevant_skills: &["jj"],
            pass_patterns: &["-m"],
            fail_patterns: &[],
        },
    ]
}

// ── main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let backend = parse_args();
    let skills = load_skills();
    eprintln!("backend: {} ({})", backend.label(), backend.model_id());
    eprintln!(
        "loaded {} skills: {}",
        skills.len(),
        skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // set up provider registry
    let http_client = reqwest::Client::new();
    let mut registry = ApiRegistry::new();
    providers::register_builtins(&mut registry, http_client);

    let model = backend.to_model();

    // stream options (api key for anthropic, dummy for local)
    let options = match &backend {
        Backend::Anthropic { .. } => {
            let token = mush_ai::oauth::get_anthropic_oauth_token()
                .await
                .expect("failed to get oauth token")
                .expect("no oauth token found - run mush once to authenticate");
            StreamOptions {
                api_key: ApiKey::new(token),
                max_tokens: Some(TokenCount::new(MAX_TOKENS)),
                ..Default::default()
            }
        }
        Backend::OpenAICompat { .. } => StreamOptions {
            api_key: ApiKey::new("not-needed"),
            max_tokens: Some(TokenCount::new(MAX_TOKENS)),
            ..Default::default()
        },
    };

    // build embedding indices (need separate instances since ContextIndex
    // holds a mutex on the model)
    let docs: Vec<mush_ext::context::ContextDocument> = skills
        .iter()
        .map(|s| {
            mush_ext::context::ContextDocument::new(
                s.name.clone(),
                s.description.clone(),
                s.body.clone(),
                Some(skill_base().join(&s.name).join("SKILL.md")),
                mush_ext::context::DocumentKind::Skill,
            )
        })
        .collect();

    let skill_map: HashMap<String, Skill> =
        skills.iter().map(|s| (s.name.clone(), s.clone())).collect();

    let index1 = mush_ext::context::ContextIndex::build(docs.clone())
        .expect("failed to build embedding index");
    let index2 = mush_ext::context::ContextIndex::build(docs.clone())
        .expect("failed to build embedding index");
    // for embed_inject, use build_skill_documents to get section-level chunks
    let ext_skills: Vec<mush_ext::Skill> = skills
        .iter()
        .map(|s| mush_ext::Skill {
            name: s.name.clone(),
            description: s.description.clone(),
            path: skill_base().join(&s.name).join("SKILL.md"),
        })
        .collect();
    let section_docs = mush_ext::context::build_skill_documents(&ext_skills);
    eprintln!(
        "embed_inject index: {} section chunks from {} skills",
        section_docs.len(),
        skills.len()
    );
    let index3 = mush_ext::context::ContextIndex::build(section_docs)
        .expect("failed to build section index");

    let strategies: Vec<Box<dyn Strategy>> = vec![
        Box::new(NoneStrategy),
        Box::new(PrependedStrategy),
        Box::new(SummariesStrategy {
            skill_map: skill_map.clone(),
        }),
        Box::new(EmbeddedStrategy {
            index: index1,
            skill_map: skill_map.clone(),
        }),
        Box::new(EmbeddedSummariesStrategy {
            index: index2,
            skill_map,
        }),
        Box::new(EmbeddedNoToolStrategy { index: index3 }),
    ];

    let problems = problems();
    let strat_names: Vec<&str> = strategies.iter().map(|s| s.name()).collect();
    eprintln!(
        "running {} problems x {} strategies: {}\n",
        problems.len(),
        strategies.len(),
        strat_names.join(", ")
    );

    let mut results: Vec<EvalResult> = Vec::new();

    for problem in &problems {
        eprintln!("--- {} ---", problem.id);
        for strategy in &strategies {
            let result = run_eval(
                &registry,
                &model,
                &options,
                strategy.as_ref(),
                problem,
                &skills,
            )
            .await;
            let status = if result.passed { "✓" } else { "✗" };
            let in_tok = result.usage.input_tokens.get();
            let out_tok = result.usage.output_tokens.get();
            let tok = if in_tok > 0 {
                format!("  [{in_tok} in / {out_tok} out]")
            } else {
                String::new()
            };
            eprintln!(
                "  {:14} {} ({}ms){tok}{}",
                result.strategy,
                status,
                result.latency_ms,
                if !result.passed {
                    if let Some(ref fp) = result.failed_pattern {
                        format!("  [matched fail: {fp}]")
                    } else {
                        "  [no pass match]".into()
                    }
                } else {
                    String::new()
                }
            );
            results.push(result);
        }
    }

    // summary table
    eprintln!("\n=== summary ===\n");
    eprint!("{:35}", "problem");
    for name in &strat_names {
        eprint!(" {:>14}", name);
    }
    eprintln!();
    eprintln!("{}", "-".repeat(24 + strat_names.len() * 15));

    for problem in &problems {
        eprint!("{:35}", problem.id);
        for name in &strat_names {
            let r = results
                .iter()
                .find(|r| r.problem_id == problem.id && r.strategy == *name);
            let sym = r.map(|r| if r.passed { "✓" } else { "✗" }).unwrap_or("?");
            eprint!(" {:>14}", sym);
        }
        eprintln!();
    }

    eprintln!("{}", "-".repeat(24 + strat_names.len() * 15));
    for name in &strat_names {
        let strat_results: Vec<&EvalResult> =
            results.iter().filter(|r| r.strategy == *name).collect();
        let passed = strat_results.iter().filter(|r| r.passed).count();
        let total = problems.len();
        eprint!(
            "{:24} {:>3}/{} ({:>3.0}%)",
            name,
            passed,
            total,
            passed as f64 / total as f64 * 100.0
        );
        let avg_ms: u64 = strat_results.iter().map(|r| r.latency_ms).sum::<u64>() / total as u64;
        let total_in: u64 = strat_results
            .iter()
            .map(|r| r.usage.input_tokens.get())
            .sum();
        let total_out: u64 = strat_results
            .iter()
            .map(|r| r.usage.output_tokens.get())
            .sum();
        if total_in > 0 {
            eprintln!(
                "  avg {avg_ms}ms  tokens: {total_in} in / {total_out} out ({} total)",
                total_in + total_out
            );
        } else {
            eprintln!("  avg {avg_ms}ms");
        }
    }

    // failed responses
    let failures: Vec<&EvalResult> = results.iter().filter(|r| !r.passed).collect();
    if !failures.is_empty() {
        eprintln!("\n=== failed responses ===\n");
        for r in failures {
            eprintln!("-- {} / {} --", r.problem_id, r.strategy);
            let display: String = r.response.chars().take(500).collect();
            let display = if display.len() < r.response.len() {
                format!("{display}...")
            } else {
                display
            };
            eprintln!("{display}\n");
        }
    }
}
