//! mush cli - fast little robot harness 🍄

mod commands;
mod config;
mod logging;
mod setup;
mod timing;

use clap::Parser;
use color_eyre::eyre::{Result, eyre};
use futures::StreamExt;

use mush_agent::{AgentConfig, AgentEvent, agent_loop, summarise_tool_args};
use mush_ai::models;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_session::{PaneSession, Session, SessionStore};

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

    /// print startup timing breakdown
    #[arg(long, env = "MUSH_PROFILE_STARTUP")]
    profile_startup: bool,

    /// enable chrome tracing timeline (requires profiling build)
    #[arg(long, env = "MUSH_TRACE")]
    trace: bool,

    #[arg(long, hide = true, env = mush_tui::KEYBOARD_ENHANCEMENT_ENV)]
    tui_keyboard_enhancement: Option<mush_tui::KeyboardEnhancementMode>,

    #[arg(long, hide = true, env = mush_tui::MOUSE_TRACKING_ENV)]
    tui_mouse_tracking: Option<mush_tui::MouseTrackingMode>,

    #[arg(long, hide = true, env = mush_tui::IMAGE_PROBE_ENV)]
    tui_image_probe: Option<mush_tui::ImageProbeMode>,
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

impl Cli {
    fn terminal_policy_overrides(&self) -> mush_tui::TerminalPolicyOverrides {
        mush_tui::TerminalPolicyOverrides {
            keyboard_enhancement: self.tui_keyboard_enhancement,
            mouse_tracking: self.tui_mouse_tracking,
            image_probe: self.tui_image_probe,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cfg = config::load_config();
    let cli = Cli::parse();
    let (_log_guards, log_buffer) = logging::init_logging(cfg.log_filter.as_deref(), cli.trace);
    tracing::info!("mush starting");

    // clean up old tool output files
    mush_agent::truncation::cleanup();

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
        Some(Command::Status) => return commands::status(cli.terminal_policy_overrides()),
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
    let timer = if cli.profile_startup {
        Some(timing::PhaseTimer::new())
    } else {
        None
    };
    let (mut setup, startup_report) = AppSetup::init(SetupArgs {
        model: cli.model.clone(),
        thinking: cli.thinking,
        max_tokens: cli.max_tokens,
        max_turns: cli.max_turns,
        system: cli.system,
        no_tools: cli.no_tools,
        debug_cache: cli.debug_cache,
        output_sink: None,
        timer,
    })
    .await?;

    if let Some(report) = startup_report {
        eprintln!("{report}");
    }

    // in print mode, hint is always prepended (one-shot, no transform loop)
    let enricher =
        build_prompt_enricher(&setup.cwd, &setup.cfg.retrieval, &setup.tool_descriptions);
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
    let (compact_model, compact_options) = setup
        .compaction_model
        .clone()
        .unwrap_or_else(|| (setup.model.clone(), setup.options.clone()));
    let context_window = compact_model.context_window.max(setup.model.context_window);
    let reg_ref = setup.registry.clone();
    let context_tokens_shared = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ctx_tokens_for_transform = context_tokens_shared.clone();
    let lifecycle_hooks = std::sync::Arc::new(setup.lifecycle_hooks.clone());
    let cwd = std::sync::Arc::new(setup.cwd.clone());
    let transform: Option<mush_agent::TransformFn> = if setup.cfg.auto_compact {
        Some(Box::new(move |msgs| {
            let m = compact_model.clone();
            let o = compact_options.clone();
            let ctx = ctx_tokens_for_transform.clone();
            let input_len = msgs.len();
            let owned = msgs.to_vec();
            let hooks = lifecycle_hooks.clone();
            let cwd = cwd.clone();
            let reg = reg_ref.clone();
            Box::pin(async move {
                let tokens = TokenCount::new(ctx.load(std::sync::atomic::Ordering::Relaxed));
                let compacted = auto_compact(
                    owned,
                    tokens,
                    context_window,
                    &reg,
                    &m,
                    &o,
                    Some(&hooks),
                    Some(cwd.as_path()),
                )
                .await;
                if compacted.len() < input_len {
                    mush_agent::ContextTransformResult::Updated(compacted)
                } else {
                    mush_agent::ContextTransformResult::Unchanged
                }
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
        hooks: Box::new(mush_agent::ClosureHooks {
            transform,
            ..mush_agent::ClosureHooks::default()
        }),
        injections: mush_agent::AgentInjections {
            lifecycle_hooks: setup.lifecycle_hooks,
            cwd: Some(std::env::current_dir().unwrap_or_default()),
            ..mush_agent::AgentInjections::default()
        },
        cancel: None,
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
    let terminal_policy_overrides = cli.terminal_policy_overrides();
    let tool_output_live = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink_state = tool_output_live.clone();
    let output_sink: mush_tools::bash::OutputSink = std::sync::Arc::new(move |line: &str| {
        if let Ok(mut guard) = sink_state.lock() {
            *guard = Some(line.to_string());
        }
    });

    let timer = if cli.profile_startup {
        Some(timing::PhaseTimer::new())
    } else {
        None
    };
    let (mut setup, startup_report) = AppSetup::init(SetupArgs {
        model: cli.model.clone(),
        thinking: cli.thinking,
        max_tokens: cli.max_tokens,
        max_turns: cli.max_turns,
        system: cli.system,
        no_tools: cli.no_tools,
        debug_cache: cli.debug_cache,
        output_sink: Some(output_sink),
        timer,
    })
    .await?;

    if let Some(report) = startup_report {
        eprintln!("{report}");
    }

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
    let initial_panes: Vec<mush_tui::PaneSnapshot> = session
        .panes
        .iter()
        .map(|p| mush_tui::PaneSnapshot {
            pane_id: p.pane_id,
            label: p.label.clone(),
            model_id: p.model_id.to_string(),
            conversation: p.conversation.clone(),
        })
        .collect();
    let session_id = session.meta.id.clone();
    setup.options.session_id = Some(session_id.clone());

    let theme = mush_tui::Theme::from_config(&setup.cfg.theme);
    let prompt_enricher =
        build_prompt_enricher(&setup.cwd, &setup.cfg.retrieval, &setup.tool_descriptions);

    let hint_mode = setup.cfg.hint_mode;
    let terminal_policy = setup.cfg.terminal.with_overrides(terminal_policy_overrides);

    // build agent card before setup fields are moved into tui_config
    let agent_card = {
        let mut card = mush_agent::AgentCard::build(&setup.model.id, &setup.tools);
        card.capabilities = mush_agent::Capabilities {
            streaming: true,
            multi_pane: true,
            messaging: true,
            lsp: setup.lsp_diagnostics.is_some(),
        };
        card
    };

    let config_file = config::config_dir().join("config.toml");
    let provider_api_keys = setup.cfg.api_keys.to_map();

    let tui_config = TuiConfig {
        model: setup.model,
        system_prompt: Some(setup.system_prompt),
        options: setup.options,
        max_turns: setup.max_turns,
        initial_messages,
        initial_panes,
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
            let cwd_s = cwd_str.clone();
            Some(std::sync::Arc::new(
                move |snapshot: mush_tui::SessionSnapshot| {
                    let store = SessionStore::new(SessionStore::default_dir());
                    let mut session = Session::new(&snapshot.model_id, &cwd_s);
                    session.meta.id = snapshot.session_id;
                    session.conversation = snapshot.primary;
                    session.meta.message_count = session.conversation.context_len();
                    session.panes = snapshot
                        .panes
                        .into_iter()
                        .map(|p| PaneSession {
                            pane_id: p.pane_id,
                            label: p.label,
                            model_id: p.model_id.into(),
                            conversation: p.conversation,
                        })
                        .collect();
                    session.auto_title();
                    let _ = store.save(&session);
                },
            ))
        },
        confirm_tools: setup.cfg.confirm_tools,
        auto_compact: setup.cfg.auto_compact,
        auto_fork_compact: setup.cfg.auto_fork_compact,
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
        terminal_policy,
        lifecycle_hooks: setup.lifecycle_hooks,
        cwd: std::env::current_dir().unwrap_or_default(),
        dynamic_system_context: None,
        file_rules: setup.file_rules,
        lsp_diagnostics: setup.lsp_diagnostics,
        agent_card: Some(agent_card),
        model_tiers: setup.cfg.model_tiers,
        compaction_model: setup.compaction_model,
        http_client: Some(setup.http_client),
        session_id: session_id.clone(),
        settings: mush_tui::settings::ScopedSettings {
            scope: setup.cfg.settings.scope,
            anthropic_betas: setup.cfg.settings.anthropic_betas.clone(),
        },
    };

    mush_tui::run_tui(tui_config, &setup.tools, &setup.registry)
        .await
        .map_err(|e| eyre!("TUI error: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::*;

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                // serialised by ENV_LOCK in each test
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn with_env(vars: &[(&'static str, Option<&str>)]) -> EnvGuard {
        let saved = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect::<Vec<_>>();
        for (key, value) in vars {
            // serialised by ENV_LOCK in each test
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        EnvGuard { saved }
    }

    #[test]
    fn clap_env_overrides_feed_terminal_policy() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = with_env(&[
            (mush_tui::KEYBOARD_ENHANCEMENT_ENV, Some("disabled")),
            (mush_tui::MOUSE_TRACKING_ENV, Some("off")),
            (mush_tui::IMAGE_PROBE_ENV, Some("false")),
        ]);

        let cli = Cli::try_parse_from(["mush"]).unwrap();
        let overrides = cli.terminal_policy_overrides();

        assert_eq!(
            overrides.keyboard_enhancement,
            Some(mush_tui::KeyboardEnhancementMode::Disabled)
        );
        assert_eq!(
            overrides.mouse_tracking,
            Some(mush_tui::MouseTrackingMode::Disabled)
        );
        assert_eq!(
            overrides.image_probe,
            Some(mush_tui::ImageProbeMode::Disabled)
        );
    }
}
