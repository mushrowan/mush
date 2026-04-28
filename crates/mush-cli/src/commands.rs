//! subcommand handlers for the mush CLI

use color_eyre::eyre::{Result, eyre};

use mush_ai::models;
use mush_ai::types::CacheRetention;
use mush_session::SessionStore;

use crate::config;
use crate::setup::default_model_id;

pub fn list_models() -> Result<()> {
    for m in models::all_models_with_user() {
        let cost = format!("${:.2}/${:.2} per 1M tokens", m.cost.input, m.cost.output);
        println!("  \x1b[1m{}\x1b[0m ({})", m.id, m.provider);
        println!(
            "    context: {}k, max output: {}, {cost}",
            m.context_window.get() / 1000,
            m.max_output_tokens
        );
    }
    Ok(())
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub fn list_sessions() -> Result<()> {
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
        let age = format_age(&meta.updated_at);
        let id = short_id(&meta.id);
        println!(
            "  \x1b[2m{}\x1b[0m  {} \x1b[2m({}, {} msgs, {})\x1b[0m",
            id, title, meta.model_id, meta.message_count, age,
        );
    }
    println!("\nresume with: mush -c <id>");
    Ok(())
}

pub fn delete_session(id: &str) -> Result<()> {
    let store = SessionStore::new(SessionStore::default_dir());
    let sessions = store
        .list()
        .map_err(|e| eyre!("failed to list sessions: {e}"))?;

    let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();

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
                let id = short_id(&s.id);
                eprintln!("  {id} - {title}");
            }
            Err(eyre!("ambiguous prefix, be more specific"))
        }
    }
}

pub fn open_config() -> Result<()> {
    let path = config::config_path();

    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
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
             # system_prompt = \"\"\n\
             \n\
             # [keys]\n\
             # edit_steering = [\"alt+k\", \"up\", \"ctrl+k\"]  # lift queued steering msg\n\
             # cycle_favourite = \"alt+m\"          # next favourite model\n\
             # cycle_favourite_backward = \"alt+shift+m\"  # prev favourite model\n\
             # enter_search = \"ctrl+f\"            # enter fullscreen search\n\
             # enter_scroll = \"ctrl+s\"            # enter scroll / selection mode\n\
             \n\
             # [status_bar]\n\
             # show_thinking = true          # `thinking: <level>` segment\n\
             # show_context = true           # `used/window` + cache warmth\n\
             # show_oauth_usage = true       # 5h / 7d usage bars (claude code plan)\n\
             # show_status_messages = true   # ephemeral slash / tool messages\n\
             # show_scroll_position = true   # `N%` when scrolled up\n\
             # show_background_alerts = true # background pane notifications\n\
             # show_pane_indicator = true    # [i/n] when multi-pane\n\
             # show_cwd = true               # cwd in the right column\n\
             \n\
             # [terminal]\n\
             # keyboard_enhancement = \"auto\"  # auto | enabled | disabled\n\
             # mouse_tracking = \"minimal\"     # minimal | disabled\n\
             # image_probe = \"auto\"           # auto | disabled\n\
             # env overrides: MUSH_TUI_KEYBOARD_ENHANCEMENT, MUSH_TUI_MOUSE_TRACKING, MUSH_TUI_IMAGE_PROBE\n\
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

pub fn status(terminal_overrides: mush_tui::TerminalPolicyOverrides) -> Result<()> {
    let cfg = config::load_config();
    let terminal = cfg.terminal.with_overrides(terminal_overrides);

    println!("\x1b[1mmush status\x1b[0m\n");

    // config
    let config_path = config::config_dir().join("config.toml");
    if config_path.exists() {
        println!("  config: {}", config_path.display());
    } else {
        println!("  config: \x1b[2m(none, using defaults)\x1b[0m");
    }

    // model
    let cwd = mush_tui::canonical_dir(&std::env::current_dir().unwrap_or_default());
    let model_id = default_model_id(&cfg, &cwd);
    println!("  model:  {model_id}");

    // thinking
    let thinking = cfg
        .thinking
        .map(|l| format!("{l:?}").to_lowercase())
        .unwrap_or_else(|| "off".into());
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
    println!("  debug cache: {}", cfg.debug_cache);
    println!("  cache timer: {}", cfg.cache_timer);
    println!(
        "  keyboard enhancement: {}",
        terminal.keyboard_enhancement.as_str()
    );
    println!("  mouse tracking: {}", terminal.mouse_tracking.as_str());
    println!("  image probe: {}", terminal.image_probe.as_str());

    #[cfg(feature = "embeddings")]
    println!("  embeddings: enabled");
    #[cfg(not(feature = "embeddings"))]
    println!("  embeddings: disabled");

    // auth status
    println!("\n\x1b[1mauth\x1b[0m\n");

    let has_anthropic_env = std::env::var("ANTHROPIC_API_KEY").is_ok();
    let has_openrouter_env = std::env::var("OPENROUTER_API_KEY").is_ok();
    let has_openai_env = std::env::var("OPENAI_API_KEY").is_ok();
    let has_anthropic_cfg = cfg.api_keys.anthropic.is_some();
    let has_openrouter_cfg = cfg.api_keys.openrouter.is_some();
    let has_openai_cfg = cfg.api_keys.openai.is_some();

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

    // additional openai-compatible providers. env vars follow the
    // convention `{PROVIDER}_API_KEY` (matches `env_api_key` in mush-ai)
    for provider in [
        "groq",
        "deepseek",
        "xai",
        "cerebras",
        "mistral",
        "together",
        "deepinfra",
    ] {
        let env_name = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
        let has_env = std::env::var(&env_name).is_ok();
        let has_cfg = cfg.api_keys.get(provider).is_some();
        print!("  {provider:<13}");
        if has_env {
            println!("\x1b[32m✓ env var\x1b[0m");
        } else if has_cfg {
            println!("\x1b[32m✓ config file\x1b[0m");
        } else {
            println!("\x1b[2m- not configured\x1b[0m");
        }
    }

    // sessions
    let store = SessionStore::new(SessionStore::default_dir());
    let session_count = store.list().map(|s| s.len()).unwrap_or(0);
    println!("\n\x1b[1msessions\x1b[0m\n");
    println!("  {session_count} saved");

    Ok(())
}

pub async fn login(provider_id: Option<String>) -> Result<()> {
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

    let creds = if provider_id == "openai-codex" {
        login_codex_with_local_server().await?
    } else {
        login_via_paste(provider.as_ref()).await?
    };

    let mut store =
        oauth::load_credentials().map_err(|e| eyre!("failed to load credentials: {e}"))?;
    store.providers.insert(provider_id, creds);
    oauth::save_credentials(&store).map_err(|e| eyre!("failed to save credentials: {e}"))?;

    eprintln!("\n\x1b[32m✓ logged in to {}\x1b[0m", provider.name());
    Ok(())
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

async fn login_codex_with_local_server() -> Result<mush_ai::oauth::OAuthCredentials> {
    use mush_ai::oauth::openai_codex::CodexLoginSession;

    let session = CodexLoginSession::start()
        .await
        .map_err(|e| eyre!("failed to start codex login server: {e}"))?;

    eprintln!("open this URL in your browser:\n");
    eprintln!("  {}\n", session.auth_url);
    eprintln!("waiting for the browser callback...");

    session
        .await_credentials()
        .await
        .map_err(|e| eyre!("login failed: {e}"))
}

pub fn logout(provider_id: Option<String>) -> Result<()> {
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

fn format_age(ts: &mush_ai::types::Timestamp) -> String {
    let age = ts.age_display();
    if age == "now" {
        "just now".into()
    } else {
        format!("{age} ago")
    }
}
