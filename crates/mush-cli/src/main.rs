//! mush cli - fast little robot harness 🍄

mod config;

use clap::Parser;
use color_eyre::eyre::{Result, eyre};
use futures::StreamExt;

use mush_agent::{AgentConfig, AgentEvent, agent_loop, summarise_tool_args};
use mush_ai::models;
use mush_ai::providers;
use mush_ai::registry::ApiRegistry;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_ext::loader;
use mush_session::{Session, SessionStore};
use mush_tools::{builtin_tools, builtin_tools_with_sink};
use mush_tui::TuiConfig;

#[derive(Parser)]
#[command(
    name = "mush",
    version,
    about = "fast little robot harness 🍄",
    trailing_var_arg = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// prompt to send (enables print mode, no TUI)
    #[arg(short, long, num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,

    /// resume a previous session by id
    #[arg(short = 'c', long = "continue")]
    resume: Option<String>,

    /// model id to use
    #[arg(short, long)]
    model: Option<String>,

    /// enable extended thinking
    #[arg(long)]
    thinking: bool,

    /// max output tokens
    #[arg(long)]
    max_tokens: Option<u64>,

    /// max tool-calling turns before stopping
    #[arg(long)]
    max_turns: Option<usize>,

    /// print cache-read/write debug lines when available
    #[arg(long)]
    debug_cache: bool,

    /// system prompt override
    #[arg(long)]
    system: Option<String>,

    /// disable tools (just chat)
    #[arg(long)]
    no_tools: bool,

    /// don't save the session
    #[arg(long)]
    no_session: bool,
}

#[derive(clap::Subcommand)]
enum Command {
    /// log in to an oauth provider (e.g. mush login anthropic)
    Login {
        /// provider to log in to (e.g. anthropic)
        provider: Option<String>,
    },
    /// log out from an oauth provider
    Logout {
        /// provider to log out from (e.g. anthropic)
        provider: Option<String>,
    },
    /// list available models
    Models,
    /// list saved sessions
    Sessions,
    /// show current configuration and auth status
    Status,
    /// delete a saved session
    Delete {
        /// session id (prefix match)
        id: String,
    },
    /// open config file in $EDITOR
    Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    // read from stdin if piped
    let stdin_prompt = if !atty::is(atty::Stream::Stdin) {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        if buf.is_empty() { None } else { Some(buf) }
    } else {
        None
    };

    // combine stdin with -p flag
    let cli_prompt = if cli.prompt.is_empty() {
        None
    } else {
        Some(cli.prompt.join(" "))
    };
    let prompt = match (cli_prompt, stdin_prompt) {
        (Some(p), Some(stdin)) => Some(format!("{p}\n\n{stdin}")),
        (Some(p), None) => Some(p),
        (None, Some(stdin)) => Some(stdin),
        (None, None) => None,
    };

    // handle subcommands first
    match cli.command {
        Some(Command::Login { provider }) => return login_flow(provider).await,
        Some(Command::Logout { provider }) => return logout_flow(provider),
        Some(Command::Models) => return list_models_cmd(),
        Some(Command::Sessions) => return list_sessions_cmd(),
        Some(Command::Status) => return status_cmd(),
        Some(Command::Delete { id }) => return delete_session_cmd(&id),
        Some(Command::Config) => return config_cmd(),
        None => {}
    }

    match prompt {
        Some(prompt) => {
            let prompt = expand_template(&prompt);
            print_mode(cli, prompt).await
        }
        None => tui_mode(cli).await,
    }
}

/// expand /template_name args... into template content
fn expand_template(prompt: &str) -> String {
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
        // not a template, return as-is
        prompt.to_string()
    }
}

async fn print_mode(cli: Cli, prompt: String) -> Result<()> {
    let cfg = config::load_config();
    let debug_cache = cli.debug_cache || cfg.debug_cache.unwrap_or(false);

    // CLI args override config file
    let model_id = cli.model.clone().unwrap_or_else(|| default_model_id(&cfg));

    let model = models::find_model_by_id(&model_id).ok_or_else(|| {
        eyre!(
            "unknown model: {model_id}\n\navailable models:\n{}",
            list_models_short()
        )
    })?;

    let mut registry = ApiRegistry::new();
    providers::register_builtins(&mut registry);

    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.display().to_string();

    let mut tools: Vec<Box<dyn mush_agent::tool::AgentTool>> = if cli.no_tools {
        vec![]
    } else {
        builtin_tools(cwd.clone())
    };

    // connect to MCP servers and add their tools
    if !cli.no_tools && !cfg.mcp.is_empty() {
        let (_mcp_manager, mcp_tools) = mush_mcp::McpManager::connect_all(&cfg.mcp).await;
        tools.extend(mcp_tools);
    }

    let system_prompt = cli
        .system
        .or(cfg.system_prompt.clone())
        .unwrap_or_else(|| build_system_prompt(&cwd));

    // in print mode, hint is always prepended to the message (one-shot, no transform loop)
    let enricher = build_prompt_enricher(&cwd);
    let prompt = if cfg.hint_mode != config::HintMode::None
        && let Some(ref enricher) = enricher
        && let Some(hint) = enricher(&prompt)
    {
        format!("{hint}\n\n{prompt}")
    } else {
        prompt
    };

    // thinking level priority: --thinking flag > saved per-model pref > config default
    let thinking_prefs = config::load_thinking_prefs();
    let thinking_level = if cli.thinking {
        Some(ThinkingLevel::High)
    } else if let Some(&level) = thinking_prefs.get(model.id.0.as_str()) {
        Some(level)
    } else if cfg.thinking.unwrap_or(false) {
        Some(ThinkingLevel::High)
    } else {
        None
    };
    let max_tokens = cli.max_tokens.or(cfg.max_tokens);

    let mut options = StreamOptions {
        thinking: thinking_level,
        max_tokens,
        cache_retention: cfg.cache_retention,
        ..Default::default()
    };

    // api key resolution: env > config > oauth
    if options.api_key.is_none()
        && !provider_env_key_is_set(&model.provider)
        && let Some(key) = config_api_key_for_provider(&cfg, &model.provider)
    {
        options.api_key = Some(key);
    }

    if options.api_key.is_none()
        && let Some(provider_id) = oauth_provider_id_for_model(&model)
        && let Ok(Some(token)) = mush_ai::oauth::get_oauth_token(provider_id).await
    {
        options.api_key = Some(token);
        options.account_id = oauth_account_id(provider_id);
    }

    // session: resume or create new
    let store = SessionStore::new(SessionStore::default_dir());
    let mut session = if let Some(ref id) = cli.resume {
        let sid = mush_session::SessionId(id.clone());
        store
            .load(&sid)
            .map_err(|e| eyre!("failed to load session: {e}"))?
    } else {
        Session::new(model.id.as_str(), &cwd_str)
    };
    options.session_id = Some(session.meta.id.0.clone());

    // add the user message
    let user_msg = Message::User(UserMessage {
        content: UserContent::Text(prompt),
        timestamp_ms: Timestamp::now(),
    });
    session.push_message(user_msg);
    session.auto_title();

    // auto-compact when approaching context limit
    let context_window = model.context_window as usize;
    let compact_model = model.clone();
    let compact_options = options.clone();
    let reg_ref = &registry;
    let transform: Option<mush_agent::ContextTransform<'_>> = Some(Box::new(move |msgs| {
        let m = compact_model.clone();
        let o = compact_options.clone();
        Box::pin(async move { auto_compact(msgs, context_window, reg_ref, &m, &o).await })
    }));

    let config = AgentConfig {
        model: &model,
        system_prompt: Some(system_prompt),
        tools: &tools,
        registry: &registry,
        options,
        max_turns: cli
            .max_turns
            .or(cfg.max_turns)
            .unwrap_or(mush_agent::DEFAULT_MAX_TURNS),
        get_steering: None,
        get_follow_up: None,
        transform_context: transform,
        confirm_tool: None,
    };

    let mut stream = std::pin::pin!(agent_loop(config, session.messages.clone()));
    let mut in_text = false;

    while let Some(event) = stream.next().await {
        match event {
            AgentEvent::StreamEvent { event } => match event {
                StreamEvent::TextDelta { delta, .. } => {
                    if !in_text {
                        in_text = true;
                    }
                    print!("{delta}");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
                StreamEvent::ThinkingDelta { delta, .. } => {
                    print!("\x1b[2m{delta}\x1b[0m");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
                StreamEvent::TextEnd { .. } => {
                    in_text = false;
                }
                StreamEvent::ThinkingEnd { .. } => {
                    println!();
                }
                _ => {}
            },
            AgentEvent::ToolExecStart {
                tool_name, args, ..
            } => {
                if in_text {
                    println!();
                    in_text = false;
                }
                let args_summary = summarise_tool_args(tool_name.as_str(), &args);
                eprintln!("\x1b[36m▶ {tool_name}\x1b[0m {args_summary}");
            }
            AgentEvent::ToolExecEnd {
                tool_name, result, ..
            } => {
                if result.is_error {
                    eprintln!("\x1b[31m✗ {tool_name} failed\x1b[0m");
                } else {
                    eprintln!("\x1b[32m✓ {tool_name}\x1b[0m");
                }
            }
            AgentEvent::MessageEnd { message } => {
                session.push_message(Message::Assistant(message));
            }
            AgentEvent::TurnEnd { message, .. } => {
                if in_text {
                    println!();
                    in_text = false;
                }
                let cost = models::calculate_cost(&model, &message.usage);
                eprintln!(
                    "\n\x1b[2m{} | in:{} out:{} cache:{} | ${:.4}\x1b[0m",
                    message.model,
                    message.usage.input_tokens,
                    message.usage.output_tokens,
                    message.usage.cache_read_tokens,
                    cost.total(),
                );
                if debug_cache && message.usage.cache_read_tokens > 0 {
                    eprintln!(
                        "\x1b[36mcache read detected: {} tokens\x1b[0m",
                        message.usage.cache_read_tokens
                    );
                }
            }
            AgentEvent::ContextTransformed {
                before_count,
                after_count,
            } => {
                eprintln!("\x1b[2m(compacted: {before_count} → {after_count} messages)\x1b[0m");
            }
            AgentEvent::SteeringInjected { count } => {
                eprintln!("\x1b[2m(steering: {count} messages injected)\x1b[0m");
            }
            AgentEvent::FollowUpInjected { count } => {
                eprintln!("\x1b[2m(follow-up: {count} messages queued)\x1b[0m");
            }
            AgentEvent::MaxTurnsReached { max_turns } => {
                eprintln!("\n\x1b[33m⚠ hit max turns limit ({max_turns})\x1b[0m");
            }
            AgentEvent::Error { error } => {
                eprintln!("\x1b[31merror: {}\x1b[0m", format_error(&error));
                // save even on error so the session isn't lost
                if !cli.no_session {
                    store.save(&session).ok();
                    eprintln!("\x1b[2msession: {}\x1b[0m", session.meta.id);
                }
                return Err(eyre!("{error}"));
            }
            _ => {}
        }
    }

    // persist session
    if !cli.no_session {
        store.save(&session)?;
        eprintln!("\x1b[2msession: {}\x1b[0m", session.meta.id);
    }

    Ok(())
}

async fn tui_mode(cli: Cli) -> Result<()> {
    let cfg = config::load_config();
    let debug_cache = cli.debug_cache || cfg.debug_cache.unwrap_or(false);

    let model_id = cli.model.clone().unwrap_or_else(|| default_model_id(&cfg));

    let model = models::find_model_by_id(&model_id).ok_or_else(|| {
        eyre!(
            "unknown model: {model_id}\n\navailable models:\n{}",
            list_models_short()
        )
    })?;

    let mut registry = ApiRegistry::new();
    providers::register_builtins(&mut registry);

    let cwd = std::env::current_dir()?;

    // shared state for streaming bash output to the TUI
    let tool_output_live = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink_state = tool_output_live.clone();
    let output_sink: mush_tools::bash::OutputSink = std::sync::Arc::new(move |line: &str| {
        if let Ok(mut guard) = sink_state.lock() {
            *guard = Some(line.to_string());
        }
    });

    let mut tools: Vec<Box<dyn mush_agent::tool::AgentTool>> = if cli.no_tools {
        vec![]
    } else {
        builtin_tools_with_sink(cwd.clone(), Some(output_sink))
    };

    // connect to MCP servers and add their tools
    if !cli.no_tools && !cfg.mcp.is_empty() {
        let (_mcp_manager, mcp_tools) = mush_mcp::McpManager::connect_all(&cfg.mcp).await;
        tools.extend(mcp_tools);
    }

    let system_prompt = cli
        .system
        .or(cfg.system_prompt.clone())
        .unwrap_or_else(|| build_system_prompt(&cwd));

    // thinking level priority: --thinking flag > saved per-model pref > config default
    let thinking_prefs = config::load_thinking_prefs();
    let thinking_level = if cli.thinking {
        Some(ThinkingLevel::High)
    } else if let Some(&level) = thinking_prefs.get(model.id.0.as_str()) {
        Some(level)
    } else if cfg.thinking.unwrap_or(false) {
        Some(ThinkingLevel::High)
    } else {
        None
    };

    let mut options = StreamOptions {
        thinking: thinking_level,
        max_tokens: cli.max_tokens.or(cfg.max_tokens),
        cache_retention: cfg.cache_retention,
        ..Default::default()
    };

    if options.api_key.is_none()
        && !provider_env_key_is_set(&model.provider)
        && let Some(key) = config_api_key_for_provider(&cfg, &model.provider)
    {
        options.api_key = Some(key);
    }

    if options.api_key.is_none()
        && let Some(provider_id) = oauth_provider_id_for_model(&model)
        && let Ok(Some(token)) = mush_ai::oauth::get_oauth_token(provider_id).await
    {
        options.api_key = Some(token);
        options.account_id = oauth_account_id(provider_id);
    }

    // create or resume a session
    let cwd_str = cwd.display().to_string();
    let session = if let Some(ref resume_id) = cli.resume {
        let store = SessionStore::new(SessionStore::default_dir());
        let sessions = store
            .list()
            .map_err(|e| eyre!("failed to list sessions: {e}"))?;
        let matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.id.0.starts_with(resume_id.as_str()))
            .collect();
        match matches.len() {
            0 => return Err(eyre!("no session matching '{resume_id}'")),
            1 => store
                .load(&matches[0].id)
                .map_err(|e| eyre!("failed to load session: {e}"))?,
            n => {
                return Err(eyre!(
                    "'{resume_id}' matches {n} sessions, be more specific"
                ));
            }
        }
    } else {
        Session::new(model.id.as_str(), &cwd_str)
    };
    let initial_messages = session.messages.clone();
    let session_id = session.meta.id.clone();
    options.session_id = Some(session_id.0.clone());

    let theme = mush_tui::Theme::from_config(&cfg.theme);
    let prompt_enricher = build_prompt_enricher(&cwd);

    let hint_mode = match cfg.hint_mode {
        config::HintMode::Message => mush_tui::HintMode::Message,
        config::HintMode::Transform => mush_tui::HintMode::Transform,
        config::HintMode::None => mush_tui::HintMode::None,
    };

    let config_file = config::config_dir().join("config.toml");
    let mut provider_api_keys = std::collections::HashMap::new();
    if let Some(key) = cfg.api_keys.anthropic.clone() {
        provider_api_keys.insert("anthropic".into(), key);
    }
    if let Some(key) = cfg.api_keys.openrouter.clone() {
        provider_api_keys.insert("openrouter".into(), key);
    }
    if let Some(key) = cfg.api_keys.openai.clone() {
        provider_api_keys.insert("openai".into(), key);
    }

    let tui_config = TuiConfig {
        model,
        system_prompt: Some(system_prompt),
        options,
        max_turns: cli
            .max_turns
            .or(cfg.max_turns)
            .unwrap_or(mush_agent::DEFAULT_MAX_TURNS),
        initial_messages,
        theme,
        prompt_enricher,
        hint_mode,
        config_path: if config_file.exists() {
            Some(config_file)
        } else {
            None
        },
        provider_api_keys,
        thinking_prefs,
        save_thinking_prefs: Some(std::sync::Arc::new(|prefs| {
            config::save_thinking_prefs(prefs);
        })),
        save_session: if cli.no_session {
            None
        } else {
            let sid = session_id;
            let cwd_s = cwd_str.clone();
            Some(std::sync::Arc::new(move |msgs, tree, model_id| {
                let store = SessionStore::new(SessionStore::default_dir());
                let mut session = Session::new(model_id, &cwd_s);
                session.meta.id = sid.clone();
                session.messages = msgs.to_vec();
                session.tree = tree.clone();
                session.meta.message_count = msgs.len();
                session.auto_title();
                store.save(&session).ok();
            }))
        },
        confirm_tools: cfg.confirm_tools.unwrap_or(false),
        debug_cache,
        tool_output_live: Some(tool_output_live),
    };

    mush_tui::run_tui(tui_config, &tools, &registry)
        .await
        .map_err(|e| eyre!("TUI error: {e}"))?;

    Ok(())
}

/// compact messages using LLM summarisation when approaching context limit
async fn auto_compact(
    messages: Vec<Message>,
    context_window: usize,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
) -> Vec<Message> {
    use mush_session::compact;

    if !compact::needs_compaction(&messages, context_window) {
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

/// enrich error messages with actionable suggestions
fn format_error(error: &str) -> String {
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

fn build_system_prompt(cwd: &std::path::Path) -> String {
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

    // append AGENTS.md context
    let context = loader::discover_project_context(cwd);
    for agents in &context.agents_md {
        prompt.push_str("\n\n# Project Context\n\n");
        prompt.push_str(&agents.content);
    }

    // load all skill content into system prompt (stable prefix for prompt caching).
    // loading everything avoids KV-cache invalidation from dynamic injection.
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

/// build a prompt enricher that returns a relevance hint for the user message.
/// all skill content is already in the system prompt (stable for caching).
/// the enricher just nudges the model toward the most relevant skills.
#[cfg(feature = "embeddings")]
fn build_prompt_enricher(cwd: &std::path::Path) -> Option<mush_tui::PromptEnricher> {
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
fn build_prompt_enricher(_cwd: &std::path::Path) -> Option<mush_tui::PromptEnricher> {
    None
}

fn config_api_key_for_provider(cfg: &config::Config, provider: &Provider) -> Option<String> {
    match provider {
        Provider::Anthropic => cfg.api_keys.anthropic.clone(),
        Provider::OpenRouter => cfg.api_keys.openrouter.clone(),
        Provider::Custom(name) if name == "openai" => cfg.api_keys.openai.clone(),
        _ => None,
    }
}

fn provider_env_key_is_set(provider: &Provider) -> bool {
    mush_ai::env::env_api_key(provider).is_some()
}

fn oauth_provider_id_for_model(model: &Model) -> Option<&'static str> {
    match &model.provider {
        Provider::Anthropic => Some("anthropic"),
        Provider::Custom(name) if name == "openai-codex" => Some("openai-codex"),
        _ => None,
    }
}

fn oauth_account_id(provider_id: &str) -> Option<String> {
    mush_ai::oauth::load_credentials().ok().and_then(|store| {
        store
            .providers
            .get(provider_id)
            .and_then(|c| c.account_id.clone())
    })
}

fn default_model_id(cfg: &config::Config) -> String {
    if let Some(model) = &cfg.model {
        return model.clone();
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
        return "gpt-5.3-codex".into();
    }

    "claude-opus-4-6".into()
}

fn list_models_short() -> String {
    models::all_models_with_user()
        .iter()
        .map(|m| format!("  {} ({})", m.id, m.provider))
        .collect::<Vec<_>>()
        .join("\n")
}

fn list_models_cmd() -> Result<()> {
    for m in models::all_models_with_user() {
        let cost = format!("${:.2}/${:.2} per 1M tokens", m.cost.input, m.cost.output);
        println!("  \x1b[1m{}\x1b[0m ({})", m.id, m.provider);
        println!(
            "    context: {}k, max output: {}, {cost}",
            m.context_window / 1000,
            m.max_output_tokens
        );
    }
    Ok(())
}

fn list_sessions_cmd() -> Result<()> {
    let store = SessionStore::new(SessionStore::default_dir());
    let sessions = store
        .list()
        .map_err(|e| eyre!("failed to list sessions: {e}"))?;

    if sessions.is_empty() {
        println!("no saved sessions");
        return Ok(());
    }

    for meta in &sessions {
        let title = meta.title.as_deref().unwrap_or("(untitled)");
        let age = format_age(meta.updated_at.as_ms());
        println!(
            "  \x1b[2m{}\x1b[0m  {} \x1b[2m({}, {} msgs, {})\x1b[0m",
            &meta.id.0[..8],
            title,
            meta.model_id,
            meta.message_count,
            age,
        );
    }
    println!("\nresume with: mush -c <id>");
    Ok(())
}

fn format_age(timestamp_ms: u64) -> String {
    let now = Timestamp::now().as_ms();
    let elapsed = now.saturating_sub(timestamp_ms);
    let secs = elapsed / 1000;
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn delete_session_cmd(id: &str) -> Result<()> {
    let store = SessionStore::new(SessionStore::default_dir());
    let sessions = store
        .list()
        .map_err(|e| eyre!("failed to list sessions: {e}"))?;

    // find by prefix match
    let matches: Vec<_> = sessions.iter().filter(|s| s.id.0.starts_with(id)).collect();

    match matches.len() {
        0 => Err(eyre!("no session matching '{id}'")),
        1 => {
            let session = matches[0];
            let title = session.title.as_deref().unwrap_or("(untitled)");
            store
                .delete(&session.id)
                .map_err(|e| eyre!("failed to delete: {e}"))?;
            eprintln!("\x1b[32m✓ deleted session: {title}\x1b[0m");
            Ok(())
        }
        n => {
            eprintln!("'{id}' matches {n} sessions:");
            for s in matches {
                let title = s.title.as_deref().unwrap_or("(untitled)");
                eprintln!("  {} - {title}", &s.id.0[..8]);
            }
            Err(eyre!("ambiguous prefix, be more specific"))
        }
    }
}

fn config_cmd() -> Result<()> {
    let path = config::config_path();

    // create default config if missing
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            "# mush configuration\n\
             # model = \"gpt-5.3-codex\"  # optional, auto-default depends on auth\n\
             # thinking = false\n\
             # max_tokens = 16384\n\
             # max_turns = 30\n\
             # cache_retention = \"short\"  # none | short | long\n\
             # debug_cache = false\n\
             # system_prompt = \"\"\n\
             \n\
             # [api_keys]\n\
             # anthropic = \"sk-...\"\n\
             # openrouter = \"sk-or-...\"\n\
             # openai = \"sk-proj-...\"\n",
        )?;
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&path).status()?;

    if !status.success() {
        return Err(eyre!("{editor} exited with {status}"));
    }

    Ok(())
}

fn status_cmd() -> Result<()> {
    let cfg = config::load_config();

    println!("\x1b[1mmush status\x1b[0m\n");

    // config
    let config_path = config::config_dir().join("config.toml");
    if config_path.exists() {
        println!("  config: {}", config_path.display());
    } else {
        println!("  config: \x1b[2m(none, using defaults)\x1b[0m");
    }

    // model
    let model_id = default_model_id(&cfg);
    println!("  model:  {model_id}");

    // thinking
    let thinking = cfg.thinking.unwrap_or(false);
    println!("  thinking: {thinking}");

    // max turns
    let max_turns = cfg.max_turns.unwrap_or(mush_agent::DEFAULT_MAX_TURNS);
    println!("  max turns: {max_turns}");

    // cache retention
    let cache_retention = cfg.cache_retention.unwrap_or(CacheRetention::Short);
    let cache_retention = match cache_retention {
        CacheRetention::None => "none",
        CacheRetention::Short => "short",
        CacheRetention::Long => "long",
    };
    println!("  cache retention: {cache_retention}");
    println!("  debug cache: {}", cfg.debug_cache.unwrap_or(false));

    #[cfg(feature = "embeddings")]
    println!("  embeddings: enabled");
    #[cfg(not(feature = "embeddings"))]
    println!("  embeddings: disabled");

    // auth status
    println!("\n\x1b[1mauth\x1b[0m\n");

    // env keys
    let has_anthropic_env = std::env::var("ANTHROPIC_API_KEY").is_ok();
    let has_openrouter_env = std::env::var("OPENROUTER_API_KEY").is_ok();
    let has_openai_env = std::env::var("OPENAI_API_KEY").is_ok();
    let has_anthropic_cfg = cfg.api_keys.anthropic.is_some();
    let has_openrouter_cfg = cfg.api_keys.openrouter.is_some();
    let has_openai_cfg = cfg.api_keys.openai.is_some();

    // oauth
    let oauth_store = mush_ai::oauth::load_credentials().unwrap_or_default();
    let has_anthropic_oauth = oauth_store.providers.contains_key("anthropic");
    let has_openai_codex_oauth = oauth_store.providers.contains_key("openai-codex");

    print!("  anthropic:   ");
    if has_anthropic_env {
        println!("\x1b[32m✓ env var\x1b[0m");
    } else if has_anthropic_cfg {
        println!("\x1b[32m✓ config file\x1b[0m");
    } else if has_anthropic_oauth {
        println!("\x1b[32m✓ oauth\x1b[0m");
    } else {
        println!("\x1b[31m✗ not configured\x1b[0m");
    }

    print!("  openrouter:  ");
    if has_openrouter_env {
        println!("\x1b[32m✓ env var\x1b[0m");
    } else if has_openrouter_cfg {
        println!("\x1b[32m✓ config file\x1b[0m");
    } else {
        println!("\x1b[2m- not configured\x1b[0m");
    }

    print!("  openai:      ");
    if has_openai_env {
        println!("\x1b[32m✓ env var\x1b[0m");
    } else if has_openai_cfg {
        println!("\x1b[32m✓ config file\x1b[0m");
    } else {
        println!("\x1b[2m- not configured\x1b[0m");
    }

    print!("  openai-codex:");
    if has_openai_codex_oauth {
        println!("\x1b[32m ✓ oauth\x1b[0m");
    } else {
        println!("\x1b[2m - not configured\x1b[0m");
    }

    // sessions
    let store = SessionStore::new(SessionStore::default_dir());
    let session_count = store.list().map(|s| s.len()).unwrap_or(0);
    println!("\n\x1b[1msessions\x1b[0m\n");
    println!("  {session_count} saved");

    Ok(())
}

async fn login_flow(provider_id: Option<String>) -> Result<()> {
    use mush_ai::oauth;

    let provider_id = match provider_id {
        Some(id) => id,
        None => {
            let providers = oauth::list_providers();
            if providers.len() == 1 {
                providers[0].0.to_string()
            } else {
                eprintln!("available providers:");
                for (id, name) in &providers {
                    eprintln!("  {id} - {name}");
                }
                return Err(eyre!("specify a provider: mush login <provider>"));
            }
        }
    };

    let provider = oauth::get_provider(&provider_id).ok_or_else(|| {
        let available = oauth::list_providers()
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eyre!("unknown provider: {provider_id}\navailable: {available}")
    })?;

    eprintln!("logging in to {}...\n", provider.name());

    let (prompt, pkce) = provider.begin_login();
    eprintln!("open this URL in your browser:\n");
    eprintln!("  {}\n", prompt.url);
    eprintln!("{}", prompt.instructions);

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();
    if input.is_empty() {
        return Err(eyre!("no code provided"));
    }

    let creds = provider
        .exchange_code(input, &pkce)
        .await
        .map_err(|e| eyre!("login failed: {e}"))?;

    let mut store =
        oauth::load_credentials().map_err(|e| eyre!("failed to load credentials: {e}"))?;
    store.providers.insert(provider_id, creds);
    oauth::save_credentials(&store).map_err(|e| eyre!("failed to save credentials: {e}"))?;

    eprintln!("\n\x1b[32m✓ logged in to {}\x1b[0m", provider.name());
    Ok(())
}

fn logout_flow(provider_id: Option<String>) -> Result<()> {
    use mush_ai::oauth;

    let provider_id = provider_id.unwrap_or_else(|| "anthropic".into());

    let mut store =
        oauth::load_credentials().map_err(|e| eyre!("failed to load credentials: {e}"))?;

    if store.providers.remove(&provider_id).is_some() {
        oauth::save_credentials(&store).map_err(|e| eyre!("failed to save credentials: {e}"))?;
        eprintln!("\x1b[32m✓ logged out from {provider_id}\x1b[0m");
    } else {
        eprintln!("not logged in to {provider_id}");
    }

    Ok(())
}
