//! mush cli - fast little robot harness 🍄

mod commands;
mod config;
mod setup;

use clap::Parser;
use color_eyre::eyre::{Result, eyre};
use futures::StreamExt;

use mush_agent::{AgentConfig, AgentEvent, agent_loop, summarise_tool_args};
use mush_ai::models;
use mush_ai::providers;
use mush_ai::registry::ApiRegistry;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_session::{Session, SessionStore};
use mush_tools::{builtin_tools, builtin_tools_with_sink};
use mush_tui::TuiConfig;

use setup::{
    auto_compact, build_prompt_enricher, build_system_prompt, expand_template, format_error,
    list_models_short, resolve_api_key, resolve_thinking,
};

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
        Some(Command::Login { provider }) => return commands::login(provider).await,
        Some(Command::Logout { provider }) => return commands::logout(provider),
        Some(Command::Models) => return commands::list_models(),
        Some(Command::Sessions) => return commands::list_sessions(),
        Some(Command::Status) => return commands::status(),
        Some(Command::Delete { id }) => return commands::delete_session(&id),
        Some(Command::Config) => return commands::open_config(),
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

async fn print_mode(cli: Cli, prompt: String) -> Result<()> {
    let cfg = config::load_config();
    let debug_cache = cli.debug_cache || cfg.debug_cache.unwrap_or(false);

    let model_id = cli
        .model
        .clone()
        .unwrap_or_else(|| setup::default_model_id(&cfg));

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

    if !cli.no_tools && !cfg.mcp.is_empty() {
        let (_mcp_manager, mcp_tools) = mush_mcp::McpManager::connect_all(&cfg.mcp).await;
        tools.extend(mcp_tools);
    }

    let system_prompt = cli
        .system
        .or(cfg.system_prompt.clone())
        .unwrap_or_else(|| build_system_prompt(&cwd));

    // in print mode, hint is always prepended (one-shot, no transform loop)
    let enricher = build_prompt_enricher(&cwd);
    let prompt = if cfg.hint_mode != config::HintMode::None
        && let Some(ref enricher) = enricher
        && let Some(hint) = enricher(&prompt)
    {
        format!("{hint}\n\n{prompt}")
    } else {
        prompt
    };

    let thinking_prefs = config::load_thinking_prefs();
    let thinking_level = resolve_thinking(cli.thinking, &model, &thinking_prefs, &cfg);

    let mut options = StreamOptions {
        thinking: thinking_level,
        max_tokens: cli.max_tokens.or(cfg.max_tokens),
        cache_retention: cfg.cache_retention,
        ..Default::default()
    };

    resolve_api_key(&mut options, &model, &cfg).await;

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
    options.session_id = Some(session.meta.id.to_string());

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
                if !cli.no_session {
                    store.save(&session).ok();
                    eprintln!("\x1b[2msession: {}\x1b[0m", session.meta.id);
                }
                return Err(eyre!("{error}"));
            }
            _ => {}
        }
    }

    if !cli.no_session {
        store.save(&session)?;
        eprintln!("\x1b[2msession: {}\x1b[0m", session.meta.id);
    }

    Ok(())
}

async fn tui_mode(cli: Cli) -> Result<()> {
    let cfg = config::load_config();
    let debug_cache = cli.debug_cache || cfg.debug_cache.unwrap_or(false);

    let model_id = cli
        .model
        .clone()
        .unwrap_or_else(|| setup::default_model_id(&cfg));

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

    if !cli.no_tools && !cfg.mcp.is_empty() {
        let (_mcp_manager, mcp_tools) = mush_mcp::McpManager::connect_all(&cfg.mcp).await;
        tools.extend(mcp_tools);
    }

    let system_prompt = cli
        .system
        .or(cfg.system_prompt.clone())
        .unwrap_or_else(|| build_system_prompt(&cwd));

    let thinking_prefs = config::load_thinking_prefs();
    let thinking_level = resolve_thinking(cli.thinking, &model, &thinking_prefs, &cfg);

    let mut options = StreamOptions {
        thinking: thinking_level,
        max_tokens: cli.max_tokens.or(cfg.max_tokens),
        cache_retention: cfg.cache_retention,
        ..Default::default()
    };

    resolve_api_key(&mut options, &model, &cfg).await;

    // create or resume a session
    let cwd_str = cwd.display().to_string();
    let session = if let Some(ref resume_id) = cli.resume {
        let store = SessionStore::new(SessionStore::default_dir());
        let sessions = store
            .list()
            .map_err(|e| eyre!("failed to list sessions: {e}"))?;
        let matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.id.starts_with(resume_id.as_str()))
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
    options.session_id = Some(session_id.to_string());

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
