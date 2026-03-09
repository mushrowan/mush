//! mush cli - fast little robot harness 🍄

mod commands;
mod config;
mod logging;
mod setup;

use clap::Parser;
use color_eyre::eyre::{Result, eyre};
use futures::StreamExt;

use mush_agent::{AgentConfig, AgentEvent, agent_loop, summarise_tool_args};
use mush_ai::models;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_session::{Session, SessionStore};

use mush_tui::TuiConfig;

use setup::{
    AppSetup, SetupArgs, auto_compact, build_prompt_enricher, expand_template, format_error,
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
    let cfg = config::load_config();
    let (_log_guard, log_buffer) = logging::init_logging(cfg.log_filter.as_deref());
    tracing::info!("mush starting");

    // clean up old tool output files
    mush_agent::truncation::cleanup();

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
        None => tui_mode(cli, log_buffer).await,
    }
}

async fn print_mode(cli: Cli, prompt: String) -> Result<()> {
    let mut setup = AppSetup::init(SetupArgs {
        model: cli.model.clone(),
        thinking: cli.thinking,
        max_tokens: cli.max_tokens,
        max_turns: cli.max_turns,
        system: cli.system,
        no_tools: cli.no_tools,
        debug_cache: cli.debug_cache,
        output_sink: None,
    })
    .await?;

    // in print mode, hint is always prepended (one-shot, no transform loop)
    let enricher = build_prompt_enricher(&setup.cwd);
    let prompt = if setup.cfg.hint_mode != config::HintMode::None
        && let Some(ref enricher) = enricher
        && let Some(hint) = enricher(&prompt)
    {
        format!("{hint}\n\n{prompt}")
    } else {
        prompt
    };

    // session: resume or create new
    let cwd_str = setup.cwd.display().to_string();
    let store = SessionStore::new(SessionStore::default_dir());
    let mut session = if let Some(ref id) = cli.resume {
        let sid = mush_session::SessionId::from(id.clone());
        store
            .load(&sid)
            .map_err(|e| eyre!("failed to load session: {e}"))?
    } else {
        Session::new(setup.model.id.as_str(), &cwd_str)
    };
    setup.options.session_id = Some(session.meta.id.clone());

    // add the user message
    let user_msg = Message::User(UserMessage {
        content: UserContent::Text(prompt),
        timestamp_ms: Timestamp::now(),
    });
    session.push_message(user_msg);
    session.auto_title();

    // auto-compact when approaching context limit
    let context_window = setup.model.context_window;
    let compact_model = setup.model.clone();
    let compact_options = setup.options.clone();
    let reg_ref = &setup.registry;
    let context_tokens_shared = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ctx_tokens_for_transform = context_tokens_shared.clone();
    let transform: Option<mush_agent::ContextTransform<'_>> = if setup.cfg.auto_compact {
        Some(Box::new(move |msgs| {
            let m = compact_model.clone();
            let o = compact_options.clone();
            let ctx = ctx_tokens_for_transform.clone();
            Box::pin(async move {
                let tokens = TokenCount::new(ctx.load(std::sync::atomic::Ordering::Relaxed));
                auto_compact(msgs, tokens, context_window, reg_ref, &m, &o).await
            })
        }))
    } else {
        None
    };

    let config = AgentConfig {
        model: setup.model.clone(),
        system_prompt: Some(setup.system_prompt),
        tools: setup.tools.clone(),
        registry: &setup.registry,
        options: setup.options,
        max_turns: setup.max_turns,
        get_steering: None,
        get_follow_up: None,
        transform_context: transform,
        confirm_tool: None,
    };

    let mut stream = std::pin::pin!(agent_loop(config, session.context()));
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
                    let _ = std::io::stdout().flush();
                }
                StreamEvent::ThinkingDelta { delta, .. } => {
                    print!("\x1b[2m{delta}\x1b[0m");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
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
                if result.outcome.is_error() {
                    eprintln!("\x1b[31m✗ {tool_name} failed\x1b[0m");
                } else {
                    eprintln!("\x1b[32m✓ {tool_name}\x1b[0m");
                }
            }
            AgentEvent::MessageEnd { message } => {
                context_tokens_shared.store(
                    message.usage.total_input_tokens().get(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                session.push_message(Message::Assistant(message));
            }
            AgentEvent::TurnEnd { message, .. } => {
                if in_text {
                    println!();
                    in_text = false;
                }
                let cost = models::calculate_cost(&setup.model, &message.usage);
                eprintln!(
                    "\n\x1b[2m{} | in:{} out:{} cache:{} | {}\x1b[0m",
                    message.model,
                    message.usage.input_tokens,
                    message.usage.output_tokens,
                    message.usage.cache_read_tokens,
                    cost.total(),
                );
                if setup.debug_cache && message.usage.cache_read_tokens > TokenCount::ZERO {
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
                    let _ = store.save(&session);
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

async fn tui_mode(cli: Cli, log_buffer: logging::LogBuffer) -> Result<()> {
    // shared state for streaming bash output to the TUI
    let tool_output_live = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink_state = tool_output_live.clone();
    let output_sink: mush_tools::bash::OutputSink = std::sync::Arc::new(move |line: &str| {
        if let Ok(mut guard) = sink_state.lock() {
            *guard = Some(line.to_string());
        }
    });

    let mut setup = AppSetup::init(SetupArgs {
        model: cli.model.clone(),
        thinking: cli.thinking,
        max_tokens: cli.max_tokens,
        max_turns: cli.max_turns,
        system: cli.system,
        no_tools: cli.no_tools,
        debug_cache: cli.debug_cache,
        output_sink: Some(output_sink),
    })
    .await?;

    // create or resume a session
    let cwd_str = setup.cwd.display().to_string();
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
        Session::new(setup.model.id.as_str(), &cwd_str)
    };
    let initial_messages = session.context();
    let session_id = session.meta.id.clone();
    setup.options.session_id = Some(session_id.clone());

    let theme = mush_tui::Theme::from_config(&setup.cfg.theme);
    let prompt_enricher = build_prompt_enricher(&setup.cwd);

    let hint_mode = setup.cfg.hint_mode;

    let config_file = config::config_dir().join("config.toml");
    let provider_api_keys = setup.cfg.api_keys.to_map();

    let tui_config = TuiConfig {
        model: setup.model,
        system_prompt: Some(setup.system_prompt),
        options: setup.options,
        max_turns: setup.max_turns,
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
        thinking_prefs: setup.thinking_prefs,
        save_thinking_prefs: Some(std::sync::Arc::new(|prefs| {
            config::save_thinking_prefs(prefs);
        })),
        save_last_model: Some(std::sync::Arc::new(|model_id| {
            config::save_last_model(model_id);
        })),
        save_session: if cli.no_session {
            None
        } else {
            let sid = session_id.clone();
            let cwd_s = cwd_str.clone();
            Some(std::sync::Arc::new(move |conversation, model_id| {
                let store = SessionStore::new(SessionStore::default_dir());
                let mut session = Session::new(model_id, &cwd_s);
                session.meta.id = sid.clone();
                session.conversation = conversation.clone();
                session.meta.message_count = session.context().len();
                session.auto_title();
                let _ = store.save(&session);
            }))
        },
        confirm_tools: setup.cfg.confirm_tools,
        auto_compact: setup.cfg.auto_compact,
        show_cost: setup.cfg.show_cost,
        debug_cache: setup.debug_cache,
        cache_timer: setup.cfg.cache_timer,
        thinking_display: setup.cfg.thinking_display,
        tool_output_live: Some(tool_output_live),
        log_buffer: Some({
            let buf = log_buffer.clone();
            std::sync::Arc::new(move |n| buf.tail(n))
        }),
        update_title: if cli.no_session {
            None
        } else {
            let sid = session_id.clone();
            Some(std::sync::Arc::new(move |title: String| {
                let store = SessionStore::new(SessionStore::default_dir());
                if let Ok(mut session) = store.load(&sid) {
                    session.meta.title = Some(title);
                    let _ = store.save(&session);
                }
            }))
        },
        isolation_mode: setup.cfg.isolation,
    };

    mush_tui::run_tui(tui_config, &setup.tools, &setup.registry)
        .await
        .map_err(|e| eyre!("TUI error: {e}"))?;

    Ok(())
}
