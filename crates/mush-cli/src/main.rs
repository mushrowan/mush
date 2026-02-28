//! mush cli - minimal coding agent

mod config;

use clap::Parser;
use color_eyre::eyre::{Result, eyre};
use futures::StreamExt;

use mush_agent::{AgentConfig, AgentEvent, agent_loop};
use mush_ai::models;
use mush_ai::providers;
use mush_ai::registry::ApiRegistry;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_ext::loader;
use mush_session::{Session, SessionStore};
use mush_tools::builtin_tools;
use mush_tui::TuiConfig;

#[derive(Parser)]
#[command(name = "mush", version, about = "minimal coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// prompt to send (enables print mode, no TUI)
    #[arg(short, long)]
    prompt: Option<String>,

    /// resume a previous session by id
    #[arg(short = 'c', long = "continue")]
    resume: Option<String>,

    /// model id to use
    #[arg(short, long, default_value = "claude-sonnet-4-20250514")]
    model: String,

    /// enable extended thinking
    #[arg(long)]
    thinking: bool,

    /// max output tokens
    #[arg(long)]
    max_tokens: Option<u64>,

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
    /// log in to claude.ai via oauth
    Login,
    /// log out (remove stored oauth credentials)
    Logout,
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
    let prompt = match (cli.prompt.clone(), stdin_prompt) {
        (Some(p), Some(stdin)) => Some(format!("{p}\n\n{stdin}")),
        (Some(p), None) => Some(p),
        (None, Some(stdin)) => Some(stdin),
        (None, None) => None,
    };

    // handle subcommands first
    match cli.command {
        Some(Command::Login) => return login_flow().await,
        Some(Command::Logout) => return logout_flow(),
        None => {}
    }

    match prompt {
        Some(prompt) => print_mode(cli, prompt).await,
        None => tui_mode(cli).await,
    }
}

async fn print_mode(cli: Cli, prompt: String) -> Result<()> {
    let cfg = config::load_config();

    // CLI args override config file
    let model_id = if cli.model != "claude-sonnet-4-20250514" {
        cli.model.clone()
    } else {
        cfg.model.unwrap_or_else(|| cli.model.clone())
    };

    let model = models::find_model_by_id(&model_id).ok_or_else(|| {
        eyre!(
            "unknown model: {model_id}\n\navailable models:\n{}",
            list_models()
        )
    })?;

    let mut registry = ApiRegistry::new();
    providers::register_builtins(&mut registry);

    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.display().to_string();

    let tools = if cli.no_tools {
        vec![]
    } else {
        builtin_tools(cwd.clone())
    };

    let system_prompt = cli
        .system
        .or(cfg.system_prompt)
        .unwrap_or_else(|| build_system_prompt(&cwd));

    let thinking = cli.thinking || cfg.thinking.unwrap_or(false);
    let max_tokens = cli.max_tokens.or(cfg.max_tokens);

    let mut options = StreamOptions {
        thinking: if thinking {
            Some(ThinkingLevel::High)
        } else {
            None
        },
        max_tokens,
        ..Default::default()
    };

    // api key resolution: env > config > oauth
    if options.api_key.is_none() {
        if let Some(ref key) = cfg.api_keys.anthropic
            && std::env::var("ANTHROPIC_API_KEY").is_err()
        {
            options.api_key = Some(key.clone());
        }
        if let Some(ref key) = cfg.api_keys.openrouter
            && std::env::var("OPENROUTER_API_KEY").is_err()
        {
            options.api_key = Some(key.clone());
        }
    }

    // try oauth if no key found and using anthropic
    if options.api_key.is_none()
        && std::env::var("ANTHROPIC_API_KEY").is_err()
        && model.provider == Provider::Anthropic
        && let Ok(Some(token)) = mush_ai::oauth::get_anthropic_oauth_token().await
    {
        options.api_key = Some(token);
    }

    // session: resume or create new
    let store = SessionStore::new(SessionStore::default_dir());
    let mut session = if let Some(ref id) = cli.resume {
        let sid = mush_session::SessionId(id.clone());
        store
            .load(&sid)
            .map_err(|e| eyre!("failed to load session: {e}"))?
    } else {
        Session::new(&model.id, &cwd_str)
    };

    // add the user message
    let user_msg = Message::User(UserMessage {
        content: UserContent::Text(prompt),
        timestamp_ms: timestamp_ms(),
    });
    session.push_message(user_msg);
    session.auto_title();

    let config = AgentConfig {
        model: &model,
        system_prompt: Some(system_prompt),
        tools: &tools,
        registry: &registry,
        options,
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
                let args_summary = summarise_tool_args(&tool_name, &args);
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
            }
            AgentEvent::Error { error } => {
                eprintln!("\x1b[31merror: {error}\x1b[0m");
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

    let model_id = if cli.model != "claude-sonnet-4-20250514" {
        cli.model.clone()
    } else {
        cfg.model.unwrap_or_else(|| cli.model.clone())
    };

    let model = models::find_model_by_id(&model_id).ok_or_else(|| {
        eyre!(
            "unknown model: {model_id}\n\navailable models:\n{}",
            list_models()
        )
    })?;

    let mut registry = ApiRegistry::new();
    providers::register_builtins(&mut registry);

    let cwd = std::env::current_dir()?;
    let tools = if cli.no_tools {
        vec![]
    } else {
        builtin_tools(cwd.clone())
    };

    let system_prompt = cli
        .system
        .or(cfg.system_prompt)
        .unwrap_or_else(|| build_system_prompt(&cwd));

    let thinking = cli.thinking || cfg.thinking.unwrap_or(false);

    let mut options = StreamOptions {
        thinking: if thinking {
            Some(ThinkingLevel::High)
        } else {
            None
        },
        max_tokens: cli.max_tokens.or(cfg.max_tokens),
        ..Default::default()
    };

    if let Some(ref key) = cfg.api_keys.anthropic
        && std::env::var("ANTHROPIC_API_KEY").is_err()
    {
        options.api_key = Some(key.clone());
    }
    if let Some(ref key) = cfg.api_keys.openrouter
        && std::env::var("OPENROUTER_API_KEY").is_err()
    {
        options.api_key = Some(key.clone());
    }

    // try oauth if no key found and using anthropic
    if options.api_key.is_none()
        && std::env::var("ANTHROPIC_API_KEY").is_err()
        && model.provider == Provider::Anthropic
        && let Ok(Some(token)) = mush_ai::oauth::get_anthropic_oauth_token().await
    {
        options.api_key = Some(token);
    }

    let tui_config = TuiConfig {
        model,
        system_prompt: Some(system_prompt),
        options,
    };

    mush_tui::run_tui(tui_config, &tools, &registry)
        .await
        .map_err(|e| eyre!("TUI error: {e}"))?;

    Ok(())
}

fn summarise_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "read" | "write" | "edit" => args["path"].as_str().unwrap_or("").to_string(),
        "bash" => {
            let cmd = args["command"].as_str().unwrap_or("");
            if cmd.len() > 80 {
                format!("{}...", &cmd[..77])
            } else {
                cmd.to_string()
            }
        }
        "grep" => {
            let pattern = args["pattern"].as_str().unwrap_or("");
            let path = args["path"].as_str().unwrap_or(".");
            format!("{pattern} in {path}")
        }
        "find" => args["pattern"].as_str().unwrap_or("").to_string(),
        "ls" => args["path"].as_str().unwrap_or(".").to_string(),
        _ => format!("{args}"),
    }
}

fn build_system_prompt(cwd: &std::path::Path) -> String {
    let cwd_str = cwd.display();
    let mut prompt = format!(
        "You are a coding assistant. You help users by reading files, executing commands, \
         editing code, and writing new files.\n\n\
         Current working directory: {cwd_str}\n\n\
         Guidelines:\n\
         - Use bash for file operations like ls, grep, find\n\
         - Use read to examine files before editing\n\
         - Use edit for precise changes (old text must match exactly)\n\
         - Use write only for new files or complete rewrites\n\
         - Be concise in your responses"
    );

    // append AGENTS.md context
    let context = loader::discover_project_context(cwd);
    for agents in &context.agents_md {
        prompt.push_str("\n\n# Project Context\n\n");
        prompt.push_str(&agents.content);
    }

    // append available skills
    if !context.skills.is_empty() {
        prompt.push_str("\n\nThe following skills are available. ");
        prompt.push_str(
            "Use the read tool to load a skill's file when the task matches its description.\n\n",
        );
        for skill in &context.skills {
            prompt.push_str(&format!(
                "- **{}**: {} ({})\n",
                skill.name,
                skill.description,
                skill.path.display()
            ));
        }
    }

    prompt
}

fn list_models() -> String {
    models::all_models()
        .iter()
        .map(|m| format!("  {} ({})", m.id, m.provider))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn login_flow() -> Result<()> {
    use mush_ai::oauth;

    eprintln!("logging in to claude.ai...\n");

    let pkce = oauth::generate_pkce();
    let url = oauth::build_auth_url(&pkce);

    eprintln!("open this URL in your browser:\n");
    eprintln!("  {url}\n");
    eprintln!("after authorising, paste the code here (format: code#state):");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.is_empty() {
        return Err(eyre!("no code provided"));
    }

    let creds = oauth::exchange_code(input, &pkce.verifier)
        .await
        .map_err(|e| eyre!("login failed: {e}"))?;

    let mut store =
        oauth::load_credentials().map_err(|e| eyre!("failed to load credentials: {e}"))?;
    store.providers.insert("anthropic".into(), creds);
    oauth::save_credentials(&store).map_err(|e| eyre!("failed to save credentials: {e}"))?;

    eprintln!("\n\x1b[32m✓ logged in to claude.ai\x1b[0m");
    Ok(())
}

fn logout_flow() -> Result<()> {
    use mush_ai::oauth;

    let mut store =
        oauth::load_credentials().map_err(|e| eyre!("failed to load credentials: {e}"))?;

    if store.providers.remove("anthropic").is_some() {
        oauth::save_credentials(&store).map_err(|e| eyre!("failed to save credentials: {e}"))?;
        eprintln!("\x1b[32m✓ logged out from claude.ai\x1b[0m");
    } else {
        eprintln!("not logged in");
    }

    Ok(())
}

fn timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
