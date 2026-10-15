use clap::Parser;
mod headless;
mod json_events;
use color_eyre::eyre::{Result, eyre};
use futures::StreamExt;

use mush_agent::{AgentConfig, AgentEvent, agent_loop, summarise_tool_args};
use mush_ai::models;
use mush_ai::types::*;
use mush_session::{PaneSession, Session, SessionStore};

use mush_cli::output::Style;
use mush_cli::{agents, commands, config, daemon, doctor, logging, schedule, setup, timing};
use mush_tui::TuiConfig;

use setup::{
    AppSetup, SetupArgs, auto_compact, build_system_prompt_from_context, expand_template,
    format_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputMode {
    Text,
    Json,
}

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

    #[arg(long, value_enum, default_value_t = OutputMode::Text, requires = "prompt")]
    mode: OutputMode,

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

    #[arg(long, value_name = "OBJECTIVE")]
    goal: Option<String>,

    #[arg(long, value_name = "TOKENS", requires = "goal")]
    goal_budget: Option<u64>,

    #[arg(long)]
    autonomous: bool,

    #[arg(long = "autonomous-gate", value_name = "CMD")]
    autonomous_gate: Vec<String>,

    #[arg(long = "autonomous-max-continuations", value_name = "N")]
    autonomous_max_continuations: Option<String>,

    #[arg(long = "autonomous-max-turns", value_name = "N")]
    autonomous_max_turns: Option<String>,

    #[arg(long = "autonomous-max-tokens", value_name = "N")]
    autonomous_max_tokens: Option<String>,

    #[arg(long = "autonomous-timeout-ms", value_name = "N")]
    autonomous_timeout_ms: Option<String>,

    #[arg(long = "skill")]
    skill: Vec<std::path::PathBuf>,

    #[arg(long)]
    no_skills: bool,

    #[arg(long)]
    no_context_files: bool,

    #[arg(long)]
    no_session: bool,

    #[arg(long, hide = true)]
    recovered: bool,

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
                .find(|meta| {
                    mush_session::place::same_place(&meta.cwd, &cwd) && meta.parent.is_none()
                })
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

    fn startup_commands(
        &self,
        defaults: &mush_runtime::AutonomousDefaults,
    ) -> Vec<mush_runtime::ClientCommand> {
        let mut commands = Vec::new();
        if let Some(objective) = &self.goal {
            commands.push(mush_runtime::ClientCommand::Goal(
                mush_runtime::GoalCommand::Create {
                    objective: objective.clone(),
                    token_budget: self.goal_budget,
                },
            ));
        }
        let wants_autonomous = self.autonomous
            || defaults.enabled
            || !self.autonomous_gate.is_empty()
            || self.autonomous_max_continuations.is_some()
            || self.autonomous_max_turns.is_some()
            || self.autonomous_max_tokens.is_some()
            || self.autonomous_timeout_ms.is_some();
        if wants_autonomous {
            let mut words = Vec::new();
            for gate in &self.autonomous_gate {
                words.push("--gate".to_string());
                words.push(gate.clone());
            }
            for (flag, value) in [
                ("--max-continuations", &self.autonomous_max_continuations),
                ("--max-turns", &self.autonomous_max_turns),
                ("--max-tokens", &self.autonomous_max_tokens),
                ("--timeout-ms", &self.autonomous_timeout_ms),
            ] {
                if let Some(value) = value {
                    words.push(flag.to_string());
                    words.push(value.clone());
                }
            }
            match mush_tui::slash::parse_autonomous_options(&words) {
                Ok(overrides) => {
                    let (limits, gates, subagent_keep_alive_ms) = overrides.resolve(defaults);
                    commands.push(mush_runtime::ClientCommand::Autonomous(
                        mush_runtime::AutonomousCommand::On {
                            limits,
                            gates,
                            subagent_keep_alive_ms,
                        },
                    ));
                }
                Err(error) => eprintln!("{}", Style::Dim.stderr(format_args!("{error}"))),
            }
        }
        commands
    }

    fn client_args(&self) -> setup::ClientArgs {
        setup::ClientArgs {
            model: self.model.clone(),
            thinking: self.thinking,
            max_tokens: self.max_tokens,
            config_overrides: self.option.clone(),
            discovery: self.discovery(),
            debug_cache: self.debug_cache,
        }
    }

    fn setup_args(&self, model: Option<String>, kernel_sinks: setup::KernelSinks) -> SetupArgs {
        SetupArgs {
            client: setup::ClientArgs {
                model,
                ..self.client_args()
            },
            max_turns: self.max_turns,
            system: self.system.clone(),
            no_tools: self.no_tools,
            tools: self.tools.clone(),
            append_system: self.append_system.clone(),
            kernel_sinks,
            timer: self.profile_startup.then(timing::PhaseTimer::new),
        }
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
enum ScheduleCommand {
    List {
        agent: Option<String>,
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Add {
        agent: String,
        expression: String,
        #[arg(num_args = 1.., trailing_var_arg = true)]
        message: Vec<String>,
    },
    #[command(alias = "remove", alias = "delete")]
    Cancel { job: String },
}

#[derive(clap::Subcommand)]
enum Command {
    Control(mush_cli::control::ControlArgs),
    Worker {
        #[arg(long, value_name = "SOCKET")]
        socket: std::path::PathBuf,
        #[arg(long, value_name = "FILE")]
        descriptor: Option<std::path::PathBuf>,
        #[arg(long, value_name = "SOCKET")]
        daemon_socket: Option<std::path::PathBuf>,
    },
    Daemon {
        #[arg(long, value_name = "SOCKET")]
        socket: Option<std::path::PathBuf>,
    },
    #[command(alias = "ps")]
    List {
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    Agents,
    Attach {
        agent: String,
    },
    Rename {
        agent: String,
        #[arg(num_args = 1..)]
        name: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    Pause {
        agent: Option<String>,
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        reason: Option<String>,
    },
    Unpause {
        agent: Option<String>,
        #[arg(short, long)]
        all: bool,
    },
    Stop {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Send {
        session: String,
        #[arg(num_args = 1.., trailing_var_arg = true)]
        text: Vec<String>,
    },
    Shutdown {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    Doctor {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        json: bool,
    },
    Login {
        provider: Option<String>,
        #[arg(long)]
        account: Option<mush_ai::types::AccountLabel>,
    },
    Logout {
        provider: Option<String>,
        #[arg(long)]
        account: Option<mush_ai::types::AccountLabel>,
    },
    Accounts {
        provider: Option<String>,
        #[arg(long)]
        default: Option<mush_ai::types::AccountLabel>,
    },
    Models,
    Status,
    Delete {
        id: String,
    },
    Slim {
        session: Option<String>,
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        apply: bool,
    },
    #[command(about = mush_cli::sync::ABOUT, long_about = mush_cli::sync::LONG_ABOUT)]
    Sync {
        #[command(subcommand)]
        command: Option<mush_cli::sync::SyncCommand>,
    },
    Config,
}

impl Cli {
    fn absorb_attach(&mut self) {
        match self.command.take() {
            Some(Command::Agents) => self.resume = Some(None),
            Some(Command::Attach { agent }) => self.resume = Some(Some(agent)),
            command => self.command = command,
        }
    }

    fn terminal_policy_overrides(&self) -> mush_tui::TerminalPolicyOverrides {
        mush_tui::TerminalPolicyOverrides {
            keyboard_enhancement: self.tui_keyboard_enhancement,
            image_probe: self.tui_image_probe,
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(report) => match report.downcast_ref::<headless::PrintExit>() {
            Some(exit) => std::process::ExitCode::from(exit.code),
            None => {
                eprintln!("Error: {report:?}");
                std::process::ExitCode::FAILURE
            }
        },
    }
}

async fn run() -> Result<()> {
    color_eyre::config::HookBuilder::default()
        .theme(if mush_cli::output::stderr_colour() {
            color_eyre::config::Theme::dark()
        } else {
            color_eyre::config::Theme::new()
        })
        .install()?;
    let mut cli = Cli::parse();
    cli.absorb_attach();
    let command = cli.command.take();
    if let Some(Command::Control(args)) = command {
        return mush_cli::control::run(args).await;
    }
    if let Some(Command::Sync {
        command: Some(sync),
    }) = &command
        && sync.runs_as_service()
    {
        mush_cli::sync::log_to_stderr();
        let cfg = config::load_config_with(&cli.option)?;
        return mush_cli::sync::run(Some(sync.clone()), &cfg.sync).await;
    }
    let cfg = config::load_config_with(&cli.option)?;
    let (_log_guards, log_buffer) = logging::init_logging(cfg.log_filter.as_deref(), cli.trace);
    tracing::info!("mush starting");

    mush_agent::truncation::cleanup();

    let Some(command) = command else {
        return prompt_or_tui(cli, log_buffer).await;
    };
    match command {
        Command::Control(_) | Command::Agents | Command::Attach { .. } => unreachable!(),
        Command::Worker {
            socket,
            descriptor,
            daemon_socket,
        } => worker_mode(cli, socket, descriptor, daemon_socket).await,
        Command::Daemon { socket } => {
            daemon_mode(&socket.unwrap_or_else(mush_daemon::daemon_socket_path)).await
        }
        Command::List { all, json } => agents::list(all, json).await,
        Command::Rename { agent, name, json } => {
            agents::rename(&agent, &name.join(" "), json).await
        }
        Command::Pause { agent, all, reason } => {
            mush_cli::pause::pause(agent.as_deref(), all, reason.as_deref()).await
        }
        Command::Unpause { agent, all } => mush_cli::pause::resume(agent.as_deref(), all).await,
        Command::Schedule { command } => match command {
            ScheduleCommand::List { agent, all, json } => {
                schedule::list(agent.as_deref(), all, json).await
            }
            ScheduleCommand::Add {
                agent,
                expression,
                message,
            } => schedule::add(&agent, &expression, &message.join(" ")).await,
            ScheduleCommand::Cancel { job } => schedule::cancel(&job).await,
        },
        Command::Stop { id, json } => daemon::stop(&id, json).await,
        Command::Send { session, text } => daemon::send(&session, &text.join(" ")).await,
        Command::Shutdown { force, json } => daemon::shutdown(force, json).await,
        Command::Doctor { fix, json } => doctor::doctor(fix, json).await,
        Command::Login { provider, account } => commands::login(provider, account).await,
        Command::Logout { provider, account } => commands::logout(provider, account.as_ref()),
        Command::Accounts { provider, default } => commands::accounts(provider, default.as_ref()),
        Command::Models => commands::list_models(),
        Command::Status => commands::status(cli.terminal_policy_overrides()).await,
        Command::Delete { id } => commands::delete_session(&id),
        Command::Slim {
            session,
            all,
            apply,
        } => commands::slim_sessions(session.as_deref(), all, apply),
        Command::Sync { command } => mush_cli::sync::run(command, &cfg.sync).await,
        Command::Config => commands::open_config(),
    }
}

async fn prompt_or_tui(cli: Cli, log_buffer: logging::LogBuffer) -> Result<()> {
    let prompt = if !cli.prompt.is_empty() {
        Some(cli.prompt.join(" "))
    } else if !cli.resumes_a_session()
        && cli.status_file.is_none()
        && !atty::is(atty::Stream::Stdin)
    {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        (!buf.is_empty()).then_some(buf)
    } else {
        None
    };

    match prompt {
        Some(prompt) => print_mode(cli, expand_template(&prompt)).await,
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

fn print_failure(message: String) -> color_eyre::Report {
    eprintln!("{}", Style::Red.stderr(format_args!("error: {message}")));
    headless::PrintExit { code: 1, message }.into()
}

async fn print_session(
    cli: &Cli,
    store: &SessionStore,
    cwd: &std::path::Path,
    cfg: &config::Config,
) -> Result<Stored> {
    let target = cli.session_target()?;
    if matches!(target, SessionTarget::Pick) {
        return Err(eyre!("--resume without an id needs the tui"));
    }
    let cwd_str = cwd.display().to_string();
    open_stored(store, &target, &cwd_str, &cfg.sync, cli.no_session, || {
        let model_id = match cli.model.clone() {
            Some(model) => model,
            None => setup::default_model_id(cfg, cwd),
        };
        Session::new(&model_id, &cwd_str)
    })
    .await
}

struct Stored {
    session: Session,
    lease: Option<mush_session::SessionLease>,
    remote: Option<mush_sync::Checkout>,
}

async fn open_stored(
    store: &SessionStore,
    target: &SessionTarget,
    cwd: &str,
    sync: &config::SyncConfig,
    no_session: bool,
    fresh: impl FnOnce() -> Session,
) -> Result<Stored> {
    let (id, prepared) = match target {
        SessionTarget::Resume(id) => match store.resolve(id) {
            Ok(meta) => (meta.id, None),
            Err(mush_session::StoreError::NotFound(missing)) if sync.settings().is_some() => {
                return Err(eyre!(
                    "session not found: {missing}. mush sync fetches sessions from your other machines"
                ));
            }
            Err(error) => return Err(error.into()),
        },
        SessionTarget::Fork(id) => {
            let meta = store.resolve(id)?;
            let session = fork_session(store.load(&meta.id)?, cwd);
            (session.meta.id.clone(), Some(session))
        }
        SessionTarget::New | SessionTarget::Pick => {
            let session = fresh();
            (session.meta.id.clone(), Some(session))
        }
    };
    let lease = (!no_session).then(|| store.acquire(&id)).transpose()?;
    let remote = match &lease {
        Some(lease) => mush_cli::sync::checkout(sync, lease).await?,
        None => None,
    };
    let session = match prepared {
        Some(session) => session,
        None => store.load(&id)?,
    };
    Ok(Stored {
        session,
        lease,
        remote,
    })
}

async fn print_mode_inner(cli: Cli, prompt: String, status: &mut headless::Status) -> Result<()> {
    let store = SessionStore::new(SessionStore::default_dir());
    let cwd = mush_tui::canonical_dir(&std::env::current_dir()?);
    let cfg = config::load_config_with(&cli.option)?;
    let Stored {
        mut session,
        lease: _lease,
        remote,
    } = print_session(&cli, &store, &cwd, &cfg).await?;
    if let Some(model) = &cli.model {
        session.meta.model_id = model.clone().into();
    }
    status.model = Some(session.meta.model_id.to_string());
    let thinking =
        mush_ai::discovery::resolve_model_by_id(session.meta.model_id.as_str()).map(|model| {
            let level = setup::session_thinking(cli.thinking, &model, &cfg, &session.conversation);
            session.conversation.record_thinking_level(&level);
            level
        });
    let prompt_message = (!prompt.is_empty()).then(|| {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(prompt))
                .timestamp_ms(Timestamp::now())
                .prompt_id(PromptId::new())
                .build(),
        )
    });
    if let Some(message) = &prompt_message {
        session.push_message(message.clone());
        session.auto_title();
    }
    status.session_id = Some(session.meta.id.clone());
    let persist = |session: &Session| -> Result<()> {
        if !cli.no_session {
            store.save(session)?;
        }
        Ok(())
    };
    persist(&session)?;
    status.write()?;
    let mut shutdown = std::pin::pin!(headless::shutdown_signal());
    let init = AppSetup::init(cli.setup_args(
        Some(session.meta.model_id.to_string()),
        setup::KernelSinks::default(),
    ));
    let (mut setup, startup_report) = tokio::select! {
        biased;
        result = &mut shutdown => {
            let signal = result?;
            status.state = "cancelled";
            return Err(headless::PrintExit::cancelled(signal).into());
        }
        result = init => result?,
    };

    if let Some(report) = startup_report {
        eprintln!("{report}");
    }

    setup.client.options.session_id = Some(session.meta.id.clone());
    if let Some(level) = &thinking {
        setup.client.options.thinking =
            Some(setup.client.model.clamp_thinking(level).normalize_visible());
    }
    session.meta.model_id = setup.client.model.id.clone();
    status.model = Some(setup.client.model.id.to_string());
    status.state = "running";
    persist(&session)?;
    status.write()?;

    let control = mush_agent::ipc::control::Control::new(session.meta.id.to_string());
    let listener = mush_agent::IpcListener::start_control(
        mush_agent::ipc::control_socket_path(&control.target().instance),
        std::sync::Arc::new(mush_agent::AgentCard::build(
            &setup.client.model.id,
            &setup.tools,
        )),
        control.clone(),
    )?;
    let control_endpoint = Some(mush_agent::exec_context::ControlEndpoint {
        socket: listener.path().to_owned(),
        target: control.target(),
    });

    let context_tokens = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let transform = compaction_transform(&setup, context_tokens.clone());

    let config = AgentConfig {
        model: setup.client.model.clone(),
        system_prompt: Some(setup.system_prompt),
        tools: setup.tools.clone(),
        registry: &setup.registry,
        options: setup.client.options,
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
    let json = cli.mode == OutputMode::Json;
    let mut json_events = json.then(|| {
        println!("{}", json_events::header(&session));
        json_events::JsonEvents::new(setup.client.model.clone(), prompt_message)
    });
    let mut stream = std::pin::pin!(agent_loop(config, session.context()));
    let mut transcript = headless::Transcript::default();
    let mut signalled = None;
    let mut cancel_deadline = None;
    let mut ended = false;
    let mut final_message: Option<AssistantMessage> = None;
    let mut failure: Option<String> = None;

    loop {
        let event = tokio::select! {
            biased;
            result = &mut shutdown, if signalled.is_none() => {
                signalled = Some(result?);
                cancel.cancel();
                status.state = "cancelled";
                persist(&session)?;
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
        if transcript.apply(&mut session, &event) {
            persist(&session)?;
        }
        if let Some(json_events) = &mut json_events {
            for line in json_events.apply(&event) {
                println!("{line}");
            }
        }
        print_progress(&event, &setup.client.model, setup.client.debug_cache);
        match event {
            AgentEvent::AgentEnd => ended = true,
            AgentEvent::MessageEnd { message } => {
                context_tokens.store(
                    message.usage.total_input_tokens().get(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                final_message = Some(message);
            }
            AgentEvent::ContextCompacted {
                before_count,
                compaction,
            } => {
                if mush_runtime::apply_context_compaction(&mut session.conversation, &compaction)
                    .is_err()
                {
                    eprintln!(
                        "{}",
                        Style::Yellow.stderr(format_args!(
                            "compaction discarded, the conversation changed while it ran"
                        ))
                    );
                    continue;
                }
                let after_count = session.conversation.context_len();
                session.meta.message_count = after_count;
                persist(&session)?;
                eprintln!(
                    "{}",
                    Style::Dim.stderr(format_args!(
                        "(compacted: {before_count} → {after_count} messages)"
                    ))
                );
            }
            AgentEvent::MaxTurnsReached { .. } if signalled.is_none() => {
                status.state = "turn_limit";
            }
            AgentEvent::Error { error } => failure = Some(error),
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
    if let Some(remote) = remote {
        remote.finish().await;
    }

    listener.shutdown().await?;
    if let Some(signal) = signalled {
        return Err(headless::PrintExit::cancelled(signal).into());
    }
    if let Some(error) = failure {
        return Err(print_failure(format_error(&error)));
    }
    if !ended || !transcript.is_complete() {
        return Err(print_failure(
            "agent ended with an incomplete transcript".into(),
        ));
    }
    if !json && let Some(message) = final_message {
        print_final_text(&message);
    }
    Ok(())
}

fn print_progress(event: &AgentEvent, model: &Model, debug_cache: bool) {
    match event {
        AgentEvent::ToolExecStart {
            tool_name, args, ..
        } => {
            let args_summary = summarise_tool_args(tool_name.as_str(), args);
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
        AgentEvent::TurnEnd { message, .. } => {
            let cost = models::calculate_cost(model, &message.usage);
            eprintln!(
                "{}",
                Style::Dim.stderr(format_args!(
                    "{} | in:{} out:{} cache:{} | {}",
                    message.model,
                    message.usage.input_tokens,
                    message.usage.output_tokens,
                    message.usage.cache_read_tokens,
                    cost.total(),
                )),
            );
            if debug_cache && message.usage.cache_read_tokens > TokenCount::ZERO {
                eprintln!(
                    "{}",
                    Style::Cyan.stderr(format_args!(
                        "cache read detected: {} tokens",
                        message.usage.cache_read_tokens
                    ))
                );
            }
        }
        AgentEvent::PartialDiscarded { error } => {
            eprintln!(
                "{}",
                Style::Yellow.stderr(format_args!(
                    "⚠ discarding the partial response and retrying: {}",
                    format_error(error)
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
            eprintln!(
                "{}",
                Style::Yellow.stderr(format_args!("⚠ hit max turns limit ({max_turns})"))
            );
        }
        _ => {}
    }
}

fn compaction_transform(
    setup: &AppSetup,
    context_tokens: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Option<mush_agent::TransformFn> {
    if !setup.client.cfg.compaction.enabled {
        return None;
    }
    let (compact_model, compact_options) = setup
        .client
        .compaction_model
        .clone()
        .unwrap_or_else(|| (setup.client.model.clone(), setup.client.options.clone()));
    let context_window = compact_model
        .context_window
        .max(setup.client.model.context_window);
    let session_model = (compact_model.id != setup.client.model.id)
        .then(|| (setup.client.model.clone(), setup.client.options.clone()));
    let target = mush_runtime::CompactionTarget {
        model: compact_model,
        options: compact_options,
        window: context_window,
        session: session_model,
    };
    let reg_ref = setup.registry.clone();
    let lifecycle_hooks = std::sync::Arc::new(setup.lifecycle_hooks.clone());
    let cwd = std::sync::Arc::new(setup.client.cwd.clone());
    let live = std::sync::Arc::new(mush_session::compact::LiveContext {
        system_prompt: Some(setup.system_prompt.clone()),
        tools: mush_agent::tool_definitions(&setup.tools, setup.client.model.api),
    });
    let compaction_settings = setup.client.cfg.compaction.settings();
    let kernel = setup.kernel_state.clone();
    Some(Box::new(move |msgs, pressure| {
        let target = target.clone();
        let ctx = context_tokens.clone();
        let owned = msgs.to_vec();
        let hooks = lifecycle_hooks.clone();
        let cwd = cwd.clone();
        let reg = reg_ref.clone();
        let live = live.clone();
        let kernel = kernel.clone();
        Box::pin(async move {
            let tokens = TokenCount::new(ctx.load(std::sync::atomic::Ordering::Relaxed));
            let tokens = match pressure {
                mush_agent::ContextPressure::Normal => tokens,
                mush_agent::ContextPressure::Overflowed => tokens.max(context_window),
            };
            let target = target.fitting(|| {
                mush_session::compact::summary_request_tokens(
                    &owned,
                    compaction_settings.keep_recent(&owned),
                    &mush_session::compact::CompactionFocus::default(),
                    &compaction_settings,
                )
            });
            let mut result = auto_compact(
                owned,
                tokens,
                context_window,
                &reg,
                &target.model,
                &target.options,
                Some(&hooks),
                Some(cwd.as_path()),
                target.uses_session_model().then_some(live.as_ref()),
                &compaction_settings,
            )
            .await;
            if result.summarised_count > 0 {
                let restored = mush_runtime::AfterCompaction {
                    kernel: kernel.clone(),
                    ..Default::default()
                }
                .messages(&result.messages)
                .await;
                let appended = restored.len();
                result.messages.extend(restored);
                ctx.store(
                    mush_session::compact::estimate_tokens(&result.messages) as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                mush_agent::ContextTransformResult::Compacted(mush_agent::ContextCompaction {
                    messages: result.messages,
                    summary: result.summary,
                    summarised_count: result.summarised_count,
                    appended,
                })
            } else {
                mush_agent::ContextTransformResult::Unchanged
            }
        })
    }))
}

fn print_final_text(message: &AssistantMessage) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    for part in &message.content {
        if let AssistantContentPart::Text(text) = part {
            let _ = writeln!(stdout, "{}", text.text);
        }
    }
    let _ = stdout.flush();
}

struct Prepared {
    setup: AppSetup,
    kernel_signals: mush_runtime::KernelSignals,
}

struct OpenedSession {
    session: Session,
    lease: Option<mush_session::SessionLease>,
    saver: Option<SaverThread>,
    remote: Option<mush_sync::Checkout>,
}

async fn daemon_mode(socket: &std::path::Path) -> Result<()> {
    let program = std::env::current_exe()?;
    let supervisor = mush_daemon::Supervisor::new(
        program,
        socket.to_path_buf(),
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
    daemon_socket: Option<std::path::PathBuf>,
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
    let mut opened = open_session(&cli.session_target()?, &setup, cli.no_session).await?;
    let worker = worker_config(&cli, &setup, &mut opened, kernel_signals);
    eprintln!(
        "{}",
        Style::Dim.stderr(format_args!("worker listening on {}", socket.display()))
    );
    let mush_runtime::HostedWorker {
        mut connection,
        ipc_listener,
    } = mush_runtime::host_worker(worker, &setup.tools, &setup.registry)?;
    if let Some(remote) = &opened.remote {
        connection.follow_sync(remote.state());
    }
    let shutdown = tokio_util::sync::CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let _ = headless::shutdown_signal().await;
            shutdown.cancel();
        }
    });
    if let Some(daemon_socket) = daemon_socket {
        tokio::spawn(mush_daemon::election::watch(
            daemon_socket,
            std::env::current_exe()?,
            mush_session::data_dir().join("workers"),
            shutdown.clone(),
        ));
    }
    let served = mush_daemon::serve(connection, listener, descriptor, shutdown).await;
    let _ = std::fs::remove_file(&socket);
    served?;
    if let Some(listener) = ipc_listener {
        listener.shutdown().await?;
    }
    if let Some(saver) = opened.saver {
        saver.finish();
    }
    if let Some(remote) = opened.remote {
        remote.finish().await;
    }
    Ok(())
}

fn spawn_discovery_refresh(
    http_client: mush_ai::http::HttpClient,
) -> std::sync::mpsc::Receiver<mush_tui::runner::DiscoveryUpdate> {
    let (discovery_tx, discovery_rx) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        let summary = mush_ai::discovery::refresh_and_save(http_client).await;
        tracing::info!(
            target: "mush::discovery",
            models = summary.total_models(),
            "{}",
            summary.one_line()
        );
        let _ = discovery_tx.send(mush_tui::runner::DiscoveryUpdate::Refreshed);
    });
    discovery_rx
}

async fn tui_mode(cli: Cli, log_buffer: logging::LogBuffer) -> Result<()> {
    let target = cli.session_target()?;
    match cli.no_session {
        false => {
            let mut timer = cli.profile_startup.then(timing::PhaseTimer::new);
            let client = setup::ClientSetup::init(cli.client_args(), &mut timer).await?;
            if let Some(report) = timer.map(timing::PhaseTimer::finish) {
                eprintln!("{report}");
            }
            let discovery_rx = spawn_discovery_refresh(client.http_client.clone());
            let open = match &target {
                SessionTarget::Resume(id) => mush_daemon::OpenTarget::Resume(id.clone()),
                SessionTarget::Fork(id) => mush_daemon::OpenTarget::Fork(id.clone()),
                SessionTarget::New | SessionTarget::Pick => mush_daemon::OpenTarget::New,
            };
            let descriptor = match daemon::open(open.clone(), cli.worker_args()).await {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    let stale = error.downcast::<daemon::StaleDaemon>()?;
                    daemon::take_over(&stale).await?;
                    daemon::open(open, cli.worker_args()).await?
                }
            };
            let connection = mush_daemon::DaemonConnection::connect(&descriptor.socket).await?;
            let mut tui_config = client_config(&cli, &client, &log_buffer, None, None);
            tui_config.open_session_picker = matches!(target, SessionTarget::Pick);
            tui_config.worker_opener = Some(daemon::worker_opener(cli.worker_args()));
            tui_config.agent_roster = Some(agents::roster(client.cfg.sync.clone()));
            if connection.worker_build() != mush_daemon::build_id() {
                tui_config.startup_notice = Some(format!(
                    "this worker is from another build of mush. mush stop {} and start again to pick up the new one",
                    mush_cli::output::short_id(&descriptor.session_id)
                ));
            }
            let attached =
                mush_tui::run_tui_attached(tui_config, Box::new(connection), Some(discovery_rx))
                    .await
                    .map_err(|e| eyre!("TUI error: {e}"))?;
            eprintln!(
                "{}",
                Style::Dim.stderr(format_args!(
                    "detached, mush --resume {} reattaches",
                    mush_cli::output::short_id(attached.as_str())
                ))
            );
        }
        true => {
            let Prepared {
                setup,
                kernel_signals,
            } = prepare(&cli).await?;
            let discovery_rx = spawn_discovery_refresh(setup.client.http_client.clone());
            let mut opened = open_session(&target, &setup, cli.no_session).await?;
            let worker = worker_config(&cli, &setup, &mut opened, kernel_signals);
            let mut tui_config = client_config(
                &cli,
                &setup.client,
                &log_buffer,
                Some(&opened.session),
                Some(&setup),
            );
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

fn resumed_model(cli: &Cli) -> Option<String> {
    let (SessionTarget::Resume(id) | SessionTarget::Fork(id)) = cli.session_target().ok()? else {
        return None;
    };
    let meta = SessionStore::new(SessionStore::default_dir())
        .resolve(&id)
        .ok()?;
    mush_ai::discovery::resolve_model_by_id(meta.model_id.as_str())
        .map(|model| model.id.to_string())
}

async fn prepare(cli: &Cli) -> Result<Prepared> {
    let (sinks, kernel_signals) = mush_runtime::kernel_channels();
    let (setup, startup_report) = AppSetup::init(cli.setup_args(
        cli.model.clone().or_else(|| resumed_model(cli)),
        sinks.into(),
    ))
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
    let cwd_str = setup.client.cwd.display().to_string();
    let store = SessionStore::new(SessionStore::default_dir());
    let Stored {
        mut session,
        lease,
        remote,
    } = open_stored(
        &store,
        target,
        &cwd_str,
        &setup.client.cfg.sync,
        no_session,
        || Session::new(setup.client.model.id.as_str(), &cwd_str),
    )
    .await?;
    if !session.meta.lifecycle.is_live() {
        session.meta.lifecycle = mush_session::Lifecycle::Live;
        if lease.is_some() {
            store.set_lifecycle(&session.meta.id, mush_session::Lifecycle::Live)?;
        }
    }
    let saver = (!no_session)
        .then(|| SaverThread::spawn(store, cwd_str))
        .transpose()?;
    Ok(OpenedSession {
        session,
        lease,
        saver,
        remote,
    })
}

fn agent_card(setup: &AppSetup) -> mush_agent::AgentCard {
    let mut card = mush_agent::AgentCard::build(&setup.client.model.id, &setup.tools);
    card.capabilities = mush_agent::Capabilities {
        streaming: true,
        multi_pane: true,
        messaging: true,
        lsp: setup.lsp_diagnostics.is_some(),
    };
    card
}

#[derive(Clone)]
struct ChildSeed {
    model: Model,
    options: StreamOptions,
    max_turns: usize,
    provider_api_keys: std::collections::HashMap<String, mush_ai::ApiKey>,
    confirm_tools: bool,
    forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars,
    kernel: config::KernelConfig,
    web_search: mush_tools::web_search::WebSearchConfig,
    kernel_packages: Vec<String>,
    cwd: std::path::PathBuf,
    discovery: mush_ext::DiscoveryOptions,
    registry: mush_ai::registry::ApiRegistry,
    lifecycle_hooks: mush_agent::LifecycleHooks,
    file_rules: Option<mush_agent::FileRuleCallback>,
    lsp_diagnostics: Option<mush_agent::DiagnosticCallback>,
    saver: Option<mush_runtime::SessionSaver>,
    rlm_max_depth: u32,
    subagent_model: Option<String>,
    models: mush_runtime::ModelSelection,
    session_dir: std::path::PathBuf,
    prompt_overrides: setup::PromptOverrides,
    compaction: CompactionSeed,
}

#[derive(Clone)]
struct CompactionSeed {
    auto: bool,
    settings: mush_session::compact::CompactionSettings,
    model: Option<(Model, StreamOptions)>,
    agent_callable: bool,
}

impl CompactionSeed {
    fn from_setup(setup: &AppSetup) -> Self {
        let cfg = &setup.client.cfg.compaction;
        Self {
            auto: cfg.enabled,
            settings: cfg.settings(),
            model: setup.client.compaction_model.clone(),
            agent_callable: cfg.agent_callable,
        }
    }
}

fn rlm_system_prompt(
    cwd: &std::path::Path,
    discovery: &mush_ext::DiscoveryOptions,
    overrides: &setup::PromptOverrides,
    kernel_packages: &[String],
    role: &setup::RlmRole,
) -> String {
    let mut project = mush_ext::loader::discover_project_context_with(cwd, discovery);
    overrides.apply(&mut project);
    setup::build_rlm_system_prompt(&project, cwd, kernel_packages, role)
}

fn turn_config(
    seed: &ChildSeed,
    session_id: &SessionId,
    model: Model,
    options: StreamOptions,
) -> mush_runtime::TurnConfig {
    let mut options = options;
    options.session_id = Some(session_id.clone());
    mush_runtime::TurnConfig {
        default_model: model,
        options,
        max_turns: seed.max_turns,
        provider_api_keys: seed.provider_api_keys.clone(),
        confirm_tools: seed.confirm_tools,
        auto_compact: seed.compaction.auto,
        compaction: seed.compaction.settings,
        branch_summary: Default::default(),
        auto_refine: Default::default(),
        auto_fork_compact: false,
        compaction_model: seed.compaction.model.clone().map(|(model, mut options)| {
            options.session_id = Some(session_id.clone());
            (model, options)
        }),
        forbidden_chars: seed.forbidden_chars.clone(),
        tool_allowlist: Some(vec!["ipython".into()]),
        generate_title: false,
    }
}

fn parent_agent_name(spec: &mush_runtime::ChildSpec, session_dir: &std::path::Path) -> String {
    if spec.parent.id.is_some() {
        return spec.parent.name.clone();
    }
    let parent = SessionId::from(spec.parent_session_id.as_str());
    let path = SessionStore::new(session_dir.to_path_buf()).session_path(&parent);
    mush_session::store::load_meta(&path)
        .ok()
        .and_then(|meta| meta.title)
        .unwrap_or_else(|| spec.parent_session_id.clone())
}

fn child_artifacts(saved: bool, session_id: &SessionId) -> Option<std::path::PathBuf> {
    saved.then(|| SessionStore::default_artifact_dir(session_id))
}

fn child_factory(seed: ChildSeed) -> mush_runtime::ChildFactory {
    std::sync::Arc::new(move |spec: mush_runtime::ChildSpec, family| {
        let seed = seed.clone();
        Box::pin(async move {
            let chosen = seed
                .models
                .for_child(spec.model.as_deref())
                .or_else(|| seed.subagent_model.clone());
            let model = match chosen {
                Some(id) => mush_ai::discovery::resolve_model_by_id(&id)
                    .ok_or_else(|| format!("unknown model {id}"))?,
                None => mush_ai::discovery::resolve_model_by_id(&seed.models.current())
                    .unwrap_or_else(|| seed.model.clone()),
            };
            let mut options = seed.options.clone();
            let parent_thinking = spec
                .parent_thinking
                .clone()
                .or_else(|| seed.models.thinking())
                .or_else(|| options.thinking.clone())
                .unwrap_or(ThinkingLevel::Off);
            options.thinking = Some(mush_runtime::child_thinking(
                &model,
                spec.thinking.as_ref(),
                &parent_thinking,
            )?);
            let session = match &spec.resume_session {
                Some(id) => SessionStore::new(seed.session_dir.clone())
                    .load(&SessionId::from(id.as_str()))
                    .map_err(|error| format!("cannot reopen {}: {error}", spec.name))?,
                None => Session::new(model.id.as_str(), &seed.cwd.display().to_string()),
            };
            let (sinks, signals) = mush_runtime::kernel_channels();
            let cfg = config::Config {
                kernel: seed.kernel.clone(),
                web_search: seed.web_search.clone(),
                ..config::Config::default()
            };
            let mut project =
                mush_ext::loader::discover_project_context_with(&seed.cwd, &seed.discovery);
            seed.prompt_overrides.apply(&mut project);
            let ipython = std::sync::Arc::new(setup::build_ipython_tool(
                &cfg,
                &seed.cwd,
                &project.skills,
                sinks.into(),
            ));
            let identity = mush_runtime::Identity {
                id: Some(spec.id.clone()),
                name: spec.name.clone(),
                depth: spec.depth,
            };
            let goal = std::sync::Arc::new(mush_runtime::GoalStore::new(None));
            let scheduler = std::sync::Arc::new(mush_runtime::Scheduler::new(None));
            let notes =
                mush_runtime::Notes::new(child_artifacts(seed.saver.is_some(), &session.meta.id));
            let host_commands = std::sync::Arc::new(mush_runtime::HostCommands::default());
            if let Some(host) = ipython.mush_host() {
                host.set_extension(std::sync::Arc::new(
                    mush_runtime::RlmHandler::new(family.clone(), identity)
                        .with_goal(goal.clone())
                        .with_scheduler(scheduler.clone())
                        .with_notes(notes.clone())
                        .with_harness(std::sync::Arc::new(mush_runtime::HarnessStore::new(
                            &session.meta.id,
                        )))
                        .with_host_commands(host_commands.clone())
                        .with_compaction_callable(seed.compaction.agent_callable),
                ));
            }
            let max_depth = family.max_depth();
            let role = setup::RlmRole::child(&spec.name, spec.depth, max_depth)
                .spawned_by(parent_agent_name(&spec, &seed.session_dir));
            let system_prompt =
                setup::build_rlm_system_prompt(&project, &seed.cwd, &seed.kernel_packages, &role);
            let mut worker = mush_runtime::WorkerConfig::new(turn_config(
                &seed,
                &session.meta.id,
                model,
                options.clone(),
            ));
            worker.conversation = session.conversation.clone();
            worker.session_id = session.meta.id.clone();
            worker.title = Some(spec.name.clone());
            worker.parent = Some(spec.parent_session_id.clone().into());
            worker.system_prompt = Some(session.system_prompt.clone().unwrap_or(system_prompt));
            worker.thinking_level = options
                .thinking
                .clone()
                .unwrap_or(ThinkingLevel::Off)
                .normalize_visible();
            worker.rlm_depth = spec.depth;
            worker.rlm_max_depth = max_depth;
            worker.rlm_max_depth_source = mush_runtime::RlmMaxDepthSource::Inherited;
            worker.accounts = if spec.resume_session.is_some() {
                session.meta.accounts.clone()
            } else {
                seed.models.accounts()
            };
            worker.kernel_state = Some(ipython.clone());
            worker.saver = seed.saver.clone();
            worker.injections = mush_agent::AgentInjections {
                lifecycle_hooks: seed.lifecycle_hooks.clone(),
                cwd: Some(seed.cwd.clone()),
                file_rules: seed.file_rules.clone(),
                lsp_diagnostics: seed.lsp_diagnostics.clone(),
                control_endpoint: None,
            };
            worker.kernel_signals = Some(signals);
            worker.goal = Some(goal);
            worker.scheduler = Some(scheduler);
            worker.notes = Some(notes);
            worker.host_commands = Some(host_commands);
            worker.family_member = Some((family, spec.id.clone()));
            Ok(mush_runtime::ChildParts {
                config: worker,
                tools: mush_agent::tool::ToolRegistry::from_shared(vec![
                    ipython as mush_agent::tool::SharedTool,
                ]),
                registry: seed.registry.clone(),
            })
        })
    })
}

const WORKER_INTERRUPTED: &str = "The isolated session worker stopped during in-flight work. The saved transcript was recovered, but uncertain model, tool, bash, or child-agent work was not replayed. Inspect external side effects before continuing.";

fn worker_config(
    cli: &Cli,
    setup: &AppSetup,
    opened: &mut OpenedSession,
    kernel_signals: mush_runtime::KernelSignals,
) -> mush_runtime::WorkerConfig {
    let session = &opened.session;
    let models = mush_runtime::ModelSelection::new(
        setup.client.model.id.to_string(),
        session.meta.subagent_model.clone(),
    );
    let (base_depth, base_source) = setup::rlm_max_depth(&setup.client.cfg);
    let max_depth = session.meta.rlm_max_depth.unwrap_or(base_depth);
    let prompt_overrides = setup::PromptOverrides {
        system: cli
            .system
            .clone()
            .or(setup.client.cfg.system_prompt.clone()),
        append: cli.append_system.clone(),
    };
    let seed = ChildSeed {
        models: models.clone(),
        model: setup.client.model.clone(),
        options: setup.client.options.clone(),
        max_turns: setup.max_turns,
        provider_api_keys: setup.client.cfg.api_keys.to_map(),
        confirm_tools: setup.client.cfg.confirm_tools,
        web_search: setup.client.cfg.web_search.clone(),
        forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars::with_classes(
            setup.client.cfg.forbidden_chars.iter().copied(),
            setup.client.cfg.forbidden_char_classes.iter().copied(),
        ),
        kernel: setup.client.cfg.kernel.clone(),
        kernel_packages: setup.kernel_packages.clone(),
        cwd: setup.client.cwd.clone(),
        discovery: cli.discovery(),
        registry: setup.registry.clone(),
        lifecycle_hooks: setup.lifecycle_hooks.clone(),
        file_rules: setup.file_rules.clone(),
        lsp_diagnostics: setup.lsp_diagnostics.clone(),
        saver: opened.saver.as_ref().map(|saver| saver.handle()),
        rlm_max_depth: max_depth,
        subagent_model: setup.client.cfg.subagent_model.clone(),
        session_dir: SessionStore::default_dir(),
        prompt_overrides: prompt_overrides.clone(),
        compaction: CompactionSeed::from_setup(setup),
    };
    let mut turn = turn_config(
        &seed,
        &session.meta.id,
        setup.client.model.clone(),
        setup.client.options.clone(),
    );
    turn.branch_summary = setup.client.cfg.branch_summary.settings();
    turn.auto_refine = setup.client.cfg.auto_refine.settings();
    turn.auto_fork_compact = setup.client.cfg.auto_fork_compact;
    turn.tool_allowlist = if setup.client.rlm {
        Some(vec!["ipython".into()])
    } else {
        cli.tools.clone()
    };
    turn.generate_title = !cli.no_session;
    let mut worker = mush_runtime::WorkerConfig::new(turn);
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
        .client
        .options
        .thinking
        .clone()
        .unwrap_or(ThinkingLevel::Off)
        .normalize_visible();
    if cli.thinking && worker.conversation.leaf_id().is_some() {
        worker
            .conversation
            .record_thinking_level(&worker.thinking_level);
    }
    worker.rlm_depth = setup::rlm_depth();
    worker.rlm_max_depth = base_depth;
    worker.rlm_max_depth_source = base_source;
    worker.rlm_max_depth_chat = session.meta.rlm_max_depth;
    worker.lease = opened.lease.take();
    worker.kernel_state = setup.kernel_state.clone();
    worker.saver = seed.saver.clone();
    worker.injections = mush_agent::AgentInjections {
        lifecycle_hooks: setup.lifecycle_hooks.clone(),
        cwd: Some(std::env::current_dir().unwrap_or_default()),
        file_rules: setup.file_rules.clone(),
        lsp_diagnostics: setup.lsp_diagnostics.clone(),
        control_endpoint: None,
    };
    worker.agent_card = Some(agent_card(setup));
    worker.kernel_signals = Some(kernel_signals);
    worker.models = models;
    worker.subagent_model = session.meta.subagent_model.clone();
    worker.accounts = session.meta.accounts.clone();
    worker.paused = session.meta.paused.clone();
    if let Some(ipython) = &setup.ipython_tool {
        let root = mush_runtime::Identity::root(
            session
                .meta
                .title
                .clone()
                .unwrap_or_else(|| "parent".to_string()),
        );
        let artifacts =
            (!cli.no_session).then(|| SessionStore::default_artifact_dir(&session.meta.id));
        let family = mush_runtime::Family::new(
            root.clone(),
            session.meta.id.to_string(),
            seed.rlm_max_depth,
            Some(child_factory(seed)),
            artifacts.as_ref().map(|dir| dir.join("subagents.json")),
        );
        let goal = std::sync::Arc::new(mush_runtime::GoalStore::new(
            artifacts.as_ref().map(|dir| dir.join("goal.json")),
        ));
        let scheduler = std::sync::Arc::new(mush_runtime::Scheduler::new(
            artifacts
                .as_ref()
                .map(|dir| dir.join("scheduled-jobs.json")),
        ));
        let notes = mush_runtime::Notes::new(artifacts.clone());
        let host_commands = std::sync::Arc::new(mush_runtime::HostCommands::default());
        if let Some(host) = ipython.mush_host() {
            let mut handler = mush_runtime::RlmHandler::new(family.clone(), root)
                .with_goal(goal.clone())
                .with_scheduler(scheduler.clone())
                .with_notes(notes.clone())
                .with_harness(std::sync::Arc::new(mush_runtime::HarnessStore::new(
                    &session.meta.id,
                )))
                .with_host_commands(host_commands.clone())
                .with_compaction_callable(setup.client.cfg.compaction.agent_callable);
            if !cli.no_session {
                handler = handler.with_session_spawner(daemon::session_spawner(cli.worker_args()));
            }
            host.set_extension(std::sync::Arc::new(handler));
        }
        worker.agent_inbox = Some(family.root_inbox());
        worker.observe_inbox = Some(family.root_observe_inbox());
        worker.family = Some(family);
        worker.goal = Some(goal);
        worker.scheduler = Some(scheduler);
        worker.notes = Some(notes);
        worker.host_commands = Some(host_commands);
    }
    let role = setup::RlmRole {
        depth: worker.rlm_depth,
        max_depth,
        child_name: None,
        parent_name: None,
    };
    worker.system_prompt_builder = Some(std::sync::Arc::new({
        let discovery = cli.discovery();
        let rlm = setup.client.rlm;
        let kernel_packages = setup.kernel_packages.clone();
        let cwd = setup.client.cwd.clone();
        let role = role.clone();
        let family = worker.family.clone();
        move || {
            if rlm {
                let role = setup::RlmRole {
                    max_depth: family
                        .as_ref()
                        .map_or(role.max_depth, |family| family.max_depth()),
                    ..role.clone()
                };
                rlm_system_prompt(&cwd, &discovery, &prompt_overrides, &kernel_packages, &role)
            } else {
                let mut project = mush_ext::loader::discover_project_context_with(&cwd, &discovery);
                prompt_overrides.apply(&mut project);
                build_system_prompt_from_context(&project, &cwd)
            }
        }
    }));

    worker.autonomous = Some(std::sync::Arc::new(mush_runtime::Autonomous::new(
        setup.client.cwd.clone(),
    )));
    if cli.recovered {
        worker.recovery_notice = Some(WORKER_INTERRUPTED.into());
    }
    worker
}

fn client_config(
    cli: &Cli,
    client: &setup::ClientSetup,
    log_buffer: &logging::LogBuffer,
    session: Option<&Session>,
    worker: Option<&AppSetup>,
) -> TuiConfig {
    let terminal_policy_overrides = cli.terminal_policy_overrides();
    let session_id = session
        .map(|session| session.meta.id.clone())
        .unwrap_or_default();
    let mut options = client.options.clone();
    options.session_id = Some(session_id.clone());

    let theme = mush_tui::Theme::from_config(&client.cfg.theme);
    let terminal_policy = client
        .cfg
        .terminal
        .with_overrides(terminal_policy_overrides);
    let config_file = config::config_dir().join("config.toml");
    let (favourite_models, favourites_locked) = config::resolve_favourites(
        &client.cfg.favourite_models,
        config::load_favourite_models(),
    );

    TuiConfig {
        model: client.model.clone(),
        options,
        initial_title: session.and_then(|session| session.meta.title.clone()),
        theme,
        config_path: if config_file.exists() {
            Some(config_file)
        } else {
            None
        },
        provider_api_keys: client.cfg.api_keys.to_map(),
        last_models: config::load_last_models(),
        save_last_models: Some(std::sync::Arc::new(|models| {
            config::save_last_models(models);
        })),
        feature_hints: client.cfg.feature_hints,
        show_cost: client.cfg.show_cost,
        show_usage_lines: client.cfg.show_usage_lines,
        show_token_counters: client.cfg.show_token_counters,
        status_bar_config: client.cfg.status_bar.clone(),
        debug_cache: client.debug_cache,
        cache_timer: client.cfg.cache_timer,
        thinking_display: client.cfg.thinking_display,
        log_buffer: Some({
            let buf = log_buffer.clone();
            std::sync::Arc::new(move |n| buf.tail(n))
        }),
        terminal_policy,
        cwd: std::env::current_dir().unwrap_or_default(),
        agent_card: worker.map(agent_card),
        model_tiers: client.cfg.model_tiers.clone(),
        compaction_model: client.compaction_model.clone(),
        resolve_model: Some(std::sync::Arc::new(mush_ai::discovery::resolve_model_by_id)),
        http_client: Some(client.http_client.clone()),
        session_id,
        settings: mush_tui::settings::ScopedSettings {
            scope: client.cfg.settings.scope,
            anthropic_betas: client.cfg.settings.anthropic_betas.clone(),
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
            let warnings = km.override_scoped(&client.cfg.keys.scoped());
            for warning in warnings {
                tracing::warn!("keybind config: {warning}");
            }
            km
        },
        open_session_picker: false,
        branch_summary_skip_prompt: client.cfg.branch_summary.skip_prompt,
        startup_notice: None,
        startup_commands: cli.startup_commands(&client.cfg.autonomous.settings()),
        reload_context: Some(std::sync::Arc::new({
            let discovery = cli.discovery();
            move |cwd: &std::path::Path| -> mush_tui::ReloadedContext {
                let project = mush_ext::loader::discover_project_context_with(cwd, &discovery);
                mush_tui::ReloadedContext {
                    templates: mush_ext::discover_templates(cwd),
                    skills: mush_tui::SkillCatalogue {
                        skills: project.skills,
                        diagnostics: project.skill_diagnostics,
                    },
                }
            }
        })),
        mcp: worker.and_then(|worker| worker.mcp.clone()),
        skills: client.skills(),
        autonomous: client.cfg.autonomous.settings(),
        worker_opener: None,
        agent_roster: None,
    }
}

struct SaverThread {
    tx: std::sync::mpsc::Sender<mush_tui::SessionSnapshot>,
    thread: std::thread::JoinHandle<()>,
}

impl SaverThread {
    fn spawn(store: SessionStore, cwd: String) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<mush_tui::SessionSnapshot>();
        let thread = std::thread::Builder::new()
            .name("session-saver".into())
            .spawn(move || {
                let mut writer = SnapshotWriter::new(store, cwd);
                while let Ok(snapshot) = rx.recv() {
                    for snapshot in latest_per_session(snapshot, &rx) {
                        writer.save(snapshot);
                    }
                }
                writer.finish();
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

fn latest_per_session(
    first: mush_tui::SessionSnapshot,
    rx: &std::sync::mpsc::Receiver<mush_tui::SessionSnapshot>,
) -> Vec<mush_tui::SessionSnapshot> {
    let mut latest = vec![first];
    while let Ok(newer) = rx.try_recv() {
        match latest
            .iter_mut()
            .find(|snapshot| snapshot.session_id == newer.session_id)
        {
            Some(slot) => *slot = newer,
            None => latest.push(newer),
        }
    }
    latest
}

struct SnapshotWriter {
    store: SessionStore,
    cwd: String,
    blank: std::collections::HashSet<SessionId>,
}

impl SnapshotWriter {
    fn new(store: SessionStore, cwd: String) -> Self {
        Self {
            store,
            cwd,
            blank: std::collections::HashSet::new(),
        }
    }

    fn save(&mut self, snapshot: mush_tui::SessionSnapshot) {
        let session = session_from_snapshot(snapshot, &self.cwd);
        if session.conversation.is_empty() && session.meta.title.is_none() {
            self.blank.insert(session.meta.id.clone());
        } else {
            self.blank.remove(&session.meta.id);
        }
        let _ = self.store.save(&session);
    }

    fn finish(self) {
        for id in &self.blank {
            let _ = self.store.delete(id);
        }
    }
}

fn fork_session(source: Session, cwd: &str) -> Session {
    let mut forked = Session::new(source.meta.model_id.as_str(), cwd);
    forked.conversation = source.conversation;
    forked.system_prompt = source.system_prompt;
    forked.panes = source.panes;
    forked.meta.rlm_max_depth = source.meta.rlm_max_depth;
    forked.meta.accounts = source.meta.accounts;
    forked.meta.message_count = forked.conversation.context_len();
    forked
}

fn session_from_snapshot(snapshot: mush_tui::SessionSnapshot, cwd: &str) -> Session {
    let mut session = Session::new(&snapshot.model_id, cwd);
    session.meta.id = snapshot.session_id;
    session.meta.title = snapshot.title;
    session.conversation = snapshot.primary;
    session.system_prompt = snapshot.system_prompt;
    session.meta.parent = snapshot.parent;
    session.meta.subagent_model = snapshot.subagent_model;
    session.meta.accounts = snapshot.accounts;
    session.meta.paused = snapshot.paused;
    session.meta.rlm_max_depth = snapshot.rlm_max_depth;
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

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::*;

    #[test]
    fn a_goal_flag_needs_an_objective_for_its_budget() {
        let cli =
            Cli::try_parse_from(["mush", "--goal", "ship it", "--goal-budget", "5000"]).unwrap();
        assert_eq!(cli.goal.as_deref(), Some("ship it"));
        assert_eq!(cli.goal_budget, Some(5000));
        assert!(Cli::try_parse_from(["mush", "--goal-budget", "5000"]).is_err());
    }

    #[test]
    fn sessions_is_not_an_alias_of_list() {
        assert!(matches!(
            Cli::try_parse_from(["mush", "ps"]).unwrap().command,
            Some(Command::List { .. })
        ));
        assert!(Cli::try_parse_from(["mush", "sessions"]).is_err());
    }

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

    #[test]
    fn attach_and_agents_become_a_resume_target() {
        let mut attach = Cli::try_parse_from(["mush", "attach", "abc123"]).unwrap();
        attach.absorb_attach();
        assert!(attach.command.is_none());
        assert!(matches!(attach.resume, Some(Some(id)) if id == "abc123"));

        let mut agents = Cli::try_parse_from(["mush", "agents"]).unwrap();
        agents.absorb_attach();
        assert!(agents.command.is_none());
        assert!(matches!(agents.resume, Some(None)));

        let mut list = Cli::try_parse_from(["mush", "list", "--all"]).unwrap();
        list.absorb_attach();
        assert!(matches!(
            list.command,
            Some(Command::List { all: true, .. })
        ));
    }

    #[test]
    fn stop_rename_and_shutdown_take_json() {
        let stop = Cli::try_parse_from(["mush", "stop", "abc", "--json"]).unwrap();
        assert!(matches!(
            stop.command,
            Some(Command::Stop { json: true, .. })
        ));
        let shutdown = Cli::try_parse_from(["mush", "shutdown", "--json"]).unwrap();
        assert!(matches!(
            shutdown.command,
            Some(Command::Shutdown { json: true, .. })
        ));
        let rename =
            Cli::try_parse_from(["mush", "rename", "--json", "abc", "new", "name"]).unwrap();
        assert!(matches!(
            rename.command,
            Some(Command::Rename { json: true, ref name, .. }) if name == &["new", "name"]
        ));
    }

    fn snapshot(title: Option<&str>) -> mush_tui::SessionSnapshot {
        let conversation = mush_session::ConversationState::from_messages(vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("first question".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )]);
        mush_tui::SessionSnapshot {
            paused: None,
            session_id: mush_ai::types::SessionId::new(),
            title: title.map(str::to_string),
            primary: conversation,
            model_id: "test".into(),
            panes: Vec::new(),
            system_prompt: None,
            parent: None,
            subagent_model: None,
            accounts: Default::default(),
            rlm_max_depth: None,
        }
    }

    #[test]
    fn a_child_prompt_carries_the_system_and_append_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let overrides = setup::PromptOverrides {
            system: Some("base from the flag".into()),
            append: Some("tail from the flag".into()),
        };
        let discovery = mush_ext::DiscoveryOptions {
            no_skills: true,
            no_context_files: true,
            ..Default::default()
        };
        let role = setup::RlmRole::child("worker", 1, 2);

        let prompt = rlm_system_prompt(dir.path(), &discovery, &overrides, &[], &role);

        assert!(prompt.starts_with("base from the flag"), "{prompt}");
        assert!(
            prompt.contains("You are a child agent spawned by"),
            "{prompt}"
        );
        assert!(
            prompt.trim_end().ends_with("tail from the flag"),
            "{prompt}"
        );
    }

    fn child_seed(compaction: CompactionSeed) -> ChildSeed {
        let cfg = config::Config::default();
        let model = models::all_models_with_user().into_iter().next().unwrap();
        ChildSeed {
            models: mush_runtime::ModelSelection::new(model.id.to_string(), None),
            model,
            options: StreamOptions::default(),
            max_turns: 4,
            provider_api_keys: std::collections::HashMap::new(),
            confirm_tools: false,
            forbidden_chars: Default::default(),
            kernel: cfg.kernel.clone(),
            web_search: cfg.web_search.clone(),
            kernel_packages: Vec::new(),
            cwd: std::env::temp_dir(),
            discovery: Default::default(),
            registry: mush_ai::registry::ApiRegistry::new(),
            lifecycle_hooks: Default::default(),
            file_rules: None,
            lsp_diagnostics: None,
            saver: None,
            rlm_max_depth: 2,
            subagent_model: None,
            session_dir: std::env::temp_dir(),
            prompt_overrides: Default::default(),
            compaction,
        }
    }

    #[test]
    fn a_child_compacts_with_the_settings_and_summariser_of_its_session() {
        let summariser = models::all_models_with_user().into_iter().nth(1).unwrap();
        let settings = mush_session::compact::CompactionSettings {
            reserve_tokens: 8_000,
            keep_recent_tokens: 12_000,
        };
        let seed = child_seed(CompactionSeed {
            auto: true,
            settings,
            model: Some((summariser.clone(), StreamOptions::default())),
            agent_callable: true,
        });
        let child = SessionId::new();

        let turn = turn_config(&seed, &child, seed.model.clone(), StreamOptions::default());

        assert!(turn.auto_compact);
        assert_eq!(turn.compaction, settings);
        let (model, options) = turn.compaction_model.unwrap();
        assert_eq!(model.id, summariser.id);
        assert_eq!(options.session_id, Some(child.clone()));
        assert_eq!(turn.options.session_id, Some(child));
    }

    #[test]
    fn a_child_does_not_compact_when_its_session_turned_compaction_off() {
        let seed = child_seed(CompactionSeed {
            auto: false,
            settings: Default::default(),
            model: None,
            agent_callable: false,
        });

        let turn = turn_config(
            &seed,
            &SessionId::new(),
            seed.model.clone(),
            StreamOptions::default(),
        );

        assert!(!turn.auto_compact);
        assert!(turn.compaction_model.is_none());
    }

    #[test]
    fn the_saver_keeps_the_newest_snapshot_of_every_session() {
        let (tx, rx) = std::sync::mpsc::channel();
        let parent = snapshot(Some("parent one"));
        let child = snapshot(Some("child"));
        let mut newer_parent = snapshot(Some("parent two"));
        newer_parent.session_id = parent.session_id.clone();
        tx.send(child).unwrap();
        tx.send(newer_parent).unwrap();

        let saved = latest_per_session(parent, &rx);

        let titles: Vec<_> = saved
            .iter()
            .map(|snapshot| snapshot.title.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(titles, vec!["parent two", "child"]);
    }

    fn blank_snapshot(title: Option<&str>) -> mush_tui::SessionSnapshot {
        let mut snapshot = snapshot(title);
        snapshot.primary = mush_session::ConversationState::new();
        snapshot
    }

    #[test]
    fn a_session_that_never_had_a_turn_is_dropped_when_the_saver_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            SnapshotWriter::new(SessionStore::new(dir.path().to_path_buf()), "/tmp".into());
        let blank = blank_snapshot(None);
        let id = blank.session_id.clone();

        writer.save(blank);
        assert!(writer.store.session_path(&id).exists());
        let store = SessionStore::new(dir.path().to_path_buf());
        writer.finish();

        assert!(!store.session_path(&id).exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_named_or_started_session_outlives_the_saver() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            SnapshotWriter::new(SessionStore::new(dir.path().to_path_buf()), "/tmp".into());
        let named = blank_snapshot(Some("plans"));
        let blank_start = blank_snapshot(None);
        let ids = [named.session_id.clone(), blank_start.session_id.clone()];
        let mut started = snapshot(None);
        started.session_id = blank_start.session_id.clone();
        writer.save(named);
        writer.save(blank_start);
        writer.save(started);

        writer.finish();

        let store = SessionStore::new(dir.path().to_path_buf());
        for id in &ids {
            assert!(store.session_path(id).exists(), "{id} was dropped");
        }
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn a_saved_session_keeps_its_rlm_max_depth() {
        let mut snapshot = snapshot(None);
        snapshot.rlm_max_depth = Some(0);

        let session = session_from_snapshot(snapshot, "/tmp");

        assert_eq!(session.meta.rlm_max_depth, Some(0));
        assert_eq!(fork_session(session, "/tmp").meta.rlm_max_depth, Some(0));
    }

    #[test]
    fn a_saved_session_keeps_the_pause() {
        let mut snapshot = snapshot(None);
        snapshot.paused = Some(mush_session::Pause {
            at: Timestamp::zero(),
            reason: Some("rebuilding".into()),
            by: Some("cli".into()),
        });

        let session = session_from_snapshot(snapshot, "/tmp");

        assert_eq!(
            session
                .meta
                .paused
                .and_then(|pause| pause.reason)
                .as_deref(),
            Some("rebuilding")
        );
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
    fn a_saved_childs_notes_outlive_its_process() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _guard = with_env(&[("MUSH_DATA_DIR", dir.path().to_str())]);
        let id = SessionId::new();

        mush_runtime::Notes::new(child_artifacts(true, &id))
            .keep("resume from step three", None)
            .unwrap();

        let reopened = mush_runtime::Notes::new(child_artifacts(true, &id));
        assert!(reopened.read().contains("resume from step three"));
        assert_eq!(child_artifacts(false, &id), None);
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
