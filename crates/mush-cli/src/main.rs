use clap::Parser;
mod headless;
use color_eyre::eyre::{Result, eyre};
use futures::StreamExt;

use mush_agent::{AgentConfig, AgentEvent, agent_loop, summarise_tool_args};
use mush_ai::models;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_session::{PaneSession, Session, SessionStore};

use mush_cli::output::Style;
use mush_cli::{commands, config, daemon, logging, setup, timing};
use mush_tui::TuiConfig;

use setup::{
    AppSetup, SetupArgs, auto_compact, build_system_prompt_from_context, expand_template,
    format_error,
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

    #[arg(short, long, alias = "print", num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,

    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "fork"])]
    continue_recent: bool,

    #[arg(long, value_name = "ID", conflicts_with = "fork")]
    resume: Option<Option<String>>,

    #[arg(long, value_name = "ID")]
    fork: Option<String>,

    #[arg(short, long)]
    model: Option<String>,

    #[arg(long)]
    thinking: bool,

    #[arg(long)]
    max_tokens: Option<u64>,

    #[arg(long)]
    max_turns: Option<usize>,

    #[arg(long)]
    debug_cache: bool,

    #[arg(long)]
    system: Option<String>,

    #[arg(long)]
    no_tools: bool,

    #[arg(short = 'o', long = "option", value_name = "KEY=VALUE")]
    option: Vec<String>,

    #[arg(long = "tools", short = 't', value_delimiter = ',')]
    tools: Option<Vec<String>>,

    #[arg(long = "append-system-prompt")]
    append_system: Option<String>,

    #[arg(long = "skill")]
    skill: Vec<std::path::PathBuf>,

    #[arg(long)]
    no_skills: bool,

    #[arg(long)]
    no_context_files: bool,

    #[arg(long)]
    no_session: bool,

    #[arg(long)]
    status_file: Option<std::path::PathBuf>,

    #[arg(long, env = "MUSH_PROFILE_STARTUP")]
    profile_startup: bool,

    #[arg(long, env = "MUSH_TRACE")]
    trace: bool,

    #[arg(long, hide = true, env = mush_tui::KEYBOARD_ENHANCEMENT_ENV)]
    tui_keyboard_enhancement: Option<mush_tui::KeyboardEnhancementMode>,

    #[arg(long, hide = true, env = mush_tui::IMAGE_PROBE_ENV)]
    tui_image_probe: Option<mush_tui::ImageProbeMode>,
}

enum SessionTarget {
    New,
    Resume(String),
    Fork(String),
    Pick,
}

impl Cli {
    fn session_target(&self) -> Result<SessionTarget> {
        if self.continue_recent {
            let cwd = std::env::current_dir()?.display().to_string();
            let store = SessionStore::new(SessionStore::default_dir());
            return store
                .list()?
                .into_iter()
                .find(|meta| meta.cwd == cwd)
                .map(|meta| SessionTarget::Resume(meta.id.to_string()))
                .ok_or_else(|| eyre!("no session for this directory yet, run mush to start one"));
        }
        Ok(match (&self.resume, &self.fork) {
            (Some(Some(id)), _) => SessionTarget::Resume(id.clone()),
            (Some(None), _) => SessionTarget::Pick,
            (None, Some(id)) => SessionTarget::Fork(id.clone()),
            (None, None) => SessionTarget::New,
        })
    }

    fn resumes_a_session(&self) -> bool {
        self.continue_recent || self.resume.is_some()
    }

    fn worker_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let mut flag = |name: &str, on: bool| {
            if on {
                args.push(name.to_string());
            }
        };
        flag("--thinking", self.thinking);
        flag("--debug-cache", self.debug_cache);
        flag("--no-tools", self.no_tools);
        flag("--no-skills", self.no_skills);
        flag("--no-context-files", self.no_context_files);
        let mut value = |name: &str, value: Option<String>| {
            if let Some(value) = value {
                args.push(name.to_string());
                args.push(value);
            }
        };
        value("--model", self.model.clone());
        value("--max-tokens", self.max_tokens.map(|n| n.to_string()));
        value("--max-turns", self.max_turns.map(|n| n.to_string()));
        value("--system", self.system.clone());
        value("--append-system-prompt", self.append_system.clone());
        value("--tools", self.tools.as_ref().map(|tools| tools.join(",")));
        for option in &self.option {
            value("--option", Some(option.clone()));
        }
        for skill in &self.skill {
            value("--skill", Some(skill.display().to_string()));
        }
        args
    }

    fn discovery(&self) -> mush_ext::DiscoveryOptions {
        mush_ext::DiscoveryOptions {
            skill_paths: self.skill.clone(),
            no_skills: self.no_skills,
            no_context_files: self.no_context_files,
        }
    }
}

#[derive(clap::Subcommand)]
enum Command {
    Control(mush_cli::control::ControlArgs),
    Worker {
        #[arg(long, value_name = "SOCKET")]
        socket: std::path::PathBuf,
        #[arg(long, value_name = "FILE")]
        descriptor: Option<std::path::PathBuf>,
    },
    Daemon {
        #[arg(long, value_name = "SOCKET")]
        socket: Option<std::path::PathBuf>,
    },
    Ps,
    Stop {
        id: String,
    },
    Shutdown,
    Login {
        provider: Option<String>,
    },
    Logout {
        provider: Option<String>,
    },
    Models,
    Sessions,
    Status,
    Delete {
        id: String,
    },
    Config,
}

impl Cli {
    fn terminal_policy_overrides(&self) -> mush_tui::TerminalPolicyOverrides {
        mush_tui::TerminalPolicyOverrides {
            keyboard_enhancement: self.tui_keyboard_enhancement,
            image_probe: self.tui_image_probe,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::config::HookBuilder::default()
        .theme(if mush_cli::output::stderr_colour() {
            color_eyre::config::Theme::dark()
        } else {
            color_eyre::config::Theme::new()
        })
        .install()?;
    let cli = Cli::parse();
    if let Some(Command::Control(args)) = cli.command {
        return mush_cli::control::run(args).await;
    }
    let cfg = config::load_config_with(&cli.option)?;
    let (_log_guards, log_buffer) = logging::init_logging(cfg.log_filter.as_deref(), cli.trace);
    tracing::info!("mush starting");

    mush_agent::truncation::cleanup();

    if let Some(Command::Worker { socket, descriptor }) = &cli.command {
        let socket = socket.clone();
        let descriptor = descriptor.clone();
        return worker_mode(cli, socket, descriptor).await;
    }
    if let Some(Command::Daemon { socket }) = &cli.command {
        let socket = socket
            .clone()
            .unwrap_or_else(mush_daemon::daemon_socket_path);
        return daemon_mode(&socket).await;
    }
    match cli.command {
        Some(Command::Control(_) | Command::Worker { .. } | Command::Daemon { .. }) => {
            unreachable!()
        }
        Some(Command::Ps) => return daemon::ps().await,
        Some(Command::Stop { id }) => return daemon::stop(&id).await,
        Some(Command::Shutdown) => return daemon::shutdown().await,
        Some(Command::Login { provider }) => return commands::login(provider).await,
        Some(Command::Logout { provider }) => return commands::logout(provider),
        Some(Command::Models) => return commands::list_models(),
        Some(Command::Sessions) => return commands::list_sessions(),
        Some(Command::Status) => return commands::status(cli.terminal_policy_overrides()),
        Some(Command::Delete { id }) => return commands::delete_session(&id),
        Some(Command::Config) => return commands::open_config(),
        None => {}
    }

    let stdin_prompt = if cli.prompt.is_empty()
        && !cli.resumes_a_session()
        && cli.status_file.is_none()
        && !atty::is(atty::Stream::Stdin)
    {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        if buf.is_empty() { None } else { Some(buf) }
    } else {
        None
    };

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

    match prompt {
        Some(prompt) => {
            let prompt = expand_template(&prompt);
            print_mode(cli, prompt).await
        }
        None if cli.status_file.is_some() => print_mode(cli, String::new()).await,
        None => tui_mode(cli, log_buffer).await,
    }
}

async fn print_mode(cli: Cli, prompt: String) -> Result<()> {
    let mut status = headless::Status {
        path: cli.status_file.clone(),
        session_id: None,
        state: "starting",
        model: cli.model.clone(),
        error_classification: None,
    };
    status.write()?;
    let result = print_mode_inner(cli, prompt, &mut status).await;
    if let Err(error) = &result {
        if status.state != "cancelled" {
            status.state = "error";
            status.error_classification = Some(headless::classify_error(&error.to_string()));
        }
    } else if status.state == "running" {
        status.state = "completed";
    }
    status.write()?;
    result
}

async fn print_mode_inner(cli: Cli, prompt: String, status: &mut headless::Status) -> Result<()> {
    let store = SessionStore::new(SessionStore::default_dir());
    let cwd = mush_tui::canonical_dir(&std::env::current_dir()?);
    let mut _session_lease = None;
    let mut session = match cli.session_target()? {
        SessionTarget::Resume(id) => {
            let meta = store.resolve(&id)?;
            _session_lease = store.lease(&meta.id)?;
            store.load(&meta.id)?
        }
        SessionTarget::Fork(id) => {
            let meta = store.resolve(&id)?;
            fork_session(store.load(&meta.id)?, &cwd.display().to_string())
        }
        SessionTarget::Pick => return Err(eyre!("--resume without an id needs the tui")),
        SessionTarget::New => {
            let model_id = match cli.model.clone() {
                Some(model) => model,
                None => setup::default_model_id(&config::load_config_with(&cli.option)?, &cwd),
            };
            Session::new(&model_id, &cwd.display().to_string())
        }
    };
    if let Some(model) = &cli.model {
        session.meta.model_id = model.clone().into();
    }
    status.model = Some(session.meta.model_id.to_string());
    if !prompt.is_empty() {
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text(prompt))
                .timestamp_ms(Timestamp::now())
                .prompt_id(PromptId::new())
                .build(),
        ));
        session.auto_title();
    }
    status.session_id = Some(session.meta.id.clone());
    if !cli.no_session {
        store.save(&session)?;
    }
    status.write()?;
    let mut shutdown = std::pin::pin!(headless::shutdown_signal());
    let timer = if cli.profile_startup {
        Some(timing::PhaseTimer::new())
    } else {
        None
    };
    let init = AppSetup::init(SetupArgs {
        model: Some(session.meta.model_id.to_string()),
        thinking: cli.thinking,
        max_tokens: cli.max_tokens,
        max_turns: cli.max_turns,
        system: cli.system.clone(),
        no_tools: cli.no_tools,
        config_overrides: cli.option.clone(),
        tools: cli.tools.clone(),
        append_system: cli.append_system.clone(),
        discovery: cli.discovery(),
        debug_cache: cli.debug_cache,
        output_sink: None,
        kernel_notice_sink: None,
        kernel_busy_prompt: None,
        timer,
    });
    let (mut setup, startup_report) = tokio::select! {
        biased;
        result = &mut shutdown => {
            result?;
            status.state = "cancelled";
            return Err(eyre!("cancelled"));
        }
        result = init => result?,
    };

    if let Some(report) = startup_report {
        eprintln!("{report}");
    }

    setup.options.session_id = Some(session.meta.id.clone());
    session.meta.model_id = setup.model.id.clone();
    status.model = Some(setup.model.id.to_string());
    status.state = "running";
    if !cli.no_session {
        store.save(&session)?;
    }
    status.write()?;

    let control = mush_agent::ipc::control::Control::new(session.meta.id.to_string());
    let listener = mush_agent::IpcListener::start_control(
        mush_agent::ipc::control_socket_path(&control.target().instance),
        std::sync::Arc::new(mush_agent::AgentCard::build(&setup.model.id, &setup.tools)),
        control.clone(),
    )?;
    let control_endpoint = Some(mush_agent::exec_context::ControlEndpoint {
        socket: listener.path().to_owned(),
        target: control.target(),
    });

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
            let owned = msgs.to_vec();
            let hooks = lifecycle_hooks.clone();
            let cwd = cwd.clone();
            let reg = reg_ref.clone();
            Box::pin(async move {
                let tokens = TokenCount::new(ctx.load(std::sync::atomic::Ordering::Relaxed));
                let result = auto_compact(
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
                if result.summarised_count > 0 {
                    ctx.store(
                        mush_session::compact::estimate_tokens(&result.messages) as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    mush_agent::ContextTransformResult::Compacted(mush_agent::ContextCompaction {
                        messages: result.messages,
                        summary: result.summary,
                        summarised_count: result.summarised_count,
                    })
                } else if result.masked_count > 0 {
                    mush_agent::ContextTransformResult::Silent(result.messages)
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
            follow_up: Some(mush_cli::control::follow_up(control.clone())),
            ..mush_agent::ClosureHooks::default()
        }),
        injections: mush_agent::AgentInjections {
            control_endpoint,
            lifecycle_hooks: setup.lifecycle_hooks,
            cwd: Some(std::env::current_dir().unwrap_or_default()),
            ..mush_agent::AgentInjections::default()
        },
        cancel: Some(Default::default()),
    };

    let cancel = config
        .cancel
        .clone()
        .ok_or_else(|| eyre!("missing cancellation token"))?;
    let mut stream = std::pin::pin!(agent_loop(config, session.context()));
    let mut in_text = false;
    let mut transcript = headless::Transcript::default();
    let mut cancelled = false;
    let mut cancel_deadline = None;
    let mut ended = false;

    loop {
        let event = tokio::select! {
            biased;
            result = &mut shutdown, if !cancelled => {
                result?;
                cancel.cancel();
                cancelled = true;
                status.state = "cancelled";
                if !cli.no_session { store.save(&session)?; }
                status.write()?;
                cancel_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(2));
                continue;
            }
            _ = async { if let Some(deadline) = cancel_deadline { tokio::time::sleep_until(deadline).await } else { std::future::pending::<()>().await } } => break,
            event = stream.next() => event,
        };
        let Some(event) = event else { break };
        match &event {
            AgentEvent::AgentEnd => control.finish(mush_agent::ipc::control::Outcome::Completed),
            AgentEvent::Error { .. } | AgentEvent::MaxTurnsReached { .. } => {
                control.running_failed(true)
            }
            _ => {}
        }
        if transcript.apply(&mut session, &event) && !cli.no_session {
            store.save(&session)?;
        }
        match event {
            AgentEvent::AgentEnd => ended = true,
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
                    print!("{}", Style::Dim.stdout(delta));
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
                eprintln!(
                    "{} {args_summary}",
                    Style::Cyan.stderr(format_args!("▶ {tool_name}"))
                );
            }
            AgentEvent::ToolExecEnd {
                tool_name, result, ..
            } => {
                if result.outcome.is_error() {
                    eprintln!(
                        "{}",
                        Style::Red.stderr(format_args!("✗ {tool_name} failed"))
                    );
                } else {
                    eprintln!("{}", Style::Green.stderr(format_args!("✓ {tool_name}")));
                }
            }
            AgentEvent::MessageEnd { message } => {
                context_tokens_shared.store(
                    message.usage.total_input_tokens().get(),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            AgentEvent::TurnEnd { message, .. } => {
                if in_text {
                    println!();
                    in_text = false;
                }
                let cost = models::calculate_cost(&setup.model, &message.usage);
                eprintln!(
                    "\n{}",
                    Style::Dim.stderr(format_args!(
                        "{} | in:{} out:{} cache:{} | {}",
                        message.model,
                        message.usage.input_tokens,
                        message.usage.output_tokens,
                        message.usage.cache_read_tokens,
                        cost.total(),
                    )),
                );
                if setup.debug_cache && message.usage.cache_read_tokens > TokenCount::ZERO {
                    eprintln!(
                        "{}",
                        Style::Cyan.stderr(format_args!(
                            "cache read detected: {} tokens",
                            message.usage.cache_read_tokens
                        ))
                    );
                }
            }
            AgentEvent::ContextTransformed {
                before_count,
                after_count,
            } => {
                eprintln!(
                    "{}",
                    Style::Dim.stderr(format_args!(
                        "(compacted: {before_count} → {after_count} messages)"
                    ))
                );
            }
            AgentEvent::ContextCompacted {
                before_count,
                compaction,
            } => {
                session.conversation.apply_compaction(
                    compaction.summary,
                    compaction.summarised_count,
                    compaction.messages,
                );
                let after_count = session.conversation.context_len();
                session.meta.message_count = after_count;
                if !cli.no_session {
                    store.save(&session)?;
                }
                eprintln!(
                    "{}",
                    Style::Dim.stderr(format_args!(
                        "(compacted: {before_count} → {after_count} messages)"
                    ))
                );
            }
            AgentEvent::SteeringInjected { messages } => {
                eprintln!(
                    "{}",
                    Style::Dim.stderr(format_args!(
                        "(steering: {} messages injected)",
                        messages.len()
                    ))
                );
            }
            AgentEvent::FollowUpInjected { messages } => {
                eprintln!(
                    "{}",
                    Style::Dim.stderr(format_args!(
                        "(follow-up: {} messages queued)",
                        messages.len()
                    ))
                );
            }
            AgentEvent::MaxTurnsReached { max_turns } => {
                if !cancelled {
                    status.state = "turn_limit";
                }
                eprintln!(
                    "\n{}",
                    Style::Yellow.stderr(format_args!("⚠ hit max turns limit ({max_turns})"))
                );
            }
            AgentEvent::Error { error } => {
                eprintln!(
                    "{}",
                    Style::Red.stderr(format_args!("error: {}", format_error(&error)))
                );
                if !cli.no_session {
                    store.save(&session)?;
                    eprintln!(
                        "{}",
                        Style::Dim.stderr(format_args!("session: {}", session.meta.id))
                    );
                }
                return Err(eyre!("{error}"));
            }
            _ => {}
        }
    }

    if !cli.no_session {
        store.save(&session)?;
        eprintln!(
            "{}",
            Style::Dim.stderr(format_args!("session: {}", session.meta.id))
        );
    }

    listener.shutdown().await?;
    if cancelled {
        return Err(eyre!("cancelled"));
    }
    if !ended || !transcript.is_complete() {
        return Err(eyre!("agent ended with an incomplete transcript"));
    }
    Ok(())
}

struct Prepared {
    setup: AppSetup,
    kernel_signals: mush_runtime::KernelSignals,
}

struct OpenedSession {
    session: Session,
    _lease: Option<std::fs::File>,
    saver: Option<SaverThread>,
}

async fn daemon_mode(socket: &std::path::Path) -> Result<()> {
    let program = std::env::current_exe()?;
    let supervisor = mush_daemon::Supervisor::new(
        program,
        mush_daemon::workers_dir(),
        mush_session::data_dir().join("workers"),
    );
    eprintln!(
        "{}",
        Style::Dim.stderr(format_args!("daemon listening on {}", socket.display()))
    );
    mush_daemon::supervisor::run(socket, supervisor).await?;
    Ok(())
}

async fn worker_mode(
    cli: Cli,
    socket: std::path::PathBuf,
    descriptor: Option<std::path::PathBuf>,
) -> Result<()> {
    let listener = mush_daemon::bind(&socket)?;
    let descriptor = descriptor.map(|path| {
        mush_daemon::Descriptor::new(
            path,
            socket.clone(),
            std::env::current_dir().unwrap_or_default(),
        )
    });
    let Prepared {
        setup,
        kernel_signals,
    } = prepare(&cli).await?;
    let opened = open_session(&cli.session_target()?, &setup, cli.no_session).await?;
    let worker = worker_config(&cli, &setup, &opened, kernel_signals);
    eprintln!(
        "{}",
        Style::Dim.stderr(format_args!("worker listening on {}", socket.display()))
    );
    let mush_runtime::HostedWorker {
        connection,
        ipc_listener,
    } = mush_runtime::host_worker(worker, &setup.tools, &setup.registry)?;
    let shutdown = tokio_util::sync::CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let _ = headless::shutdown_signal().await;
            shutdown.cancel();
        }
    });
    let served = mush_daemon::serve(connection, listener, descriptor, shutdown).await;
    let _ = std::fs::remove_file(&socket);
    served?;
    if let Some(listener) = ipc_listener {
        listener.shutdown().await?;
    }
    if let Some(saver) = opened.saver {
        saver.finish();
    }
    Ok(())
}

async fn tui_mode(cli: Cli, log_buffer: logging::LogBuffer) -> Result<()> {
    let Prepared {
        setup,
        kernel_signals,
    } = prepare(&cli).await?;

    let (discovery_tx, discovery_rx) = std::sync::mpsc::channel();
    let startup_discovery_client = setup.http_client.clone();
    tokio::spawn(async move {
        let summary = mush_ai::discovery::refresh_and_save(startup_discovery_client).await;
        tracing::info!(
            target: "mush::discovery",
            models = summary.total_models(),
            "{}",
            summary.one_line()
        );
        let _ = discovery_tx.send(mush_tui::runner::DiscoveryUpdate::Refreshed);
    });

    let target = cli.session_target()?;
    match cli.no_session {
        false => {
            let open = match &target {
                SessionTarget::Resume(id) => mush_daemon::OpenTarget::Resume(id.clone()),
                SessionTarget::Fork(id) => mush_daemon::OpenTarget::Fork(id.clone()),
                SessionTarget::New | SessionTarget::Pick => mush_daemon::OpenTarget::New,
            };
            let descriptor = daemon::open(open, cli.worker_args()).await?;
            let connection = mush_daemon::DaemonConnection::connect(&descriptor.socket).await?;
            let mut tui_config = client_config(&cli, &setup, &log_buffer, None);
            tui_config.open_session_picker = matches!(target, SessionTarget::Pick);
            if connection.worker_build() != mush_daemon::build_id() {
                tui_config.startup_notice = Some(format!(
                    "this worker is from another build of mush. mush stop {} and start again to pick up the new one",
                    &descriptor.session_id[..8.min(descriptor.session_id.len())]
                ));
            }
            mush_tui::run_tui_attached(
                tui_config,
                Box::new(connection),
                &setup.registry,
                Some(discovery_rx),
            )
            .await
            .map_err(|e| eyre!("TUI error: {e}"))?;
            eprintln!(
                "{}",
                Style::Dim.stderr(format_args!(
                    "detached, mush --resume {} reattaches",
                    &descriptor.session_id[..8.min(descriptor.session_id.len())]
                ))
            );
        }
        true => {
            let opened = open_session(&target, &setup, cli.no_session).await?;
            let worker = worker_config(&cli, &setup, &opened, kernel_signals);
            let mut tui_config = client_config(&cli, &setup, &log_buffer, Some(&opened.session));
            tui_config.open_session_picker = matches!(target, SessionTarget::Pick);
            mush_tui::run_tui(
                tui_config,
                worker,
                &setup.tools,
                &setup.registry,
                Some(discovery_rx),
            )
            .await
            .map_err(|e| eyre!("TUI error: {e}"))?;
            if let Some(saver) = opened.saver {
                saver.finish();
            }
        }
    }
    Ok(())
}

async fn prepare(cli: &Cli) -> Result<Prepared> {
    let (sinks, kernel_signals) = mush_runtime::kernel_channels();
    let timer = if cli.profile_startup {
        Some(timing::PhaseTimer::new())
    } else {
        None
    };
    let (setup, startup_report) = AppSetup::init(SetupArgs {
        model: cli.model.clone(),
        thinking: cli.thinking,
        max_tokens: cli.max_tokens,
        max_turns: cli.max_turns,
        system: cli.system.clone(),
        no_tools: cli.no_tools,
        config_overrides: cli.option.clone(),
        tools: cli.tools.clone(),
        append_system: cli.append_system.clone(),
        discovery: cli.discovery(),
        debug_cache: cli.debug_cache,
        output_sink: Some(sinks.output),
        kernel_notice_sink: Some(sinks.notices),
        kernel_busy_prompt: Some(sinks.busy_prompt),
        timer,
    })
    .await?;

    if let Some(report) = startup_report {
        eprintln!("{report}");
    }

    Ok(Prepared {
        setup,
        kernel_signals,
    })
}

async fn open_session(
    target: &SessionTarget,
    setup: &AppSetup,
    no_session: bool,
) -> Result<OpenedSession> {
    let cwd_str = setup.cwd.display().to_string();
    let mut lease = None;
    let session = match target {
        SessionTarget::Resume(id) => {
            let store = SessionStore::new(SessionStore::default_dir());
            let meta = store.resolve(id)?;
            lease = store.lease(&meta.id)?;
            if let Some(kernel) = &setup.kernel_state {
                kernel
                    .restore_from(&SessionStore::kernel_state_path(&meta.id))
                    .await;
            }
            store.load(&meta.id)?
        }
        SessionTarget::Fork(id) => {
            let store = SessionStore::new(SessionStore::default_dir());
            let meta = store.resolve(id)?;
            fork_session(store.load(&meta.id)?, &cwd_str)
        }
        SessionTarget::New | SessionTarget::Pick => Session::new(setup.model.id.as_str(), &cwd_str),
    };
    let saver = (!no_session)
        .then(|| SaverThread::spawn(cwd_str))
        .transpose()?;
    Ok(OpenedSession {
        session,
        _lease: lease,
        saver,
    })
}

fn agent_card(setup: &AppSetup) -> mush_agent::AgentCard {
    let mut card = mush_agent::AgentCard::build(&setup.model.id, &setup.tools);
    card.capabilities = mush_agent::Capabilities {
        streaming: true,
        multi_pane: true,
        messaging: true,
        lsp: setup.lsp_diagnostics.is_some(),
    };
    card
}

fn worker_config(
    cli: &Cli,
    setup: &AppSetup,
    opened: &OpenedSession,
    kernel_signals: mush_runtime::KernelSignals,
) -> mush_runtime::WorkerConfig {
    let session = &opened.session;
    let mut options = setup.options.clone();
    options.session_id = Some(session.meta.id.clone());
    let compaction_model = setup.compaction_model.clone().map(|(model, mut opts)| {
        opts.session_id = Some(session.meta.id.clone());
        (model, opts)
    });
    let mut worker = mush_runtime::WorkerConfig::new(mush_runtime::TurnConfig {
        default_model: setup.model.clone(),
        options,
        max_turns: setup.max_turns,
        provider_api_keys: setup.cfg.api_keys.to_map(),
        confirm_tools: setup.cfg.confirm_tools,
        auto_compact: setup.cfg.auto_compact,
        auto_fork_compact: setup.cfg.auto_fork_compact,
        compaction_model,
        forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars::with_classes(
            setup.cfg.forbidden_chars.iter().copied(),
            setup.cfg.forbidden_char_classes.iter().copied(),
        ),
        tool_allowlist: if setup.rlm {
            Some(vec!["ipython".into()])
        } else {
            cli.tools.clone()
        },
        generate_title: !cli.no_session,
    });
    worker.conversation = session.conversation.clone();
    worker.session_id = session.meta.id.clone();
    worker.title = session.meta.title.clone();
    worker.system_prompt = Some(
        session
            .system_prompt
            .clone()
            .unwrap_or_else(|| setup.system_prompt.clone()),
    );
    worker.thinking_level = setup
        .options
        .thinking
        .clone()
        .unwrap_or(ThinkingLevel::Off)
        .normalize_visible();
    worker.rlm_depth = env_depth("RLM_DEPTH").unwrap_or(0);
    worker.rlm_max_depth = setup
        .cfg
        .rlm_max_depth
        .or_else(|| env_depth("RLM_MAX_DEPTH"))
        .unwrap_or(mush_tui::DEFAULT_RLM_MAX_DEPTH);
    worker.kernel_state = setup.kernel_state.clone();
    worker.saver = opened.saver.as_ref().map(|saver| saver.handle());
    worker.injections = mush_agent::AgentInjections {
        lifecycle_hooks: setup.lifecycle_hooks.clone(),
        cwd: Some(std::env::current_dir().unwrap_or_default()),
        dynamic_system_context: None,
        file_rules: setup.file_rules.clone(),
        lsp_diagnostics: setup.lsp_diagnostics.clone(),
        control_endpoint: None,
    };
    worker.agent_card = Some(agent_card(setup));
    worker.kernel_signals = Some(kernel_signals);
    worker
}

fn client_config(
    cli: &Cli,
    setup: &AppSetup,
    log_buffer: &logging::LogBuffer,
    session: Option<&Session>,
) -> TuiConfig {
    let terminal_policy_overrides = cli.terminal_policy_overrides();
    let session_id = session
        .map(|session| session.meta.id.clone())
        .unwrap_or_default();
    let mut options = setup.options.clone();
    options.session_id = Some(session_id.clone());
    let system_prompt = session
        .and_then(|session| session.system_prompt.clone())
        .unwrap_or_else(|| setup.system_prompt.clone());

    let theme = mush_tui::Theme::from_config(&setup.cfg.theme);
    let terminal_policy = setup.cfg.terminal.with_overrides(terminal_policy_overrides);
    let config_file = config::config_dir().join("config.toml");
    let (favourite_models, favourites_locked) =
        config::resolve_favourites(&setup.cfg.favourite_models, config::load_favourite_models());

    TuiConfig {
        model: setup.model.clone(),
        system_prompt: Some(system_prompt),
        options,
        initial_title: session.and_then(|session| session.meta.title.clone()),
        theme,
        config_path: if config_file.exists() {
            Some(config_file)
        } else {
            None
        },
        provider_api_keys: setup.cfg.api_keys.to_map(),
        thinking_prefs: setup.thinking_prefs.clone(),
        save_thinking_prefs: Some(std::sync::Arc::new(|prefs| {
            config::save_thinking_prefs(prefs);
        })),
        last_models: config::load_last_models(),
        save_last_models: Some(std::sync::Arc::new(|models| {
            config::save_last_models(models);
        })),
        show_cost: setup.cfg.show_cost,
        show_usage_lines: setup.cfg.show_usage_lines,
        show_token_counters: setup.cfg.show_token_counters,
        status_bar_config: setup.cfg.status_bar.clone(),
        debug_cache: setup.debug_cache,
        cache_timer: setup.cfg.cache_timer,
        thinking_display: setup.cfg.thinking_display,
        log_buffer: Some({
            let buf = log_buffer.clone();
            std::sync::Arc::new(move |n| buf.tail(n))
        }),
        terminal_policy,
        cwd: std::env::current_dir().unwrap_or_default(),
        agent_card: Some(agent_card(setup)),
        model_tiers: setup.cfg.model_tiers.clone(),
        compaction_model: setup.compaction_model.clone(),
        resolve_model: Some(std::sync::Arc::new(mush_ai::discovery::resolve_model_by_id)),
        http_client: Some(setup.http_client.clone()),
        session_id,
        settings: mush_tui::settings::ScopedSettings {
            scope: setup.cfg.settings.scope,
            anthropic_betas: setup.cfg.settings.anthropic_betas.clone(),
        },
        favourite_models,
        favourites_locked,
        save_favourite_models: if favourites_locked {
            None
        } else {
            Some(std::sync::Arc::new(|favs: &[String]| {
                config::save_favourite_models(favs);
            }))
        },
        recent_models: config::load_recent_models(),
        save_recent_models: Some(std::sync::Arc::new(|recent: &[String]| {
            config::save_recent_models(recent);
        })),
        keymap: {
            let mut km = mush_tui::KeyMap::default();
            let warnings = km.override_scoped(&setup.cfg.keys.scoped());
            for warning in warnings {
                tracing::warn!("keybind config: {warning}");
            }
            km
        },
        open_session_picker: false,
        startup_notice: None,
        reload_context: Some(std::sync::Arc::new({
            let discovery = cli.discovery();
            let rlm = setup.rlm;
            let kernel_packages = setup.kernel_packages.clone();
            move |cwd: &std::path::Path| -> mush_tui::ReloadedContext {
                let project = mush_ext::loader::discover_project_context_with(cwd, &discovery);
                let system_prompt = if rlm {
                    setup::build_rlm_system_prompt(&project, cwd, &kernel_packages)
                } else {
                    build_system_prompt_from_context(&project, cwd)
                };
                let templates = mush_ext::discover_templates(cwd);
                mush_tui::ReloadedContext {
                    system_prompt,
                    templates,
                    skills: mush_tui::SkillCatalogue {
                        skills: project.skills,
                        diagnostics: project.skill_diagnostics,
                    },
                }
            }
        })),
        mcp: setup.mcp.clone(),
        skills: setup.skills.clone(),
    }
}

struct SaverThread {
    tx: std::sync::mpsc::Sender<mush_tui::SessionSnapshot>,
    thread: std::thread::JoinHandle<()>,
}

impl SaverThread {
    fn spawn(cwd: String) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<mush_tui::SessionSnapshot>();
        let thread = std::thread::Builder::new()
            .name("session-saver".into())
            .spawn(move || {
                let store = SessionStore::new(SessionStore::default_dir());
                while let Ok(mut snapshot) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        snapshot = newer;
                    }
                    let _ = store.save(&session_from_snapshot(snapshot, &cwd));
                }
            })?;
        Ok(Self { tx, thread })
    }

    fn handle(&self) -> mush_tui::runner::SessionSaver {
        let tx = self.tx.clone();
        std::sync::Arc::new(move |snapshot| {
            let _ = tx.send(snapshot);
        })
    }

    fn finish(self) {
        drop(self.tx);
        let _ = self.thread.join();
    }
}

fn fork_session(source: Session, cwd: &str) -> Session {
    let mut forked = Session::new(source.meta.model_id.as_str(), cwd);
    forked.conversation = source.conversation;
    forked.system_prompt = source.system_prompt;
    forked.panes = source.panes;
    forked.meta.message_count = forked.conversation.context_len();
    forked
}

fn session_from_snapshot(snapshot: mush_tui::SessionSnapshot, cwd: &str) -> Session {
    let mut session = Session::new(&snapshot.model_id, cwd);
    session.meta.id = snapshot.session_id;
    session.meta.title = snapshot.title;
    session.conversation = snapshot.primary;
    session.system_prompt = snapshot.system_prompt;
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
    session
}

fn env_depth(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::*;

    #[test]
    fn resume_takes_an_optional_id_and_continue_takes_none() {
        let bare = Cli::try_parse_from(["mush", "--resume"]).unwrap();
        assert!(matches!(bare.resume, Some(None)));
        let targeted = Cli::try_parse_from(["mush", "--resume", "abc123"]).unwrap();
        assert!(matches!(targeted.resume, Some(Some(id)) if id == "abc123"));
        let with_prompt = Cli::try_parse_from(["mush", "--resume", "-p", "hello"]).unwrap();
        assert!(matches!(with_prompt.resume, Some(None)));
        assert_eq!(with_prompt.prompt, ["hello"]);
        assert!(Cli::try_parse_from(["mush", "-c"]).unwrap().continue_recent);
        assert!(Cli::try_parse_from(["mush", "-c", "abc123"]).is_err());
        assert!(Cli::try_parse_from(["mush", "-c", "--resume", "abc"]).is_err());
        assert!(Cli::try_parse_from(["mush", "--resume", "a", "--fork", "b"]).is_err());
    }

    fn snapshot(title: Option<&str>) -> mush_tui::SessionSnapshot {
        let conversation = mush_session::ConversationState::from_messages(vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("first question".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )]);
        mush_tui::SessionSnapshot {
            session_id: mush_ai::types::SessionId::new(),
            title: title.map(str::to_string),
            primary: conversation,
            model_id: "test".into(),
            panes: Vec::new(),
            system_prompt: None,
        }
    }

    #[test]
    fn a_fork_gets_a_fresh_id_and_keeps_the_conversation() {
        let source = session_from_snapshot(snapshot(Some("port prime")), "/tmp");
        let source_id = source.meta.id.clone();

        let forked = fork_session(source, "/elsewhere");

        assert_ne!(forked.meta.id, source_id);
        assert!(forked.meta.title.is_none());
        assert_eq!(forked.meta.cwd, "/elsewhere");
        assert_eq!(forked.conversation.context_len(), 1);
    }

    #[test]
    fn a_saved_session_keeps_the_generated_title() {
        let session = session_from_snapshot(snapshot(Some("port prime")), "/tmp");

        assert_eq!(session.meta.title.as_deref(), Some("port prime"));
    }

    #[test]
    fn an_untitled_session_falls_back_to_the_first_question() {
        let session = session_from_snapshot(snapshot(None), "/tmp");

        assert_eq!(session.meta.title.as_deref(), Some("first question"));
    }

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
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
            (mush_tui::IMAGE_PROBE_ENV, Some("false")),
        ]);

        let cli = Cli::try_parse_from(["mush"]).unwrap();
        let overrides = cli.terminal_policy_overrides();

        assert_eq!(
            overrides.keyboard_enhancement,
            Some(mush_tui::KeyboardEnhancementMode::Disabled)
        );
        assert_eq!(
            overrides.image_probe,
            Some(mush_tui::ImageProbeMode::Disabled)
        );
    }
}
