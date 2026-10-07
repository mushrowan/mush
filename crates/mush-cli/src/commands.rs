use color_eyre::eyre::{Result, eyre};

use mush_ai::types::CacheRetention;
use mush_session::SessionStore;

use crate::config;
use crate::output::Style;
use crate::setup::default_model_id;

pub fn list_models() -> Result<()> {
    for m in mush_ai::discovery::merged_models() {
        let cost = format!("${:.2}/${:.2} per 1M tokens", m.cost.input, m.cost.output);
        println!("  {} ({})", Style::Bold.stdout(m.id), m.provider);
        println!(
            "    context: {}k, max output: {}, {cost}",
            m.context_window.get() / 1000,
            m.max_output_tokens
        );
    }
    Ok(())
}

pub fn delete_session(id: &str) -> Result<()> {
    let store = SessionStore::new(SessionStore::default_dir());
    let session = store
        .resolve(id)
        .map_err(|e| eyre!("failed to resolve session: {e}"))?;
    let title = session.title.as_deref().unwrap_or("(untitled)");
    store
        .delete(&session.id)
        .map_err(|e| eyre!("failed to delete: {e}"))?;
    eprintln!(
        "{}",
        Style::Green.stderr(format_args!("✓ deleted session: {title}"))
    );
    Ok(())
}

pub fn slim_sessions(id: Option<&str>, all: bool, apply: bool) -> Result<()> {
    let store = SessionStore::new(SessionStore::default_dir());
    let sessions = match (id, all) {
        (Some(id), false) => vec![
            store
                .resolve(id)
                .map_err(|e| eyre!("failed to resolve session: {e}"))?,
        ],
        (None, true) => store
            .list()
            .map_err(|e| eyre!("failed to list sessions: {e}"))?,
        _ => return Err(eyre!("name one session or pass --all")),
    };
    let megabytes = |bytes: u64| bytes as f64 / 1_000_000.0;
    let (mut before, mut after) = (0, 0);
    for session in sessions {
        let title = session.title.as_deref().unwrap_or("(untitled)");
        match store.slim_history(&session.id, apply) {
            Ok(report) if report.capped > 0 => {
                before += report.before;
                after += report.after;
                println!(
                    "{title} ({}): {:.1}MB -> {:.1}MB, {} tool results capped",
                    session.id,
                    megabytes(report.before),
                    megabytes(report.after),
                    report.capped
                );
            }
            Ok(_) => {}
            Err(mush_session::StoreError::Busy(_)) => {
                eprintln!(
                    "skipped {title} ({}): open in a worker, stop it first",
                    session.id
                );
            }
            Err(error) => eprintln!("skipped {title} ({}): {error}", session.id),
        }
    }
    let verb = if apply { "saved" } else { "would save" };
    println!("{verb} {:.1}MB", megabytes(before - after));
    if !apply && before > after {
        println!("dry run, rerun with --apply to rewrite");
    }
    Ok(())
}

pub fn open_config() -> Result<()> {
    let path = config::config_path();

    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        mush_ai::private_io::write_atomic(
            &path,
            "# mush configuration\n\
             # model = \"gpt-5.4\"  # optional, auto-default depends on auth\n\
             # thinking = false\n\
             # max_tokens = 16384\n\
             # max_turns = 200  # default: unlimited\n\
             # cache_retention = \"short\"  # none | short | long\n\
             # debug_cache = false\n\
             # show_cost = false  # toggle with /cost\n\
             # show_usage_lines = false  # render usage lines under each message\n\
             # show_token_counters = false  # ↑/↓/R/W token counters in status bar\n\
             # cache_timer = true  # cache warmth countdown + notifications (on by default)\n\
             # feature_hints = true  # dim tips above the composer while the agent works\n\
             # system_prompt = \"\"\n\
             \n\
             # [keys]\n\
             # edit_steering = [\"alt+k\", \"up\", \"ctrl+k\"]  # lift queued steering msg\n\
             # cycle_favourite = \"alt+m\"          # next favourite model\n\
             # cycle_favourite_backward = \"alt+shift+m\"  # prev favourite model\n\
             # enter_search = \"ctrl+f\"            # enter fullscreen search\n\
             \n\
             # [status_bar]\n\
             # show_thinking = true          # `thinking: <level>` segment\n\
             # show_context = true           # `used/window` + cache warmth\n\
             # show_oauth_usage = true       # 5h / 7d usage bars (claude code plan)\n\
             # show_status_messages = true   # ephemeral slash / tool messages\n\
             # show_background_alerts = true # background pane notifications\n\
             # show_pane_indicator = true    # [i/n] when multi-pane\n\
             # show_cwd = true               # cwd in the right column\n\
             \n\
             # [terminal]\n\
             # keyboard_enhancement = \"auto\"  # auto | enabled | disabled\n\
             # image_probe = \"auto\"           # auto | disabled\n\
             # env overrides: MUSH_TUI_KEYBOARD_ENHANCEMENT, MUSH_TUI_IMAGE_PROBE\n\
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

pub async fn status(terminal_overrides: mush_tui::TerminalPolicyOverrides) -> Result<()> {
    let cfg = config::load_config();
    let terminal = cfg.terminal.with_overrides(terminal_overrides);

    println!("{}\n", Style::Bold.stdout("mush status"));

    let config_path = config::config_dir().join("config.toml");
    if config_path.exists() {
        println!("  config: {}", config_path.display());
    } else {
        println!("  config: {}", Style::Dim.stdout("(none, using defaults)"));
    }

    let cwd = mush_tui::canonical_dir(&std::env::current_dir().unwrap_or_default());
    let model_id = default_model_id(&cfg, &cwd);
    println!("  model:  {model_id}");

    let thinking = cfg
        .thinking
        .map(|l| format!("{l:?}").to_lowercase())
        .unwrap_or_else(|| "off".into());
    println!("  thinking: {thinking}");

    let max_turns = cfg.max_turns.unwrap_or(mush_agent::DEFAULT_MAX_TURNS);
    println!("  max turns: {max_turns}");

    let cache_retention = cfg.cache_retention.unwrap_or(CacheRetention::Short);
    let cache_retention = match cache_retention {
        CacheRetention::None => "none",
        CacheRetention::Short => "short",
        CacheRetention::Long => "long",
    };
    println!("  cache retention: {cache_retention}");
    println!("  debug cache: {}", cfg.debug_cache);
    println!("  cache timer: {}", cfg.cache_timer);
    println!(
        "  keyboard enhancement: {}",
        terminal.keyboard_enhancement.as_str()
    );
    println!("  image probe: {}", terminal.image_probe.as_str());

    println!("\n{}\n", Style::Bold.stdout("auth"));

    let key_source = |env: &str, configured: bool| {
        if std::env::var(env).is_ok() {
            Some("env var")
        } else if configured {
            Some("config file")
        } else {
            None
        }
    };
    let oauth_store = mush_ai::oauth::load_credentials().unwrap_or_default();
    let oauth = |provider: &str| oauth_store.has_provider(provider).then_some("oauth");
    let not_configured = Style::Dim.stdout("- not configured");

    print_auth(
        "anthropic:   ",
        key_source("ANTHROPIC_API_KEY", cfg.api_keys.anthropic.is_some())
            .or_else(|| oauth("anthropic")),
        Style::Red.stdout("✗ not configured"),
    );
    print_auth(
        "openrouter:  ",
        key_source("OPENROUTER_API_KEY", cfg.api_keys.openrouter.is_some()),
        &not_configured,
    );
    print_auth(
        "openai:      ",
        key_source("OPENAI_API_KEY", cfg.api_keys.openai.is_some()),
        &not_configured,
    );
    print_auth("openai-codex: ", oauth("openai-codex"), &not_configured);

    let store = SessionStore::new(SessionStore::default_dir());
    let session_count = store.list().map(|s| s.len()).unwrap_or(0);
    println!("\n{}\n", Style::Bold.stdout("sessions"));
    println!("  {session_count} saved");

    println!("\n{}\n", Style::Bold.stdout("daemon"));
    for line in crate::doctor::report(&crate::doctor::findings(false).await?).lines() {
        println!("  {line}");
    }

    Ok(())
}

fn print_auth(label: &str, source: Option<&str>, missing: impl std::fmt::Display) {
    match source {
        Some(source) => println!(
            "  {label}{}",
            Style::Green.stdout(format_args!("✓ {source}"))
        ),
        None => println!("  {label}{missing}"),
    }
}

pub async fn login(
    provider_id: Option<String>,
    account: Option<mush_ai::types::AccountLabel>,
) -> Result<()> {
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

    let creds = match login_via_browser(provider.as_ref()).await {
        Ok(creds) => creds,
        Err(e) => {
            eprintln!("browser login unavailable ({e})\nfalling back to paste-the-code\n");
            login_via_paste(provider.as_ref()).await?
        }
    };

    let label = oauth::save_login(&provider_id, account, creds)
        .map_err(|e| eyre!("failed to save credentials: {e}"))?;

    eprintln!(
        "\n{}",
        Style::Green.stderr(format_args!(
            "✓ logged in to {} as {label}",
            provider.name()
        ))
    );

    eprintln!("refreshing model catalogue...");
    let client = mush_ai::http::client();
    let summary = mush_ai::discovery::refresh_and_save(client).await;
    eprintln!("model discovery: {}", summary.one_line());

    Ok(())
}

async fn login_via_browser(
    provider: &dyn mush_ai::oauth::OAuthProvider,
) -> Result<mush_ai::oauth::OAuthCredentials> {
    let login = provider
        .begin_browser_login()
        .await
        .map_err(|e| eyre!("failed to start the local callback server: {e}"))?;

    if open::that_detached(&login.auth_url).is_ok() {
        eprintln!("opened this URL in your browser:\n");
    } else {
        eprintln!("open this URL in your browser:\n");
    }
    eprintln!("  {}\n", login.auth_url);
    eprintln!("waiting for the browser callback...");

    login
        .await_credentials()
        .await
        .map_err(|e| eyre!("login failed: {e}"))
}

async fn login_via_paste(
    provider: &dyn mush_ai::oauth::OAuthProvider,
) -> Result<mush_ai::oauth::OAuthCredentials> {
    let (prompt, pkce) = provider
        .begin_login()
        .map_err(|e| eyre!("failed to start oauth login: {e}"))?;
    eprintln!("open this URL in your browser:\n");
    eprintln!("  {}\n", prompt.url);
    eprintln!("{}", prompt.instructions);

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();
    if input.is_empty() {
        return Err(eyre!("no code provided"));
    }

    provider
        .exchange_code(input, &pkce)
        .await
        .map_err(|e| eyre!("login failed: {e}"))
}

pub fn logout(
    provider_id: Option<String>,
    account: Option<&mush_ai::types::AccountLabel>,
) -> Result<()> {
    use mush_ai::oauth;

    let provider_id = provider_id.unwrap_or_else(|| "anthropic".into());

    match oauth::remove_credentials(&provider_id, account)
        .map_err(|e| eyre!("failed to update credentials: {e}"))?
    {
        Some(label) => eprintln!(
            "{}",
            Style::Green.stderr(format_args!("✓ logged out of {provider_id} ({label})"))
        ),
        None => match account {
            Some(label) => eprintln!("no {provider_id} account called {label}"),
            None => eprintln!("not logged in to {provider_id}"),
        },
    }

    Ok(())
}

pub fn accounts(
    provider_id: Option<String>,
    default: Option<&mush_ai::types::AccountLabel>,
) -> Result<()> {
    use mush_ai::oauth;

    let provider_id = provider_id.unwrap_or_else(|| "anthropic".into());

    if let Some(label) = default {
        if !oauth::set_default_account(&provider_id, label)
            .map_err(|e| eyre!("failed to update credentials: {e}"))?
        {
            return Err(eyre!("no {provider_id} account called {label}"));
        }
        eprintln!(
            "{}",
            Style::Green.stderr(format_args!(
                "✓ new sessions use the {label} {provider_id} account"
            ))
        );
        return Ok(());
    }

    let store = oauth::load_credentials().map_err(|e| eyre!("failed to load credentials: {e}"))?;
    let Some(accounts) = store.accounts(&provider_id) else {
        eprintln!(
            "no {provider_id} accounts, add one with: mush login {provider_id} --account <label>"
        );
        return Ok(());
    };
    let default = accounts.default_label();
    for label in accounts.labels() {
        if Some(label) == default {
            println!("{label} (default)");
        } else {
            println!("{label}");
        }
    }
    Ok(())
}
