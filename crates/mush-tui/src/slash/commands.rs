//! slash command execution
//!
//! parse types live in the parent module, compaction in `compaction`

use std::fmt::Write;

use mush_ai::models;
use mush_ai::types::*;
use mush_session::ConversationState;

use crate::TuiConfig;
use crate::app::App;
use crate::runner::HintMode;

use super::SlashAction;

/// handle a slash command, returning Some(prompt) if it should trigger the agent
pub fn handle(
    app: &mut App,
    conversation: &mut ConversationState,
    tui_config: &mut TuiConfig,
    thinking_prefs: &std::collections::HashMap<String, ThinkingLevel>,
    action: &SlashAction,
) -> Option<String> {
    match action {
        SlashAction::Help => {
            let mut help = String::from("available commands:\n");
            help.push_str("  /help          - show this message\n");
            help.push_str("  /keys          - show keyboard shortcuts\n");
            help.push_str("  /new           - save session, start fresh\n");
            help.push_str("  /model [id]    - show or switch model\n");
            help.push_str("  /sessions      - browse and resume sessions\n");
            help.push_str("  /branch [n]    - branch from nth user message\n");
            help.push_str("  /tree          - show conversation tree\n");
            help.push_str("  /compact       - summarise old messages to free context\n");
            help.push_str("  /fork-compact  - fork branch then compact (preserves original)\n");
            help.push_str("  /export [file] - save conversation as markdown\n");
            help.push_str("  /undo          - revert last turn\n");
            help.push_str("  /search [text] - search conversation (or ctrl+f)\n");
            help.push_str("  /cost          - show session cost\n");
            help.push_str("  /logs [n]      - show last n log entries (default 50)\n");
            help.push_str("  /injection     - toggle prompt injection preview\n");
            help.push_str("  /settings      - view/modify runtime settings (betas, scope)\n");
            help.push_str("  /login [p]     - oauth login to provider (default anthropic)\n");
            help.push_str("  /login-complete <code> - finish the oauth flow with the code\n");
            help.push_str("  /close         - close focused pane\n");
            help.push_str("  /broadcast msg - send a message to all panes\n");
            help.push_str("  /lock <path>   - lock a file for this pane\n");
            help.push_str("  /unlock <path> - release a file lock\n");
            help.push_str("  /locks         - list all file locks\n");
            help.push_str("  /label [text]  - set pane label (or auto-generate)\n");
            help.push_str("  /panes         - list all panes with status\n");
            help.push_str("  /card          - show agent capability card\n");
            help.push_str("  /task claim/release/list - cross-agent task locking\n");
            help.push_str("  /quit          - exit mush\n");
            help.push_str("\ntip: type a prompt template name (e.g. /review file.rs) to expand it");
            app.push_system_message(help);
            None
        }
        SlashAction::Keys => {
            let mut keys = String::from("keyboard shortcuts:\n\n");
            keys.push_str("general:\n");
            keys.push_str("  enter          - send message\n");
            keys.push_str("  alt/shift+enter - insert newline\n");
            keys.push_str("  ctrl+c         - quit\n");
            keys.push_str("  esc            - abort stream / scroll to bottom\n");
            keys.push_str("  tab            - autocomplete / command menu\n");
            keys.push_str("  ctrl+j/k       - navigate menus\n");
            keys.push_str("  ctrl+v         - paste image from clipboard\n");
            keys.push_str("  ctrl+d         - quit (empty input) / delete char\n");
            keys.push_str("  page up/down   - scroll\n");
            keys.push_str("  alt+k          - edit queued steering message\n");
            keys.push_str("\nmode switches:\n");
            keys.push_str("  ctrl+s         - scroll/copy mode\n");
            keys.push_str("  ctrl+f         - search\n");
            keys.push_str("  ctrl+t         - cycle thinking level\n");
            keys.push_str("  ctrl+o         - toggle thinking text visibility\n");
            keys.push_str("  ctrl+i         - toggle prompt injection preview\n");
            keys.push_str("\nscroll mode:\n");
            keys.push_str("  j/k            - scroll down/up\n");
            keys.push_str("  g/G            - jump to top/bottom\n");
            keys.push_str("  b              - toggle block/message mode\n");
            keys.push_str("  v              - visual selection\n");
            keys.push_str("  y              - copy selected message\n");
            keys.push_str("  click/drag     - select messages with mouse\n");
            keys.push_str("  esc            - exit scroll mode\n");
            keys.push_str("\nediting:\n");
            keys.push_str("  ctrl+a / home  - start of line\n");
            keys.push_str("  ctrl+e / end   - end of line\n");
            keys.push_str("  ctrl+w         - delete word backward\n");
            keys.push_str("  alt+d          - delete word forward\n");
            keys.push_str("  ctrl+u         - delete to start\n");
            keys.push_str("  ctrl+k         - delete to end\n");
            keys.push_str("  alt+b / alt+←  - word left\n");
            keys.push_str("  alt+f / alt+→  - word right\n");
            keys.push_str("\npanes:\n");
            keys.push_str("  ctrl+shift+enter - fork into new pane\n");
            keys.push_str("  ctrl+tab         - next pane\n");
            keys.push_str("  ctrl+shift+tab   - previous pane\n");
            keys.push_str("  alt+1..9         - focus pane by number");
            app.push_system_message(keys);
            None
        }
        SlashAction::Clear => {
            app.clear_messages();
            conversation.replace_messages(vec![]);
            app.status = Some("conversation cleared".into());
            None
        }
        SlashAction::New => {
            // handled in runner/commands.rs where save_session is available
            None
        }
        SlashAction::Tree => {
            show_tree(app, conversation);
            None
        }
        SlashAction::Branch { index: Some(index) } => {
            handle_branch(app, conversation, *index);
            None
        }
        SlashAction::Branch { index: None } => {
            app.push_system_message(
                "usage: /branch <n> — branch from nth user message\ntry /tree to see messages",
            );
            None
        }
        SlashAction::Sessions => {
            let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
            match store.list() {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        app.push_system_message("no saved sessions");
                    } else {
                        let cwd = std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        app.open_session_picker(sessions, cwd);
                    }
                }
                Err(e) => app.push_system_message(format!("failed to list sessions: {e}")),
            }
            None
        }
        SlashAction::Resume { session_id } => {
            let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
            match store.load(session_id) {
                Ok(session) => {
                    *conversation = session.conversation;
                    rebuild_display(app, &conversation.context());
                    tui_config.session_id = session_id.clone();
                    let title = session.meta.title.as_deref().unwrap_or("untitled");
                    app.status = Some(format!("resumed: {title}"));
                }
                Err(e) => app.push_system_message(format!("failed to load session: {e}")),
            }
            None
        }
        SlashAction::Model { model_id: None } => {
            app.push_system_message(format!("model: {}", app.model_id));
            None
        }
        SlashAction::Model {
            model_id: Some(model_id),
        } => {
            handle_model_switch(app, tui_config, thinking_prefs, model_id);
            None
        }
        SlashAction::Search { query } => {
            app.interaction.mode = crate::app::AppMode::Search;
            app.interaction.search.query = query.clone();
            app.update_search();
            None
        }
        SlashAction::Cost => {
            app.interaction.show_cost = !app.interaction.show_cost;
            show_cost(app);
            None
        }
        SlashAction::Logs { count } => {
            if let Some(ref buf) = tui_config.log_buffer {
                let entries = buf(*count);
                if entries.is_empty() {
                    app.push_system_message("no log entries yet");
                } else {
                    app.push_system_message(entries.join("\n"));
                }
            } else {
                app.push_system_message("logging not available");
            }
            None
        }
        SlashAction::Injection => {
            app.interaction.show_prompt_injection = !app.interaction.show_prompt_injection;
            let mode = match tui_config.hint_mode {
                HintMode::Message => "message",
                HintMode::Transform => "transform",
                HintMode::None => "none",
            };
            let enricher = if tui_config.prompt_enricher.is_some() {
                "ready"
            } else {
                "unavailable"
            };
            app.push_system_message(format!(
                "prompt injection preview: {} (mode: {mode}, enricher: {enricher})",
                if app.interaction.show_prompt_injection {
                    "on"
                } else {
                    "off"
                }
            ));
            None
        }
        SlashAction::Settings { args } => {
            handle_settings(app, tui_config, args);
            None
        }
        SlashAction::Login { provider } => {
            handle_login_start(app, provider.as_deref());
            None
        }
        SlashAction::LoginComplete { code: _ } => {
            // async completion is handled in runner/commands.rs after this
            // returns. we just acknowledge here so the slash menu closes
            None
        }
        SlashAction::Undo => {
            handle_undo(app, conversation);
            None
        }
        SlashAction::Card => {
            if let Some(card) = &tui_config.agent_card {
                app.push_system_message(format!("agent card:\n```json\n{}\n```", card.to_json()));
            } else {
                app.push_system_message("no agent card available");
            }
            None
        }
        SlashAction::TaskClaim { .. } | SlashAction::TaskRelease { .. } | SlashAction::TaskList => {
            handle_task(app, &tui_config.cwd, action);
            None
        }
        SlashAction::Quit => {
            app.should_quit = true;
            None
        }
        SlashAction::Other { name, args } => try_template(app, name, args),
        _ => None,
    }
}

fn handle_task(app: &mut App, cwd: &std::path::Path, action: &SlashAction) {
    let store = mush_agent::TaskStore::new(cwd);
    let agent_id = std::process::id().to_string();

    match action {
        SlashAction::TaskClaim { id, description } => {
            let lock = mush_agent::TaskLock {
                id: id.clone(),
                description: description.clone(),
                agent: agent_id,
                claimed_at: unix_timestamp(),
                files: vec![],
            };
            match store.claim(&lock) {
                Ok(()) => app.push_system_message(format!("claimed task: {id}")),
                Err(e) => app.push_system_message(format!("failed: {e}")),
            }
        }
        SlashAction::TaskRelease { id } => match store.release(id, &agent_id) {
            Ok(()) => app.push_system_message(format!("released task: {id}")),
            Err(e) => app.push_system_message(format!("failed: {e}")),
        },
        SlashAction::TaskList => match store.list() {
            Ok(tasks) if tasks.is_empty() => app.push_system_message("no active tasks"),
            Ok(tasks) => {
                let mut msg = String::from("active tasks:\n");
                for t in &tasks {
                    writeln!(msg, "  {} (agent {}) - {}", t.id, t.agent, t.description).ok();
                }
                app.push_system_message(msg.trim_end().to_string());
            }
            Err(e) => app.push_system_message(format!("failed: {e}")),
        },
        _ => {}
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn show_tree(app: &mut App, conversation: &ConversationState) {
    let tree = conversation.tree();
    let user_msgs: Vec<_> = tree
        .current_branch()
        .into_iter()
        .filter(|e| {
            matches!(
                e.kind,
                mush_session::tree::EntryKind::Message {
                    message: Message::User(_)
                }
            )
        })
        .collect();

    if user_msgs.is_empty() {
        app.push_system_message("no messages yet");
        return;
    }

    let mut info = format!(
        "tree: {} entries, {} branch points\n",
        tree.len(),
        tree.entries()
            .iter()
            .filter(|e| tree.is_branch_point(&e.id))
            .count()
    );
    for (i, entry) in user_msgs.iter().enumerate() {
        if let mush_session::tree::EntryKind::Message {
            message: Message::User(u),
        } = &entry.kind
        {
            let text = match &u.content {
                UserContent::Text(t) => t.as_str(),
                _ => "(parts)",
            };
            let preview = if text.chars().count() > 60 {
                let truncated: String = text.chars().take(57).collect();
                format!("{truncated}...")
            } else {
                text.to_string()
            };
            let marker = if tree.is_branch_point(&entry.id) {
                " ⑂"
            } else {
                ""
            };
            info.push_str(&format!("  {}: {preview}{marker}\n", i + 1));
        }
    }
    app.push_system_message(info);
}

fn handle_branch(app: &mut App, conversation: &mut ConversationState, n: usize) {
    let user_msgs = conversation.user_messages_in_branch();
    let count = user_msgs.len();
    let target_info = user_msgs.get(n.wrapping_sub(1)).map(|e| {
        let parent = e.parent_id.clone();
        let preview = match &e.kind {
            mush_session::tree::EntryKind::Message {
                message: Message::User(u),
            } => match &u.content {
                UserContent::Text(t) if t.chars().count() > 40 => {
                    let truncated: String = t.chars().take(37).collect();
                    format!("{truncated}...")
                }
                UserContent::Text(t) => t.clone(),
                _ => "message".into(),
            },
            _ => "message".into(),
        };
        (parent, preview)
    });
    drop(user_msgs);

    if n == 0 || n > count {
        app.push_system_message(format!(
            "invalid: use 1-{count} (try /tree to see messages)"
        ));
    } else if let Some((parent_id, preview)) = target_info {
        if let Some(ref pid) = parent_id {
            conversation.branch(pid);
        } else {
            conversation.reset_leaf();
        }
        rebuild_display(app, &conversation.context());
        app.status = Some(format!("branched before: {preview}"));
    }
}

fn handle_model_switch(
    app: &mut App,
    tui_config: &mut TuiConfig,
    thinking_prefs: &std::collections::HashMap<String, ThinkingLevel>,
    args: &str,
) {
    let raw = args.trim();
    // resolve tier name (e.g. "fast" -> "claude-3-5-haiku-...") or use as-is
    let id = tui_config
        .model_tiers
        .get(raw)
        .map(|s| s.as_str())
        .unwrap_or(raw);
    if let Some(new_model) = models::find_model_by_id(id) {
        app.model_id = id.into();
        app.stats.context_window = new_model.context_window;
        app.cache.ttl_secs = if tui_config.cache_timer {
            crate::app::cache_ttl_secs(
                &new_model.provider,
                tui_config.options.cache_retention.as_ref(),
            )
        } else {
            0
        };
        let level = thinking_prefs
            .get(id)
            .copied()
            .unwrap_or(ThinkingLevel::Off)
            .normalize_visible();
        app.thinking_level = level;
        if let Some(ref save_last_model) = tui_config.save_last_model {
            save_last_model(id);
        }
        let thinking_str = format!("{level:?}").to_lowercase();
        app.push_system_message(format!("switched to {id} (thinking: {thinking_str})"));
    } else {
        let available = models::all_models_with_user()
            .iter()
            .map(|m| format!("  {}", m.id))
            .collect::<Vec<_>>()
            .join("\n");
        let mut msg = format!("unknown model: {raw}\n\navailable:\n{available}");
        if !tui_config.model_tiers.is_empty() {
            msg.push_str("\n\ntiers:");
            for (tier, model_id) in &tui_config.model_tiers {
                msg.push_str(&format!("\n  {tier} -> {model_id}"));
            }
        }
        app.push_system_message(msg);
    }
}

pub(crate) fn show_cost(app: &mut App) {
    let s = &app.stats;

    let ctx = if s.context_tokens > TokenCount::ZERO {
        let pct = s.context_tokens.percent_of(s.context_window) as u64;
        format!(
            "context: {}k/{}k ({}%)\n",
            s.context_tokens.get() / 1000,
            s.context_window.get() / 1000,
            pct
        )
    } else {
        String::new()
    };

    let total_input = s.cache_read_tokens + s.cache_write_tokens + s.input_tokens;
    let reuse_pct = if total_input > TokenCount::ZERO {
        s.cache_read_tokens.percent_of(total_input) as u64
    } else {
        0
    };
    let write_pct = if total_input > TokenCount::ZERO {
        s.cache_write_tokens.percent_of(total_input) as u64
    } else {
        0
    };

    app.push_system_message(format!(
        "{}cumulative: ↑{} ↓{} R{} W{} | reuse {}% write {}% | {}tok, {}",
        ctx,
        s.input_tokens,
        s.output_tokens,
        s.cache_read_tokens,
        s.cache_write_tokens,
        reuse_pct,
        write_pct,
        s.total_tokens,
        s.total_cost
    ));
}

/// handle `/settings [subcommand ...]`
///
/// supported forms:
/// - `/settings` - show current scope and all anthropic beta toggles
/// - `/settings scope <value>` - switch scope (global|disabled|repo|session)
/// - `/settings betas <field> <bool>` - toggle an anthropic beta
/// - `/settings reset` - restore defaults (session only)
fn handle_settings(app: &mut App, tui_config: &mut TuiConfig, args: &str) {
    let args = args.trim();
    if args.is_empty() {
        app.push_system_message(format_settings_view(&tui_config.settings));
        return;
    }

    let mut parts = args.splitn(3, char::is_whitespace);
    let sub = parts.next().unwrap_or("");
    match sub {
        "scope" => {
            let Some(value) = parts.next() else {
                app.push_system_message("usage: /settings scope <global|disabled|repo|session>");
                return;
            };
            let Some(scope) = crate::settings::SettingsScope::parse(value) else {
                app.push_system_message(format!(
                    "unknown scope {value:?}. expected one of: global, disabled, repo, session"
                ));
                return;
            };
            tui_config.settings.scope = scope;
            app.push_system_message(format!("settings scope → {}", scope.as_str()));
        }
        "betas" => {
            if tui_config.settings.scope == crate::settings::SettingsScope::Disabled {
                app.push_system_message(
                    "settings scope is disabled; edit config.toml to change anthropic betas",
                );
                return;
            }
            let Some(field) = parts.next() else {
                app.push_system_message(
                    "usage: /settings betas <field> <true|false>  (fields: context_1m, effort, context_management, redact_thinking, advisor, advanced_tool_use)",
                );
                return;
            };
            let Some(value) = parts.next() else {
                app.push_system_message("usage: /settings betas <field> <true|false>");
                return;
            };
            let Some(parsed) = parse_bool(value) else {
                app.push_system_message(format!("expected true or false, got {value:?}"));
                return;
            };
            let applied =
                apply_beta_toggle(&mut tui_config.settings.anthropic_betas, field, parsed);
            if !applied {
                app.push_system_message(format!("unknown beta field {field:?}"));
                return;
            }
            // sync to the in-flight StreamOptions so the next turn uses it
            tui_config.options.anthropic_betas = Some(tui_config.settings.anthropic_betas.clone());

            let persist_note = match crate::settings::persist(&tui_config.settings, &tui_config.cwd)
            {
                Ok(Some(path)) => format!(" (saved to {})", path.display()),
                Ok(None) => " (session scope, not persisted)".to_string(),
                Err(e) => format!(" (persist failed: {e})"),
            };
            app.push_system_message(format!("betas.{field} → {parsed}{persist_note}"));
        }
        "reset" => {
            tui_config.settings.anthropic_betas = mush_ai::types::AnthropicBetas::default();
            tui_config.options.anthropic_betas = Some(tui_config.settings.anthropic_betas.clone());
            app.push_system_message("anthropic betas reset to defaults");
        }
        _ => {
            app.push_system_message(format!(
                "unknown /settings subcommand {sub:?}. try: /settings, /settings scope <s>, /settings betas <field> <bool>, /settings reset"
            ));
        }
    }
}

fn format_settings_view(settings: &crate::settings::ScopedSettings) -> String {
    let b = &settings.anthropic_betas;
    let mut out = String::new();
    let _ = writeln!(out, "scope: {}", settings.scope.as_str());
    let _ = writeln!(out, "anthropic betas:");
    let _ = writeln!(out, "  context_1m         = {}", b.context_1m);
    let _ = writeln!(out, "  effort             = {}", b.effort);
    let _ = writeln!(out, "  context_management = {}", b.context_management);
    let _ = writeln!(out, "  redact_thinking    = {}", b.redact_thinking);
    let _ = writeln!(out, "  advisor            = {}", b.advisor);
    let _ = write!(out, "  advanced_tool_use  = {}", b.advanced_tool_use);
    out
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn apply_beta_toggle(betas: &mut mush_ai::types::AnthropicBetas, field: &str, value: bool) -> bool {
    match field {
        "context_1m" => betas.context_1m = value,
        "effort" => betas.effort = value,
        "context_management" => betas.context_management = value,
        "redact_thinking" => betas.redact_thinking = value,
        "advisor" => betas.advisor = value,
        "advanced_tool_use" => betas.advanced_tool_use = value,
        _ => return false,
    }
    true
}

/// kick off an oauth login flow in-TUI. synchronous portion only:
/// get URL + PKCE, open browser, stash pending state on `app` for
/// `/login-complete` to pick up.
fn handle_login_start(app: &mut App, provider_id_opt: Option<&str>) {
    use mush_ai::oauth;

    let provider_id = match provider_id_opt {
        Some(id) => id.to_string(),
        None => {
            let providers = oauth::list_providers();
            if providers.len() == 1 {
                providers[0].0.to_string()
            } else {
                let list = providers
                    .iter()
                    .map(|(id, name)| format!("  {id} - {name}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                app.push_system_message(format!(
                    "available oauth providers:\n{list}\n\nspecify one: /login <provider>"
                ));
                return;
            }
        }
    };

    let Some(provider) = oauth::get_provider(&provider_id) else {
        let available = oauth::list_providers()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
            .join(", ");
        app.push_system_message(format!(
            "unknown oauth provider: {provider_id}\navailable: {available}"
        ));
        return;
    };

    let (prompt, pkce) = match provider.begin_login() {
        Ok(v) => v,
        Err(e) => {
            app.push_system_message(format!("failed to start oauth login: {e}"));
            return;
        }
    };

    let opened = open::that_detached(&prompt.url).is_ok();
    let open_note = if opened {
        "opened in your browser"
    } else {
        "copy the URL above into your browser"
    };

    app.push_system_message(format!(
        "oauth login: {name}\n\n{url}\n\n{open_note}\n\n{instructions}\n\nwhen you have the code, run:\n  /login-complete <code>",
        name = provider.name(),
        url = prompt.url,
        instructions = prompt.instructions,
    ));

    app.pending_oauth = Some(crate::app::PendingOAuth {
        provider_id,
        provider_name: provider.name().to_string(),
        pkce,
    });
}

fn handle_undo(app: &mut App, conversation: &mut ConversationState) {
    let parent = conversation
        .user_messages_in_branch()
        .last()
        .map(|e| e.parent_id.clone());
    match parent {
        None => app.push_system_message("nothing to undo"),
        Some(None) => {
            conversation.reset_leaf();
            rebuild_display(app, &conversation.context());
            app.status = Some("undid last turn".into());
        }
        Some(Some(pid)) => {
            conversation.branch(&pid);
            rebuild_display(app, &conversation.context());
            app.status = Some("undid last turn".into());
        }
    }
}

fn try_template(app: &mut App, name: &str, args: &str) -> Option<String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let templates = mush_ext::discover_templates(&cwd);
    if let Some(tmpl) = mush_ext::find_template(&templates, name) {
        let arg_list: Vec<&str> = if args.is_empty() {
            vec![]
        } else {
            args.split_whitespace().collect()
        };
        let expanded = mush_ext::substitute_args(&tmpl.content, &arg_list);
        app.push_user_message(expanded.clone());
        Some(expanded)
    } else {
        app.push_system_message(format!("unknown command: /{name}  (try /help)"));
        None
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

/// rebuild the TUI display from a conversation (used after branching/resuming)
pub fn rebuild_display(app: &mut App, conversation: &[Message]) {
    crate::conversation_display::rebuild_display(app, conversation);
}

/// export conversation to a markdown file
pub fn handle_export(app: &mut App, conversation: &ConversationState, args: &str) {
    let path = if args.trim().is_empty() {
        "conversation.md".to_string()
    } else {
        args.trim().to_string()
    };

    let mut md = String::new();
    let messages = conversation.context();
    for msg in &messages {
        match msg {
            Message::User(u) => {
                let text = u.text();
                md.push_str(&format!("## you\n\n{text}\n\n"));
            }
            Message::Assistant(a) => {
                let model = a.model.as_ref();
                let text = a.text();
                md.push_str(&format!("## {model}\n\n{text}\n\n"));
            }
            Message::ToolResult(tr) => {
                let output: String = tr
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let preview = if output.chars().count() > 200 {
                    let truncated: String = output.chars().take(197).collect();
                    format!("{truncated}...")
                } else {
                    output
                };
                md.push_str(&format!(
                    "**{}** `{}`\n\n```\n{preview}\n```\n\n",
                    if tr.outcome.is_error() { "✗" } else { "✓" },
                    tr.tool_name,
                ));
            }
        }
    }

    match std::fs::write(&path, &md) {
        Ok(()) => app.push_system_message(format!("exported to {path} ({} bytes)", md.len())),
        Err(e) => app.push_system_message(format!("export failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_cost_includes_reuse_and_write_percentages() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stats.input_tokens = TokenCount::new(100);
        app.stats.output_tokens = TokenCount::new(50);
        app.stats.cache_read_tokens = TokenCount::new(150);
        app.stats.cache_write_tokens = TokenCount::new(50);
        app.stats.total_tokens = TokenCount::new(350);
        app.stats.total_cost = Dollars::new(0.0123);

        show_cost(&mut app);

        let msg = app.messages.last().unwrap();
        assert!(msg.content.contains("reuse 50%"));
        assert!(msg.content.contains("write 16%"));
    }
}
