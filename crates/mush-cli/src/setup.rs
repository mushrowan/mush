//! shared setup helpers for both print and TUI modes

use color_eyre::eyre::{Result, eyre};
use mush_ai::models;
use mush_ai::providers;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_ext::loader;
use mush_session::compact;

use crate::config;

/// shared state built from CLI args + config, used by both print and TUI modes
pub struct AppSetup {
    pub cfg: config::Config,
    pub model: Model,
    /// separate model + options for compaction (None = use active model)
    pub compaction_model: Option<(Model, StreamOptions)>,
    pub registry: ApiRegistry,
    pub cwd: std::path::PathBuf,
    pub system_prompt: String,
    pub options: StreamOptions,
    pub thinking_prefs: std::collections::HashMap<String, ThinkingLevel>,
    pub tools: mush_agent::tool::ToolRegistry,
    pub debug_cache: bool,
    pub max_turns: usize,
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    /// live repo map context (updated by file watcher), kept alive by the watcher
    pub repo_map_context: Option<mush_agent::DynamicContext>,
    /// file-triggered rule injection callback
    pub file_rules: Option<mush_agent::FileRuleCallback>,
    /// LSP diagnostic injection callback
    pub lsp_diagnostics: Option<mush_agent::DiagnosticCallback>,
    /// MCP tool name/description pairs for semantic search
    pub tool_descriptions: Vec<(String, String)>,
    /// shared http client for all network calls
    pub http_client: reqwest::Client,
    /// holds the watcher alive (dropped when AppSetup is dropped)
    _repo_map_watcher: Option<mush_treesitter::RepoMapWatcher>,
}

/// CLI args needed for shared setup (avoids depending on clap struct)
pub struct SetupArgs {
    pub model: Option<String>,
    pub thinking: bool,
    pub max_tokens: Option<u64>,
    pub max_turns: Option<usize>,
    pub system: Option<String>,
    pub no_tools: bool,
    pub debug_cache: bool,
    pub output_sink: Option<mush_tools::bash::OutputSink>,
    pub timer: Option<crate::timing::PhaseTimer>,
}

impl AppSetup {
    /// build shared state from CLI args
    pub async fn init(args: SetupArgs) -> Result<(Self, Option<crate::timing::StartupReport>)> {
        let mut timer = args.timer;

        let cfg = config::load_config();
        let debug_cache = args.debug_cache || cfg.debug_cache;
        if let Some(t) = &mut timer {
            t.phase("config");
        }

        let model_id = args.model.unwrap_or_else(|| default_model_id(&cfg));

        let model = models::find_model_by_id(&model_id).ok_or_else(|| {
            eyre!(
                "unknown model: {model_id}\n\navailable models:\n{}",
                list_models_short()
            )
        })?;

        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build http client");

        let mut registry = ApiRegistry::new();
        providers::register_builtins(&mut registry, http_client.clone());
        if let Some(t) = &mut timer {
            t.phase("model + http");
        }

        let cwd = std::env::current_dir()?;

        let use_patch = mush_tools::uses_patch_tool(&model.id);
        let skip_batch = mush_tools::supports_native_parallel_calls(&model.id);
        let mut tools = if args.no_tools {
            mush_agent::tool::ToolRegistry::new()
        } else {
            mush_tools::builtin_tools_with_options(
                cwd.clone(),
                args.output_sink,
                use_patch,
                http_client.clone(),
            )
        };
        if let Some(t) = &mut timer {
            t.phase("builtin tools");
        }

        let mut tool_descriptions: Vec<(String, String)> = Vec::new();

        if !args.no_tools && !cfg.mcp.is_empty() {
            let (mcp_manager, mcp_tools) = mush_mcp::McpManager::connect_all(&cfg.mcp).await;
            if cfg.dynamic_mcp {
                let conns = mcp_manager.connections();
                if !conns.is_empty() {
                    let index = mush_mcp::dynamic::McpToolIndex::new(&conns);
                    tool_descriptions = index.tool_descriptions();
                    let dynamic = mush_mcp::dynamic::dynamic_mcp_tools(&conns);
                    tools.extend_shared(dynamic.iter().cloned());
                }
            } else {
                tools.extend_shared(mcp_tools.iter().cloned());
            }
        }
        if let Some(t) = &mut timer {
            t.phase("mcp");
        }

        // discover project context once for both system prompt and skill tools
        let project_context = loader::discover_project_context(&cwd);

        // register skill tools for strategies that use on-demand loading
        if !args.no_tools
            && cfg.retrieval.context_strategy.needs_skill_tools()
            && !project_context.skills.is_empty()
        {
            let skill_infos: Vec<mush_tools::skills::SkillInfo> = project_context
                .skills
                .iter()
                .map(|s| mush_tools::skills::SkillInfo {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    path: s.path.clone(),
                })
                .collect();
            let skill_tools = mush_tools::skills::skill_tools(skill_infos);
            tools.extend_shared(skill_tools.iter().cloned());
        }
        if let Some(t) = &mut timer {
            t.phase("project context");
        }

        let system_prompt = args
            .system
            .or(cfg.system_prompt.clone())
            .unwrap_or_else(|| {
                build_system_prompt_from_context(
                    &project_context,
                    &cwd,
                    cfg.retrieval.context_strategy,
                )
            });
        if let Some(t) = &mut timer {
            t.phase("system prompt");
        }

        let thinking_prefs = config::load_thinking_prefs();
        let thinking_level = resolve_thinking(args.thinking, &model, &thinking_prefs, &cfg);

        let mut options = StreamOptions {
            thinking: thinking_level,
            max_tokens: args.max_tokens.or(cfg.max_tokens).map(TokenCount::new),
            cache_retention: cfg.cache_retention,
            ..Default::default()
        };

        resolve_api_key(&mut options, &model, &cfg).await;

        // resolve optional compaction model
        let compaction_model = if let Some(ref id) = cfg.compaction_model {
            let m = models::find_model_by_id(id).unwrap_or_else(|| {
                eprintln!(
                    "\x1b[33mwarning: unknown compaction_model '{id}', falling back to active model\x1b[0m"
                );
                model.clone()
            });
            let mut compact_opts = options.clone();
            resolve_api_key(&mut compact_opts, &m, &cfg).await;
            Some((m, compact_opts))
        } else {
            None
        };
        if let Some(t) = &mut timer {
            t.phase("api keys");
        }

        let max_turns = args
            .max_turns
            .or(cfg.max_turns)
            .unwrap_or(mush_agent::DEFAULT_MAX_TURNS);

        let lifecycle_hooks = cfg.lifecycle_hooks();

        // start repo map file watcher for live updates
        let (repo_map_context, _repo_map_watcher) = if cfg.retrieval.repo_map {
            start_repo_map_watcher(&cwd, cfg.retrieval.context_budget)
        } else {
            (None, None)
        };
        if let Some(t) = &mut timer {
            t.phase("repo map");
        }

        // build file rule index from .mush/rules/
        let file_rules = build_file_rules(&cwd);

        // build LSP registry, diagnostic callback, and tools
        let (lsp_diagnostics, lsp_reg) = build_lsp_diagnostics(&cwd, &cfg);
        if let Some(reg) = &lsp_reg
            && !args.no_tools
        {
            let lsp_tool_list = mush_lsp::lsp_tools(reg.clone(), cwd.clone());
            for tool in lsp_tool_list {
                tools.register_shared(std::sync::Arc::from(tool));
            }
        }
        if let Some(t) = &mut timer {
            t.phase("lsp + rules");
        }

        // add batch tool last so it can dispatch to skill, MCP, LSP tools
        if !args.no_tools && !skip_batch {
            mush_tools::add_batch_tool(&mut tools);
        }

        let report = timer.map(|t| t.finish());

        Ok((
            Self {
                cfg,
                model,
                compaction_model,
                registry,
                cwd,
                system_prompt,
                options,
                thinking_prefs,
                tools,
                debug_cache,
                max_turns,
                lifecycle_hooks,
                repo_map_context,
                file_rules,
                lsp_diagnostics,
                tool_descriptions,
                http_client,
                _repo_map_watcher,
            },
            report,
        ))
    }
}

/// expand /template_name args... into template content
pub fn expand_template(prompt: &str) -> String {
    if !prompt.starts_with('/') {
        return prompt.to_string();
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let templates = mush_ext::discover_templates(&cwd);

    let parts: Vec<&str> = prompt[1..].splitn(2, ' ').collect();
    let name = parts[0];
    let args_str = parts.get(1).unwrap_or(&"");

    if let Some(tmpl) = mush_ext::find_template(&templates, name) {
        let args: Vec<&str> = if args_str.is_empty() {
            vec![]
        } else {
            args_str.split_whitespace().collect()
        };
        mush_ext::substitute_args(&tmpl.content, &args)
    } else {
        prompt.to_string()
    }
}

/// enrich error messages with actionable suggestions
pub fn format_error(error: &str) -> String {
    if error.contains("missing api key") {
        format!(
            "{error}\n\n\
             hint: set ANTHROPIC_API_KEY, OPENROUTER_API_KEY, or OPENAI_API_KEY, or run:\n  \
             mush login anthropic\n  \
             mush login openai-codex\n  \
             mush config  (to add api_keys in config.toml)"
        )
    } else if error.contains("no provider registered") {
        format!("{error}\n\nhint: the model's api type has no registered provider")
    } else {
        error.to_string()
    }
}

fn format_available_skills(skills: &[loader::Skill]) -> String {
    let mut skills = skills.to_vec();
    skills.sort_by(|left, right| left.name.cmp(&right.name));

    let mut out = String::from("<available_skills>\n");
    for skill in skills {
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <name>{}</name>\n", skill.name));
        out.push_str(&format!(
            "    <description>{}</description>\n",
            skill.description
        ));
        out.push_str(&format!(
            "    <location>{}</location>\n",
            skill.path.display()
        ));
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>");
    out
}

fn is_skill_name_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn whole_word_match_positions(query: &str, skill_name: &str) -> Vec<usize> {
    let mut positions = Vec::new();

    for (start, _) in query.match_indices(skill_name) {
        let end = start + skill_name.len();
        let before_ok = start == 0 || !is_skill_name_char(query.as_bytes()[start - 1]);
        let after_ok = end == query.len() || !is_skill_name_char(query.as_bytes()[end]);
        if before_ok && after_ok {
            positions.push(start);
        }
    }

    positions
}

fn explicit_skill_mentions(query: &str, skills: &[loader::Skill]) -> Vec<(usize, String)> {
    let mut matches = Vec::new();
    let bytes = query.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }

        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && is_skill_name_char(bytes[end]) {
            end += 1;
        }

        if end > start {
            let mention = &query[start..end];
            if let Some(skill) = skills
                .iter()
                .find(|skill| skill.name.eq_ignore_ascii_case(mention))
            {
                matches.push((index, skill.name.clone()));
            }
            index = end;
        } else {
            index += 1;
        }
    }

    matches
}

fn has_plain_skill_cue(query: &str, start: usize, end: usize) -> bool {
    const BEFORE_CUES: [&str; 8] = [
        "use", "using", "with", "follow", "load", "apply", "need", "needs",
    ];
    const AFTER_CUES: [&str; 3] = ["skill", "workflow", "instructions"];

    let before_words: Vec<_> = query[..start].split_whitespace().rev().take(3).collect();
    if before_words.iter().any(|word| BEFORE_CUES.contains(word)) {
        return true;
    }

    query[end..]
        .split_whitespace()
        .next()
        .is_some_and(|word| AFTER_CUES.contains(&word))
}

fn plain_skill_mentions(query: &str, skills: &[loader::Skill]) -> Vec<(usize, String)> {
    let query = query.to_lowercase();
    let mut matches = Vec::new();

    for skill in skills {
        let skill_name = skill.name.to_lowercase();
        let distinctive = skill_name.contains(['-', '_']) || skill_name.len() > 6;
        for start in whole_word_match_positions(&query, &skill_name) {
            let end = start + skill_name.len();
            if distinctive || has_plain_skill_cue(&query, start, end) {
                matches.push((start, skill.name.clone()));
                break;
            }
        }
    }

    matches
}

fn requested_skill_mentions(query: &str, skills: &[loader::Skill]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut matches = explicit_skill_mentions(query, skills);
    matches.extend(plain_skill_mentions(query, skills));
    matches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    matches
        .into_iter()
        .filter_map(|(_, name)| {
            let key = name.to_lowercase();
            seen.insert(key).then_some(name)
        })
        .collect()
}

fn explicit_skill_hint(query: &str, skills: &[loader::Skill]) -> Option<String> {
    let matches = requested_skill_mentions(query, skills);
    if matches.is_empty() {
        return None;
    }

    Some(format!(
        "[relevant skills: {}. the user explicitly requested them. use the skill tool to load them before answering.]",
        matches.join(", ")
    ))
}

#[cfg(feature = "embeddings")]
fn join_prompt_hints(hints: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let mut merged = Vec::new();

    for hint in hints.into_iter().flatten() {
        if !hint.is_empty() && !merged.contains(&hint) {
            merged.push(hint);
        }
    }

    if merged.is_empty() {
        None
    } else {
        Some(merged.join("\n"))
    }
}

/// build the default system prompt from pre-discovered project context
fn build_system_prompt_from_context(
    context: &loader::ProjectContext,
    cwd: &std::path::Path,
    strategy: config::ContextStrategy,
) -> String {
    let cwd_str = cwd.display();
    let mut prompt = format!(
        "You are running inside mush, a minimal coding agent harness. \
         You help users by reading files, executing commands, \
         editing code, and writing new files.\n\n\
         Current working directory: {cwd_str}\n\n\
         Guidelines:\n\
         - Use bash for file operations like ls, grep, find\n\
         - Use read to examine files before editing\n\
         - Use edit for precise changes (old text must match exactly)\n\
         - Use write only for new files or complete rewrites\n\
         - Be concise in your responses\n\
         - Batch independent operations: when you need to read, edit, or operate on \
         multiple files that don't depend on each other, use the Batch tool to do them \
         all in a single call. This saves round-trips and helps you reason about \
         related changes holistically before making them"
    );

    for agents in &context.agents_md {
        prompt.push_str("\n\n# Project Context\n\n");
        prompt.push_str(&agents.content);
    }

    if !context.skills.is_empty() {
        match strategy {
            config::ContextStrategy::Prepended => {
                prompt.push_str(
                    "\n\nThe following skills provide specialised instructions for specific tasks.\n\
                     When a skill is relevant to the request, follow its instructions.\n",
                );
                for skill in &context.skills {
                    if let Ok(content) = std::fs::read_to_string(&skill.path) {
                        prompt.push_str(&format!(
                            "\n### {}\n{}\n\n{}\n",
                            skill.name, skill.description, content,
                        ));
                    }
                }
            }
            config::ContextStrategy::Summaries | config::ContextStrategy::Embedded => {
                prompt.push_str(
                    "\n\nSkills provide specialised instructions for specific tasks. \
                     Use the skill tool to load a skill when a task matches its description. \
                     If the user explicitly names a skill with `$skill-name`, load that skill before answering.\n\n",
                );
                prompt.push_str(&format_available_skills(&context.skills));
            }
            config::ContextStrategy::EmbedInject => {
                prompt.push_str(
                    "\n\nSpecialised skill instructions may be automatically provided \
                     with your messages when relevant to the task.\n",
                );
            }
        }
    }

    // repo map is now injected dynamically via the file watcher
    // (see start_repo_map_watcher and dynamic_system_context)

    prompt
}

fn format_nested_agents(cwd: &std::path::Path, agents: &[loader::AgentsMd]) -> Option<String> {
    if agents.is_empty() {
        return None;
    }

    let text: Vec<_> = agents
        .iter()
        .map(|agents| {
            let path = agents.path.strip_prefix(cwd).unwrap_or(&agents.path);
            format!("## {}\n{}", path.display(), agents.content.trim())
        })
        .collect();
    Some(text.join("\n\n"))
}

/// build file context callback from nested AGENTS.md and `.mush/rules/`
fn build_file_rules(cwd: &std::path::Path) -> Option<mush_agent::FileRuleCallback> {
    let rules = mush_ext::rules::discover_rules(cwd);
    let rule_index = if rules.is_empty() {
        None
    } else {
        let count = rules.len();
        eprintln!("\x1b[2mfound {count} rule files in .mush/rules/\x1b[0m");
        Some(std::sync::Arc::new(mush_ext::rules::RuleIndex::new(rules)))
    };

    let cwd = cwd.to_path_buf();
    Some(std::sync::Arc::new(move |path: &std::path::Path| {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };

        let nested_agents = loader::find_nested_agents_md(&cwd, &resolved);
        let mut parts = Vec::new();

        if let Some(text) = format_nested_agents(&cwd, &nested_agents) {
            parts.push(text);
        }

        if let Some(index) = &rule_index {
            let matched = index.match_file(path);
            if !matched.is_empty() {
                let text: Vec<_> = matched
                    .iter()
                    .map(|rule| format!("## {} rules\n{}", rule.name, rule.content))
                    .collect();
                parts.push(text.join("\n\n"));
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }))
}

/// build LSP diagnostic callback and shared registry if enabled in config
fn build_lsp_diagnostics(
    cwd: &std::path::Path,
    cfg: &config::Config,
) -> (
    Option<mush_agent::DiagnosticCallback>,
    Option<std::sync::Arc<mush_lsp::LspRegistry>>,
) {
    if !cfg.lsp.diagnostics {
        return (None, None);
    }

    let mut registry = mush_lsp::LspRegistry::new(cwd.to_path_buf());

    // apply user-configured server overrides
    for (lang_name, entry) in &cfg.lsp.servers {
        if let Some(language) = parse_language_name(lang_name) {
            registry.add_override(mush_lsp::ServerConfig {
                language,
                command: entry.command.clone(),
                args: entry.args.clone(),
            });
        }
    }

    let registry = std::sync::Arc::new(registry);

    eprintln!("\x1b[2mLSP diagnostics enabled\x1b[0m");

    let diag_registry = registry.clone();
    let callback: mush_agent::DiagnosticCallback =
        std::sync::Arc::new(move |path: &std::path::Path| {
            let registry = diag_registry.clone();
            let path = path.to_path_buf();
            Box::pin(async move {
                let text = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                match registry.notify_and_diagnose(&path, &text).await {
                    Ok(diags) if !diags.is_empty() => {
                        Some(mush_lsp::format_diagnostics(&path, &diags))
                    }
                    _ => None,
                }
            })
        });

    (Some(callback), Some(registry))
}

/// map config language name to mush-treesitter Language
fn parse_language_name(name: &str) -> Option<mush_treesitter::Language> {
    match name.to_lowercase().as_str() {
        "rust" => Some(mush_treesitter::Language::Rust),
        "python" => Some(mush_treesitter::Language::Python),
        "javascript" | "js" => Some(mush_treesitter::Language::JavaScript),
        "typescript" | "ts" => Some(mush_treesitter::Language::TypeScript),
        "tsx" => Some(mush_treesitter::Language::Tsx),
        "go" => Some(mush_treesitter::Language::Go),
        "c" => Some(mush_treesitter::Language::C),
        "cpp" | "c++" => Some(mush_treesitter::Language::Cpp),
        "java" => Some(mush_treesitter::Language::Java),
        "bash" | "shell" | "sh" => Some(mush_treesitter::Language::Bash),
        "nix" => Some(mush_treesitter::Language::Nix),
        _ => None,
    }
}

/// start the repo map file watcher and return a dynamic context callback
///
/// the watcher keeps the repo map up to date as files change during
/// the session. the callback is called before each LLM call to get
/// the latest map text for the system prompt.
fn start_repo_map_watcher(
    cwd: &std::path::Path,
    token_budget: usize,
) -> (
    Option<mush_agent::DynamicContext>,
    Option<mush_treesitter::RepoMapWatcher>,
) {
    let watcher = match mush_treesitter::RepoMapWatcher::start(cwd, token_budget) {
        Some(w) => w,
        None => {
            // fall back to a static one-shot map
            let repo_map = mush_treesitter::build_repo_map(cwd);
            if repo_map.files.is_empty() {
                return (None, None);
            }
            let map_text = repo_map.format_for_tokens(token_budget);
            if map_text.is_empty() {
                return (None, None);
            }
            let context = format_repo_map_context(&map_text);
            let ctx: mush_agent::DynamicContext =
                std::sync::Arc::new(move || Some(context.clone()));
            return (Some(ctx), None);
        }
    };

    let map_text = watcher.map_text().clone();
    let ctx: mush_agent::DynamicContext = std::sync::Arc::new(move || {
        let text = map_text.read().ok()?;
        if text.is_empty() {
            return None;
        }
        Some(format_repo_map_context(&text))
    });

    (Some(ctx), Some(watcher))
}

fn format_repo_map_context(map_text: &str) -> String {
    format!(
        "# Repository Map\n\n\
         The following is a ranked summary of the most important files \
         and symbols in this repository:\n\n\
         {map_text}"
    )
}

/// build a prompt enricher that returns a relevance hint for the user message
#[cfg(feature = "embeddings")]
pub fn build_prompt_enricher(
    cwd: &std::path::Path,
    retrieval: &config::RetrievalConfig,
    tool_descriptions: &[(String, String)],
) -> Option<mush_tui::PromptEnricher> {
    use mush_ext::context;
    use std::sync::Arc;

    let project = loader::discover_project_context(cwd);
    let skills = Arc::new(project.skills.clone());
    let supports_explicit_skill_hints =
        retrieval.context_strategy.needs_skill_tools() && !skills.is_empty();

    if !retrieval.context_strategy.needs_embeddings() {
        if !supports_explicit_skill_hints {
            return None;
        }

        return Some(Arc::new(move |query: &str| {
            explicit_skill_hint(query, skills.as_ref())
        }));
    }

    let has_skills = !project.skills.is_empty();
    let has_tools = !tool_descriptions.is_empty();

    if !has_skills && !has_tools {
        eprintln!("\x1b[2mno skills or tools discovered, enricher disabled\x1b[0m");
        return None;
    }

    let mut docs = if has_skills {
        eprintln!(
            "\x1b[2mfound {} skills, building index...\x1b[0m",
            project.skills.len()
        );
        context::build_skill_documents(&project.skills)
    } else {
        vec![]
    };

    if has_tools {
        eprintln!(
            "\x1b[2mindexing {} MCP tool descriptions\x1b[0m",
            tool_descriptions.len()
        );
        docs.extend(context::build_tool_documents(tool_descriptions));
    }

    if docs.is_empty() {
        if !supports_explicit_skill_hints {
            eprintln!(
                "\x1b[33mwarning: skills/tools found but none readable, enricher disabled\x1b[0m",
            );
            return None;
        }

        return Some(Arc::new(move |query: &str| {
            explicit_skill_hint(query, skills.as_ref())
        }));
    }

    let (model_choice, model_name) = match retrieval.embedding_model {
        config::EmbeddingModel::Coderank => (
            context::EmbeddingModelChoice::CodeRankEmbed,
            "CodeRankEmbed",
        ),
        config::EmbeddingModel::Gemma => (context::EmbeddingModelChoice::Gemma300M, "Gemma-300M"),
    };

    let include_hints = retrieval.context_strategy != config::ContextStrategy::EmbedInject;

    let doc_count = docs.len();
    match context::ContextIndex::build_with_model(docs, model_choice) {
        Ok(index) => {
            let index = Arc::new(index);
            eprintln!(
                "\x1b[2mindexed {doc_count} documents for auto-context ({model_name})\x1b[0m"
            );
            let threshold = retrieval.auto_load_threshold;
            Some(Arc::new(move |query: &str| {
                let explicit = if supports_explicit_skill_hints {
                    explicit_skill_hint(query, skills.as_ref())
                } else {
                    None
                };
                let matches = index.search(query, 3, 0.35);
                let routed = if matches.is_empty() {
                    None
                } else {
                    Some(context::route_matches(&matches, threshold, include_hints))
                };
                join_prompt_hints([explicit, routed])
            }))
        }
        Err(e) => {
            if supports_explicit_skill_hints {
                eprintln!(
                    "\x1b[33mwarning: failed to build context index: {e}, using explicit skill mentions only\x1b[0m"
                );
                Some(Arc::new(move |query: &str| {
                    explicit_skill_hint(query, skills.as_ref())
                }))
            } else {
                eprintln!("\x1b[33mwarning: failed to build context index: {e}\x1b[0m");
                None
            }
        }
    }
}

#[cfg(not(feature = "embeddings"))]
pub fn build_prompt_enricher(
    cwd: &std::path::Path,
    retrieval: &config::RetrievalConfig,
    _tool_descriptions: &[(String, String)],
) -> Option<mush_tui::PromptEnricher> {
    if !retrieval.context_strategy.needs_skill_tools() {
        return None;
    }

    let project = loader::discover_project_context(cwd);
    if project.skills.is_empty() {
        return None;
    }

    let skills = std::sync::Arc::new(project.skills);
    Some(std::sync::Arc::new(move |query: &str| {
        explicit_skill_hint(query, skills.as_ref())
    }))
}

/// resolve API key from config file for a given provider
pub fn config_api_key(cfg: &config::Config, provider: &Provider) -> Option<ApiKey> {
    let raw = match provider {
        Provider::Anthropic => cfg.api_keys.anthropic.clone(),
        Provider::OpenRouter => cfg.api_keys.openrouter.clone(),
        Provider::Custom(name) if name == "openai" => cfg.api_keys.openai.clone(),
        _ => None,
    };
    raw.and_then(ApiKey::new)
}

/// resolve the thinking level from CLI flags, saved prefs, and config
pub fn resolve_thinking(
    cli_thinking: bool,
    model: &Model,
    thinking_prefs: &std::collections::HashMap<String, ThinkingLevel>,
    cfg: &config::Config,
) -> Option<ThinkingLevel> {
    if cli_thinking {
        return Some(ThinkingLevel::High);
    }
    thinking_prefs
        .get(model.id.as_ref())
        .copied()
        .or(cfg.thinking)
        .map(ThinkingLevel::normalize_visible)
}

/// resolve API key for a model: env > config > oauth
pub async fn resolve_api_key(options: &mut StreamOptions, model: &Model, cfg: &config::Config) {
    if options.api_key.is_none()
        && mush_ai::env::env_api_key(&model.provider).is_none()
        && let Some(key) = config_api_key(cfg, &model.provider)
    {
        options.api_key = Some(key);
    }

    if options.api_key.is_none()
        && let Some(provider_id) = oauth_provider_id(model)
        && let Ok(Some(token)) = mush_ai::oauth::get_oauth_token(provider_id).await
    {
        options.api_key = ApiKey::new(token);
        options.account_id = oauth_account_id(provider_id);
    }
}

/// map model provider to oauth provider id
pub fn oauth_provider_id(model: &Model) -> Option<&'static str> {
    match &model.provider {
        Provider::Anthropic => Some("anthropic"),
        Provider::Custom(name) if name == "openai-codex" => Some("openai-codex"),
        _ => None,
    }
}

/// look up the account id from stored oauth credentials
pub fn oauth_account_id(provider_id: &str) -> Option<String> {
    mush_ai::oauth::load_credentials().ok().and_then(|store| {
        store
            .providers
            .get(provider_id)
            .and_then(|c| c.account_id.clone())
    })
}

/// pick the default model based on available auth
pub fn default_model_id(cfg: &config::Config) -> String {
    if let Some(model) = &cfg.model {
        return model.clone();
    }

    if let Some(model) = config::load_last_model()
        && models::find_model_by_id(&model).is_some()
    {
        return model;
    }

    let oauth_store = mush_ai::oauth::load_credentials().unwrap_or_default();

    let has_anthropic_auth = cfg.api_keys.anthropic.is_some()
        || std::env::var("ANTHROPIC_API_KEY").is_ok()
        || std::env::var("ANTHROPIC_OAUTH_TOKEN").is_ok()
        || oauth_store.providers.contains_key("anthropic");

    let has_openai_codex_auth = oauth_store.providers.contains_key("openai-codex");

    if has_anthropic_auth {
        return "claude-opus-4-6".into();
    }

    if has_openai_codex_auth {
        return "gpt-5.4".into();
    }

    "claude-opus-4-6".into()
}

/// short model list for error messages
pub fn list_models_short() -> String {
    models::all_models_with_user()
        .iter()
        .map(|m| format!("  {} ({})", m.id, m.provider))
        .collect::<Vec<_>>()
        .join("\n")
}

/// compact messages when approaching context limit.
/// uses escalating strategy: observation masking first, then LLM summarisation.
#[allow(clippy::too_many_arguments)]
pub async fn auto_compact(
    messages: Vec<Message>,
    context_tokens: TokenCount,
    context_window: TokenCount,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> Vec<Message> {
    let result = compact::auto_compact(
        messages,
        context_tokens,
        context_window,
        registry,
        model,
        options,
    )
    .await;

    if result.masked_count > 0 {
        eprintln!(
            "\x1b[2m(masked {} old tool outputs, ~{} tokens saved)\x1b[0m",
            result.masked_count, result.mask_tokens_saved
        );
    }
    if result.summarised_count > 0 {
        eprintln!(
            "\x1b[2m(compacted: {} messages summarised, {} kept)\x1b[0m",
            result.summarised_count,
            result.messages.len()
        );
    }

    let mut messages = result.messages;

    // post-compaction hooks (needs mush-agent types, so handled here)
    if let Some(hooks) = lifecycle_hooks
        && !hooks.post_compaction.is_empty()
    {
        let results = hooks.run_post_compaction(cwd).await;
        let output: String = results
            .iter()
            .filter(|r| !r.output.is_empty())
            .map(|r| r.output.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !output.is_empty() {
            eprintln!("\x1b[2m(post-compaction hook output injected)\x1b[0m");
            messages.push(Message::User(UserMessage {
                content: UserContent::Text(format!("[post-compaction hook output]\n{output}")),
                timestamp_ms: Timestamp::now(),
            }));
        }
        for r in &results {
            if !r.success {
                eprintln!(
                    "\x1b[33mwarning: post-compaction hook failed: {}\x1b[0m",
                    r.command
                );
            }
        }
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ext::loader::{ProjectContext, Skill};

    #[test]
    fn summaries_prompt_lists_available_skills() {
        let context = ProjectContext {
            agents_md: vec![],
            skills: vec![
                Skill {
                    name: "zeta".into(),
                    description: "zeta skill".into(),
                    path: "/tmp/zeta/SKILL.md".into(),
                },
                Skill {
                    name: "alpha".into(),
                    description: "alpha skill".into(),
                    path: "/tmp/alpha/SKILL.md".into(),
                },
            ],
        };

        let prompt = build_system_prompt_from_context(
            &context,
            std::path::Path::new("/repo"),
            config::ContextStrategy::Summaries,
        );

        let alpha = prompt.find("<name>alpha</name>").unwrap();
        let zeta = prompt.find("<name>zeta</name>").unwrap();

        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<description>alpha skill</description>"));
        assert!(prompt.contains("<location>/tmp/alpha/SKILL.md</location>"));
        assert!(prompt.contains("Use the skill tool to load a skill"));
        assert!(prompt.contains("$skill-name"));
        assert!(!prompt.contains("load_skill"));
        assert!(alpha < zeta);
    }

    #[test]
    fn explicit_skill_mentions_produce_hint() {
        let hint = explicit_skill_hint(
            "please use $zeta first, then $alpha",
            &[
                Skill {
                    name: "zeta".into(),
                    description: "zeta skill".into(),
                    path: "/tmp/zeta/SKILL.md".into(),
                },
                Skill {
                    name: "alpha".into(),
                    description: "alpha skill".into(),
                    path: "/tmp/alpha/SKILL.md".into(),
                },
            ],
        )
        .unwrap();

        assert!(hint.starts_with("[relevant skills: zeta, alpha."));
        assert!(hint.contains("use the skill tool"));
    }

    #[test]
    fn plain_skill_mentions_produce_hint() {
        let hint = explicit_skill_hint(
            "please use nix for this flake",
            &[Skill {
                name: "nix".into(),
                description: "nix skill".into(),
                path: "/tmp/nix/SKILL.md".into(),
            }],
        )
        .unwrap();

        assert!(hint.contains("[relevant skills: nix."));
    }

    #[test]
    fn plain_skill_mentions_ignore_substrings() {
        let hint = explicit_skill_hint(
            "this unix socket is acting up",
            &[Skill {
                name: "nix".into(),
                description: "nix skill".into(),
                path: "/tmp/nix/SKILL.md".into(),
            }],
        );

        assert!(hint.is_none());
    }

    #[test]
    fn file_context_includes_nested_agents_without_rules() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let nested = cwd.join("src/lib");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            cwd.join("src/AGENTS.md"),
            "# nested agents\nuse the local workflow\n",
        )
        .unwrap();

        let callback = build_file_rules(&cwd).unwrap();
        let text = callback(std::path::Path::new("src/lib/main.rs")).unwrap();

        assert!(text.contains("nested agents"));
        assert!(text.contains("use the local workflow"));
    }
}
