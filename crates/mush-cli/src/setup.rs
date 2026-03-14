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
}

impl AppSetup {
    /// build shared state from CLI args
    pub async fn init(args: SetupArgs) -> Result<Self> {
        let cfg = config::load_config();
        let debug_cache = args.debug_cache || cfg.debug_cache;

        let model_id = args.model.unwrap_or_else(|| default_model_id(&cfg));

        let model = models::find_model_by_id(&model_id).ok_or_else(|| {
            eyre!(
                "unknown model: {model_id}\n\navailable models:\n{}",
                list_models_short()
            )
        })?;

        let mut registry = ApiRegistry::new();
        providers::register_builtins(&mut registry);

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
                skip_batch,
            )
        };

        let mut tool_descriptions: Vec<(String, String)> = Vec::new();

        if !args.no_tools && !cfg.mcp.is_empty() {
            let (mcp_manager, mcp_tools) = mush_mcp::McpManager::connect_all(&cfg.mcp).await;
            if cfg.dynamic_mcp {
                // register meta-tools for on-demand MCP tool discovery
                let conns = mcp_manager.connections();
                if !conns.is_empty() {
                    let index = mush_mcp::dynamic::McpToolIndex::new(&conns);
                    tool_descriptions = index.tool_descriptions();
                    let dynamic = mush_mcp::dynamic::dynamic_mcp_tools(&conns);
                    tools.extend_shared(dynamic.iter().cloned());
                }
            } else {
                // register all MCP tools directly (eager)
                tools.extend_shared(mcp_tools.iter().cloned());
            }
        }

        // discover project context once for both system prompt and skill tools
        let project_context = loader::discover_project_context(&cwd);

        // register skill tools for strategies that use on-demand loading
        if !args.no_tools && cfg.retrieval.context_strategy.needs_skill_tools() {
            if !project_context.skills.is_empty() {
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
        }

        let system_prompt = args
            .system
            .or(cfg.system_prompt.clone())
            .unwrap_or_else(|| build_system_prompt_from_context(&project_context, &cwd, cfg.retrieval.context_strategy));

        let thinking_prefs = config::load_thinking_prefs();
        let thinking_level = resolve_thinking(args.thinking, &model, &thinking_prefs, &cfg);

        let mut options = StreamOptions {
            thinking: thinking_level,
            max_tokens: args.max_tokens.or(cfg.max_tokens).map(TokenCount::new),
            cache_retention: cfg.cache_retention,
            ..Default::default()
        };

        resolve_api_key(&mut options, &model, &cfg).await;

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

        // build file rule index from .mush/rules/
        let file_rules = build_file_rules(&cwd);

        // build LSP registry, diagnostic callback, and tools
        let (lsp_diagnostics, lsp_reg) = build_lsp_diagnostics(&cwd, &cfg);
        if let Some(reg) = &lsp_reg {
            if !args.no_tools {
                let lsp_tool_list = mush_lsp::lsp_tools(reg.clone(), cwd.clone());
                for tool in lsp_tool_list {
                    tools.register_shared(std::sync::Arc::from(tool));
                }
            }
        }

        Ok(Self {
            cfg,
            model,
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
            _repo_map_watcher,
        })
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
                    "\n\nSpecialised skills are available. Use the list_skills tool to \
                     see what's available, then load_skill to read the full instructions \
                     when a task matches a skill's description.\n",
                );
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

/// build file rule callback from `.mush/rules/` directory
fn build_file_rules(cwd: &std::path::Path) -> Option<mush_agent::FileRuleCallback> {
    let rules = mush_ext::rules::discover_rules(cwd);
    if rules.is_empty() {
        return None;
    }
    let count = rules.len();
    let index = std::sync::Arc::new(mush_ext::rules::RuleIndex::new(rules));
    eprintln!("\x1b[2mfound {count} rule files in .mush/rules/\x1b[0m");
    Some(std::sync::Arc::new(move |path: &std::path::Path| {
        let matched = index.match_file(path);
        if matched.is_empty() {
            return None;
        }
        let text: Vec<_> = matched
            .iter()
            .map(|r| format!("## {} rules\n{}", r.name, r.content))
            .collect();
        Some(text.join("\n\n"))
    }))
}

/// build LSP diagnostic callback and shared registry if enabled in config
fn build_lsp_diagnostics(
    cwd: &std::path::Path,
    cfg: &config::Config,
) -> (Option<mush_agent::DiagnosticCallback>, Option<std::sync::Arc<mush_lsp::LspRegistry>>) {
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
    let callback: mush_agent::DiagnosticCallback = std::sync::Arc::new(move |path: &std::path::Path| {
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
) -> (Option<mush_agent::DynamicContext>, Option<mush_treesitter::RepoMapWatcher>) {
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
    if !retrieval.context_strategy.needs_embeddings() {
        return None;
    }
    use mush_ext::context;
    use std::sync::Arc;

    let project = loader::discover_project_context(cwd);
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
        eprintln!(
            "\x1b[33mwarning: skills/tools found but none readable, enricher disabled\x1b[0m",
        );
        return None;
    }

    let (model_choice, model_name) = match retrieval.embedding_model {
        config::EmbeddingModel::Coderank => {
            (context::EmbeddingModelChoice::CodeRankEmbed, "CodeRankEmbed")
        }
        config::EmbeddingModel::Gemma => {
            (context::EmbeddingModelChoice::Gemma300M, "Gemma-300M")
        }
    };

    let include_hints = retrieval.context_strategy != config::ContextStrategy::EmbedInject;

    let doc_count = docs.len();
    match context::ContextIndex::build_with_model(docs, model_choice) {
        Ok(index) => {
            let index = Arc::new(index);
            eprintln!("\x1b[2mindexed {doc_count} documents for auto-context ({model_name})\x1b[0m");
            let threshold = retrieval.auto_load_threshold;
            Some(Arc::new(move |query: &str| {
                let matches = index.search(query, 3, 0.35);
                if matches.is_empty() {
                    None
                } else {
                    Some(context::route_matches(&matches, threshold, include_hints))
                }
            }))
        }
        Err(e) => {
            eprintln!("\x1b[33mwarning: failed to build context index: {e}\x1b[0m");
            None
        }
    }
}

#[cfg(not(feature = "embeddings"))]
pub fn build_prompt_enricher(
    _cwd: &std::path::Path,
    _retrieval: &config::RetrievalConfig,
    _tool_descriptions: &[(String, String)],
) -> Option<mush_tui::PromptEnricher> {
    None
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
    let result =
        compact::auto_compact(messages, context_tokens, context_window, registry, model, options)
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
                content: UserContent::Text(format!(
                    "[post-compaction hook output]\n{output}"
                )),
                timestamp_ms: Timestamp::now(),
            }));
        }
        for r in &results {
            if !r.success {
                eprintln!("\x1b[33mwarning: post-compaction hook failed: {}\x1b[0m", r.command);
            }
        }
    }

    messages
}
