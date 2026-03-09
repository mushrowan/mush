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
    pub tools: Vec<Box<dyn mush_agent::tool::AgentTool>>,
    pub debug_cache: bool,
    pub max_turns: usize,
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
        let mut tools: Vec<Box<dyn mush_agent::tool::AgentTool>> = if args.no_tools {
            vec![]
        } else {
            mush_tools::builtin_tools_with_options(
                cwd.clone(),
                args.output_sink,
                use_patch,
                skip_batch,
            )
        };

        if !args.no_tools && !cfg.mcp.is_empty() {
            let (_mcp_manager, mcp_tools) = mush_mcp::McpManager::connect_all(&cfg.mcp).await;
            tools.extend(mcp_tools);
        }

        let system_prompt = args
            .system
            .or(cfg.system_prompt.clone())
            .unwrap_or_else(|| build_system_prompt(&cwd));

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

/// build the default system prompt with project context and skills
pub fn build_system_prompt(cwd: &std::path::Path) -> String {
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

    let context = loader::discover_project_context(cwd);
    for agents in &context.agents_md {
        prompt.push_str("\n\n# Project Context\n\n");
        prompt.push_str(&agents.content);
    }

    if !context.skills.is_empty() {
        prompt.push_str(
            "\n\nThe following skills provide specialized instructions for specific tasks.\n\
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

    prompt
}

/// build a prompt enricher that returns a relevance hint for the user message
#[cfg(feature = "embeddings")]
pub fn build_prompt_enricher(cwd: &std::path::Path) -> Option<mush_tui::PromptEnricher> {
    use mush_ext::context;
    use std::sync::Arc;

    let project = loader::discover_project_context(cwd);
    if project.skills.is_empty() {
        eprintln!("\x1b[2mno skills discovered, enricher disabled\x1b[0m");
        return None;
    }
    eprintln!(
        "\x1b[2mfound {} skills, building index...\x1b[0m",
        project.skills.len()
    );
    let docs = context::build_skill_documents(&project.skills);
    if docs.is_empty() {
        eprintln!(
            "\x1b[33mwarning: {} skills found but none readable, enricher disabled\x1b[0m",
            project.skills.len()
        );
        return None;
    }

    let doc_count = docs.len();
    match context::ContextIndex::build(docs) {
        Ok(index) => {
            let index = Arc::new(index);
            eprintln!("\x1b[2mindexed {doc_count} skills for auto-context\x1b[0m");
            Some(Arc::new(move |query: &str| {
                let matches = index.search(query, 3, 0.35);
                if matches.is_empty() {
                    None
                } else {
                    Some(context::format_relevance_hint(&matches))
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
pub fn build_prompt_enricher(_cwd: &std::path::Path) -> Option<mush_tui::PromptEnricher> {
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

/// compact messages using LLM summarisation when approaching context limit.
/// `context_tokens` is the actual API-reported input token count from the last call
pub async fn auto_compact(
    messages: Vec<Message>,
    context_tokens: TokenCount,
    context_window: TokenCount,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
) -> Vec<Message> {
    // 95% of context window, using real token counts from the API
    if !context_tokens.exceeds_fraction(context_window, 95, 100) || messages.len() <= 10 {
        return messages;
    }

    eprintln!("\x1b[2m(compacting conversation...)\x1b[0m");

    let result = compact::llm_compact(messages, registry, model, options, Some(10)).await;
    eprintln!(
        "\x1b[2m(compacted: {} messages summarised, {} kept)\x1b[0m",
        result.summarised_count,
        result.messages.len()
    );
    result.messages
}
