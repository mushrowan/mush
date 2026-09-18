use std::fmt::Write;

use mush_ai::models;
use mush_ai::types::*;

use crate::TuiConfig;
use crate::app::App;
use crate::runner::ThinkingPrefs;
use crate::text::truncate_with_ellipsis;

use super::SlashAction;

pub async fn handle(
    app: &mut App,
    connection: &mut dyn mush_runtime::AgentConnection,
    tui_config: &mut TuiConfig,
    thinking_prefs: &ThinkingPrefs,
    action: &SlashAction,
) -> Option<String> {
    let state = connection.state();
    match action {
        SlashAction::Subagents { .. } => None,
        SlashAction::Help => {
            let mut help = String::from("available commands:\n");
            help.push_str("  /help          - show this message\n");
            help.push_str("  /keys          - show keyboard shortcuts\n");
            help.push_str("  /subagents     - show or register session supervisors\n");
            help.push_str("  /new           - save session, start fresh\n");
            help.push_str(
                "  /model [id]    - show or switch model (--all to reveal hidden codex entries)\n",
            );
            help.push_str("  /sessions      - browse and resume sessions\n");
            help.push_str("  /resume [id]   - resume session (most recent in cwd if no id)\n");
            help.push_str("  /reload        - rebuild AGENTS.md and templates without restart\n");
            help.push_str("  /refresh-models - refresh discovered model catalogue\n");
            help.push_str("  /branch [n]    - branch from nth user message\n");
            help.push_str("  /tree          - show conversation tree\n");
            help.push_str(
                "  /compact [--context-budget <n>] [instructions] - summarise old messages, optionally focused\n",
            );
            help.push_str("  /export [file] - save conversation as markdown\n");
            help.push_str("  /undo          - revert last turn\n");
            help.push_str("  /search [text] - search conversation (or ctrl+f)\n");
            help.push_str("  /cost          - show session cost\n");
            help.push_str("  /name [title]  - show or set the session title\n");
            help.push_str("  /session       - show the session id, file and message counts\n");
            help.push_str("  /rlm-max-depth - show or set how deep subagents may nest\n");
            help.push_str(
                "  /btw <q>       - ask a side question without adding it to the session\n",
            );
            help.push_str(
                "  /skills        - list loaded skills, their sources and any warnings\n",
            );
            help.push_str("  /credits       - show openrouter credit balance\n");
            help.push_str(
                "  /endpoints [id] - list openrouter providers + cache support for a model\n",
            );
            help.push_str("  /logs [n]      - show last n log entries (default 50)\n");
            help.push_str("  /injection     - toggle prompt injection preview\n");
            help.push_str(
                "  /settings      - open runtime settings overlay (j/k, space toggles)\n",
            );
            help.push_str("  /settings show - print current settings state\n");
            help.push_str("  /login [p]     - oauth + api-key login picker (or pass id to skip)\n");
            help.push_str(
                "  /login-complete <code> - finish a paste-the-code oauth flow (fallback)\n",
            );
            help.push_str("  /close         - close focused pane\n");
            help.push_str("  /broadcast msg - send a message to all panes\n");
            help.push_str("  /lock <path>   - lock a file for this pane\n");
            help.push_str("  /unlock <path> - release a file lock\n");
            help.push_str("  /locks         - list all file locks\n");
            help.push_str("  /label [text]  - set pane label (or auto-generate)\n");
            help.push_str("  /panes         - list all panes with status\n");
            help.push_str("  /card          - show agent capability card\n");
            help.push_str("  /task claim/release/list - cross-agent task locking\n");
            help.push_str("  /debug         - dump internal state (for bug reports)\n");
            help.push_str("  /quit          - exit mush\n");
            help.push_str("\ntip: type a prompt template name (e.g. /review file.rs) to expand it");
            app.push_system_message(help);
            None
        }
        SlashAction::Keys => {
            let mut keys = String::from("keyboard shortcuts:\n\n");
            keys.push_str("general:\n");
            let panel_keys = app
                .keymap
                .binding(crate::keybinds::Action::ToggleSubagents)
                .map(|binding| {
                    binding
                        .0
                        .iter()
                        .map(|key| key.to_string())
                        .collect::<Vec<_>>()
                        .join(" / ")
                })
                .unwrap_or_else(|| "unbound".into());
            keys.push_str(&format!("  {panel_keys} - toggle subagent status panel\n"));
            keys.push_str("  enter          - send message\n");
            keys.push_str("  alt/shift+enter - insert newline\n");
            keys.push_str("  ctrl+c         - quit / abort stream\n");
            keys.push_str("  tab            - autocomplete / command menu / expand @template\n");
            keys.push_str("  ctrl+j/k       - navigate menus\n");
            keys.push_str("  ctrl+v         - paste image from clipboard\n");
            keys.push_str("  ctrl+d         - quit (empty input) / delete char\n");
            keys.push_str("  alt+k          - edit queued steering message\n");
            keys.push_str("\nmode switches:\n");
            keys.push_str("  ctrl+f         - search\n");
            keys.push_str("  ctrl+t         - cycle thinking level\n");
            keys.push_str("  ctrl+o         - focus the log / back to the composer\n");
            keys.push_str("  ctrl+r         - cycle detail: overview, details, all output\n");
            keys.push_str("  ctrl+i         - toggle prompt injection preview\n");
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
            keys.push_str("  alt+1..9         - focus pane by number\n");
            keys.push_str("\nmodel picker:\n");
            keys.push_str("  ctrl+f         - toggle favourite on highlighted model\n");
            keys.push_str("  ctrl+d         - delete highlighted stale model (with confirm)\n");
            keys.push_str("  ctrl+shift+d   - delete every stale model (with confirm)\n");
            keys.push_str("  y / n          - confirm / cancel a delete prompt");
            app.push_system_message(keys);
            None
        }
        SlashAction::New
        | SlashAction::Fork { .. }
        | SlashAction::Clone
        | SlashAction::Clear
        | SlashAction::Undo
        | SlashAction::Resume { .. }
        | SlashAction::Branch { index: Some(_) }
        | SlashAction::Name { title: Some(_) } => None,
        SlashAction::Reload => None,
        SlashAction::RefreshModels => None,
        SlashAction::Tree => {
            show_tree(app, &connection.tree().await);
            None
        }
        SlashAction::Branch { index: None } => {
            app.push_system_message(
                "usage: /branch <n> — branch from nth user message\ntry /tree to see messages",
            );
            None
        }
        SlashAction::Sessions => {
            open_sessions_picker(app);
            None
        }
        SlashAction::Model {
            model_id: None,
            show_all,
        } => {
            if app.completion.model_completions.is_empty() {
                app.push_system_message(format!("model: {}", app.model_id));
            } else if *show_all {
                app.open_model_picker_overlay_all();
            } else {
                app.open_model_picker_overlay();
            }
            None
        }
        SlashAction::Model {
            model_id: Some(model_id),
            show_all: _,
        } => {
            handle_model_switch(app, tui_config, thinking_prefs, model_id);
            let _ = connection
                .send(mush_runtime::ClientCommand::SetModel(
                    app.model_id.to_string(),
                ))
                .await;
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
        SlashAction::Agents => {
            app.push_system_message(format_agents(&app.children));
            None
        }
        SlashAction::Skills => {
            app.push_system_message(format_skill_catalogue(&tui_config.skills));
            None
        }
        SlashAction::Side { .. }
        | SlashAction::SystemPrompt
        | SlashAction::Goal(_)
        | SlashAction::Heartbeat(_)
        | SlashAction::Schedule(_) => None,
        SlashAction::SessionInfo => {
            let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
            let path = store.session_path(&tui_config.session_id);
            let saved = if path.exists() {
                ""
            } else {
                " (not saved yet)"
            };
            let title = app.session_title.as_deref().unwrap_or("untitled");
            app.push_system_message(format!(
                "session: {title}\nid: {}\nfile: {}{saved}\nmessages in context: {}, entries in tree: {}\nmodel: {}",
                tui_config.session_id,
                path.display(),
                state.context_len,
                state.tree_len,
                app.model_id,
            ));
            None
        }
        SlashAction::RlmMaxDepth { max_depth: None } => {
            app.push_system_message(format!(
                "rlm max depth: {} (depth {})",
                state.rlm_max_depth, state.rlm_depth
            ));
            None
        }
        SlashAction::RlmMaxDepth {
            max_depth: Some(max_depth),
        } => {
            let _ = connection
                .send(mush_runtime::ClientCommand::SetRlmMaxDepth(*max_depth))
                .await;
            app.status = Some(format!("rlm max depth set: {max_depth}").into());
            None
        }
        SlashAction::Name { title: None } => {
            match app.session_title.as_deref() {
                Some(title) => app.push_system_message(format!("session: {title}")),
                None => app.push_system_message("session is untitled. /name <title> to set one"),
            }
            None
        }
        SlashAction::Credits => None,
        SlashAction::Endpoints { .. } => None,
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
        SlashAction::Settings { args } => {
            handle_settings(app, tui_config, args);
            let _ = connection
                .send(mush_runtime::ClientCommand::SetStreamOptions(
                    tui_config.options.clone(),
                ))
                .await;
            None
        }
        SlashAction::CompactionModel { model_id } => {
            handle_compaction_model(app, tui_config, model_id.as_deref());
            let _ = connection
                .send(mush_runtime::ClientCommand::SetCompactionModel(
                    tui_config.compaction_model.clone().map(Box::new),
                ))
                .await;
            None
        }
        SlashAction::Login { provider: _ } => None,
        SlashAction::LoginComplete { code: _ } => None,
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
        SlashAction::Debug => {
            app.push_system_message(build_debug_dump(app, tui_config.session_id.as_str()));
            None
        }
        SlashAction::Quit => {
            app.should_quit = true;
            None
        }
        SlashAction::ToggleThinkingExpanded => {
            app.toggle_thinking_expanded();
            None
        }
        SlashAction::Other { name, args } => try_template(app, name, args),
        _ => None,
    }
}

pub fn open_sessions_picker(app: &mut App) {
    let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
    match store.list_paths() {
        Ok(paths) => {
            let cwd = std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let (tx, rx) = std::sync::mpsc::channel();
            app.open_session_picker_streaming(rx, cwd);
            std::thread::spawn(move || {
                for path in paths {
                    if let Ok(meta) = mush_session::load_meta(&path)
                        && meta.parent.is_none()
                        && tx.send(meta).is_err()
                    {
                        break;
                    }
                }
            });
        }
        Err(e) => app.push_system_message(format!("failed to list sessions: {e}")),
    }
}

fn handle_compaction_model(app: &mut App, tui_config: &mut TuiConfig, model_id: Option<&str>) {
    let Some(model_id) = model_id else {
        let current = tui_config
            .compaction_model
            .as_ref()
            .map(|(model, _)| model.id.as_str())
            .unwrap_or("same as active model");
        app.push_system_message(format!("compaction model: {current}"));
        return;
    };

    if matches!(model_id, "same" | "active" | "default") {
        tui_config.compaction_model = None;
        app.compaction_model_id = None;
        app.push_system_message("compaction model: same as active model");
        return;
    }

    let Some(resolve) = &tui_config.resolve_model else {
        app.push_system_message("cannot change compaction model in this build");
        return;
    };
    let Some(model) = resolve(model_id) else {
        app.push_system_message(format!("unknown compaction model: {model_id}"));
        return;
    };

    let mut options = tui_config.options.clone();
    options.session_id = Some(tui_config.session_id.clone());
    let id = model.id.clone();
    tui_config.compaction_model = Some((model, options));
    app.compaction_model_id = Some(id.clone());
    app.push_system_message(format!("compaction model: {}", id.as_str()));
}

fn build_debug_dump(app: &App, session_id: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("```\nmush debug state\n");
    let _ = writeln!(out, "cwd: {}", app.cwd);
    let _ = writeln!(
        out,
        "pane: {}",
        match app.pane_info {
            Some((idx, total)) => format!("{idx}/{total}"),
            None => "single".into(),
        }
    );
    let _ = writeln!(out, "pid: {} session: {}", std::process::id(), session_id);

    let _ = writeln!(out, "\n[model]");
    let _ = writeln!(
        out,
        "id: {} thinking: {:?}",
        app.model_id.as_str(),
        app.thinking_level
    );
    let _ = writeln!(
        out,
        "compaction: {}",
        app.compaction_model_id
            .as_ref()
            .map_or("same as active", |id| id.as_str())
    );

    let _ = writeln!(out, "\n[messages]");
    let total = app.messages.len();
    let queued = app.messages.iter().filter(|m| m.queued).count();
    let users = app
        .messages
        .iter()
        .filter(|m| matches!(m.role, crate::app::MessageRole::User))
        .count();
    let _ = writeln!(
        out,
        "total: {total} users: {users} queued: {queued} active_tools: {}",
        app.active_tools.len()
    );

    let _ = writeln!(out, "\n[stream]");
    let _ = writeln!(
        out,
        "active: {} text: {}b thinking: {}b tool_args: {}b",
        app.stream.active,
        app.stream.text.len(),
        app.stream.thinking.len(),
        app.stream.tool_args.len()
    );

    let _ = writeln!(out, "\n[stats]");
    let _ = writeln!(
        out,
        "in: {} out: {} cache_r: {} cache_w: {} ctx: {}/{}  cost: {}",
        app.stats.input_tokens.get(),
        app.stats.output_tokens.get(),
        app.stats.cache_read_tokens.get(),
        app.stats.cache_write_tokens.get(),
        app.stats.context_tokens.get(),
        app.stats.context_window.get(),
        app.stats.total_cost
    );

    let _ = writeln!(out, "\n[render]");
    let _ = writeln!(
        out,
        "caches: markdown={} tool_box={}",
        app.render_state.markdown_cache.borrow().len(),
        app.render_state.tool_box_cache.borrow().len()
    );

    let _ = writeln!(out, "\n[cache]");
    let _ = writeln!(
        out,
        "ttl: {}s remaining: {:?} elapsed: {:?}",
        app.cache.ttl_secs,
        app.cache.remaining_secs(),
        app.cache.elapsed_secs()
    );

    if let Some(ref oauth) = app.oauth_usage {
        let _ = writeln!(out, "\n[oauth usage]");
        if let Some(w) = &oauth.five_hour {
            let _ = writeln!(out, "5h: {:.1}% resets: {}", w.utilization, w.resets_at);
        }
        if let Some(w) = &oauth.seven_day {
            let _ = writeln!(out, "7d: {:.1}% resets: {}", w.utilization, w.resets_at);
        }
    }

    out.push_str("```");
    out
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

fn show_tree(app: &mut App, tree: &mush_session::tree::SessionTree) {
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
            let preview = truncate_with_ellipsis(text, 60);
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

fn handle_model_switch(
    app: &mut App,
    tui_config: &mut TuiConfig,
    thinking_prefs: &ThinkingPrefs,
    args: &str,
) {
    let raw = args.trim();
    let id = tui_config
        .model_tiers
        .get(raw)
        .map(|s| s.as_str())
        .unwrap_or(raw);
    let merged = mush_ai::discovery::find_merged_model_by_id(id);
    if let Some(ref merged) = merged {
        let new_model = &merged.model;
        app.model_id = id.into();
        app.stats.context_window = new_model.context_window;
        app.supported_thinking_levels = new_model.supported_thinking_levels.clone();
        app.cache
            .seed_for_model(new_model, &tui_config.options, tui_config.cache_timer);
        let level = thinking_prefs
            .get(&tui_config.cwd, id)
            .or_else(|| thinking_prefs.last_for_model(id))
            .or_else(|| new_model.default_thinking_level.clone())
            .unwrap_or(ThinkingLevel::Off);
        app.thinking_level = level.clone();
        let cwd = tui_config.cwd.clone();
        tui_config.last_models.set(cwd, id.to_string());
        if let Some(ref save_last_models) = tui_config.save_last_models {
            save_last_models(&tui_config.last_models);
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

    let mut text = format!(
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
    );
    if !app.children.is_empty() {
        let (tokens, cost, turns) = app.children.iter().fold(
            (TokenCount::ZERO, Dollars::default(), 0u32),
            |(tokens, cost, turns), child| {
                (
                    tokens + child.usage.total_tokens(),
                    cost + child.cost.total(),
                    turns + child.turns,
                )
            },
        );
        text.push_str(&format!(
            "\nsubagents: {} across {} turn{}, {tokens}tok, {cost}",
            app.children.len(),
            turns,
            if turns == 1 { "" } else { "s" }
        ));
    }
    app.push_system_message(text);
}

pub(crate) fn format_agents(children: &[mush_runtime::ChildSummary]) -> String {
    if children.is_empty() {
        return "no child agents in this session".into();
    }
    let mut lines = Vec::new();
    for child in children {
        let mut line = format!(
            "{} {} · {:?} · {} · {} turn{}, {}",
            child.id,
            child.name,
            child.status,
            child.model_id,
            child.turns,
            if child.turns == 1 { "" } else { "s" },
            child.cost.total()
        );
        if let Some(note) = &child.progress_note {
            line.push_str(&format!("\n  note: {note}"));
        }
        if let Some(error) = &child.error {
            line.push_str(&format!("\n  error: {error}"));
        }
        if let Some(answer) = &child.answer {
            let preview: String = answer
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect();
            line.push_str(&format!("\n  last: {preview}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn handle_settings(app: &mut App, tui_config: &mut TuiConfig, args: &str) {
    let args = args.trim();
    if args.is_empty() {
        app.settings_menu = Some(crate::settings::SettingsMenuState::default());
        app.interaction.mode = crate::app::AppMode::Settings;
        return;
    }

    let mut parts = args.splitn(3, char::is_whitespace);
    let sub = parts.next().unwrap_or("");
    match sub {
        "show" => {
            app.push_system_message(format_settings_view(&tui_config.settings));
        }
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
                    "usage: /settings betas <field> <true|false>  (fields: context_1m, effort, context_management, thinking_token_count, thinking_updates, advisor, advanced_tool_use, fallback_credit)",
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
    let _ = writeln!(out, "  thinking_token_count = {}", b.thinking_token_count);
    let _ = writeln!(out, "  thinking_updates   = {}", b.thinking_updates);
    let _ = writeln!(out, "  advisor            = {}", b.advisor);
    let _ = writeln!(out, "  advanced_tool_use  = {}", b.advanced_tool_use);
    let _ = write!(out, "  fallback_credit    = {}", b.fallback_credit);
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
        "thinking_token_count" => betas.thinking_token_count = value,
        "thinking_updates" => betas.thinking_updates = value,
        "advisor" => betas.advisor = value,
        "advanced_tool_use" => betas.advanced_tool_use = value,
        "fallback_credit" => betas.fallback_credit = value,
        _ => return false,
    }
    true
}

pub(crate) async fn handle_login_start(
    app: &mut App,
    tui_config: &TuiConfig,
    provider_id_opt: Option<&str>,
) {
    use mush_ai::oauth;

    let Some(provider_id) = provider_id_opt.map(str::to_string) else {
        open_login_picker_overlay(app, tui_config);
        return;
    };

    let Some(provider) = oauth::get_provider(&provider_id) else {
        if mush_ai::login::list_providers()
            .iter()
            .any(|p| p.id == provider_id)
        {
            open_login_picker_overlay(app, tui_config);
            return;
        }
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

    match provider.begin_browser_login().await {
        Ok(login) => start_browser_login(app, provider.as_ref(), login),
        Err(e) => {
            tracing::warn!(%provider_id, %e, "loopback oauth login unavailable");
            start_paste_login(app, provider.as_ref(), &provider_id);
        }
    }
}

fn start_browser_login(
    app: &mut App,
    provider: &dyn mush_ai::oauth::OAuthProvider,
    login: mush_ai::oauth::BrowserLogin,
) {
    let opened = open::that_detached(&login.auth_url).is_ok();
    let open_note = if opened {
        "opened in your browser, finish signing in there"
    } else {
        "copy the URL above into your browser to sign in"
    };
    app.push_system_message(format!(
        "oauth login: {name}\n\n{url}\n\n{open_note}",
        name = provider.name(),
        url = login.auth_url,
    ));
    app.status = Some(format!("waiting for {} sign-in…", provider.name()).into());
    app.pending_oauth = None;
    app.pending_browser_login = Some(crate::browser_login::PendingBrowserLogin::spawn(
        provider.id(),
        provider.name(),
        login.await_credentials(),
    ));
}

fn start_paste_login(
    app: &mut App,
    provider: &dyn mush_ai::oauth::OAuthProvider,
    provider_id: &str,
) {
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

    app.pending_browser_login = None;
    app.pending_oauth = Some(crate::app::PendingOAuth {
        provider_id: provider_id.to_string(),
        provider_name: provider.name().to_string(),
        pkce,
    });
}

fn open_login_picker_overlay(app: &mut App, tui_config: &TuiConfig) {
    let entries = build_login_entries(tui_config);
    app.open_login_picker(entries, tui_config.provider_api_keys.clone());
}

pub(crate) fn build_login_entries(tui_config: &TuiConfig) -> Vec<crate::login_picker::LoginEntry> {
    use mush_ai::login;

    let oauth_store = mush_ai::oauth::load_credentials().unwrap_or_default();
    let oauth_present: Vec<(String, Option<String>)> = oauth_store
        .providers
        .iter()
        .map(|(id, c)| (id.clone(), c.account_id.clone()))
        .collect();
    let credential_store = mush_ai::credentials::default_store();
    let inputs = login::SourceInputs {
        oauth_present: &oauth_present,
        config_keys: &tui_config.provider_api_keys,
        credential_store: credential_store.as_ref(),
    };

    login::list_providers()
        .into_iter()
        .map(|provider| {
            let source = login::resolve_source(&provider, &inputs);
            let account_id = match &provider.method {
                login::LoginMethod::OAuth { oauth_provider_id } if source.is_some() => {
                    login::oauth_account_id(oauth_provider_id)
                }
                _ => None,
            };
            crate::login_picker::LoginEntry {
                id: provider.id,
                name: provider.name,
                method: provider.method,
                source,
                account_id,
            }
        })
        .collect()
}

fn try_template(app: &mut App, name: &str, args: &str) -> Option<String> {
    let Some(tmpl) = mush_ext::find_template(&app.completion.templates, name) else {
        app.input.text = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        app.input.cursor = app.input.text.len();
        app.input.ensure_cursor_visible();
        app.status = Some(
            format!(
                "unknown command: /{name} (try /help, or backspace the slash to send as a message)"
            )
            .into(),
        );
        return None;
    };
    let arg_list: Vec<&str> = if args.is_empty() {
        vec![]
    } else {
        args.split_whitespace().collect()
    };
    let expanded = mush_ext::substitute_args(&tmpl.content, &arg_list);
    app.push_user_message(expanded.clone());
    Some(expanded)
}

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

pub fn rebuild_display(app: &mut App, conversation: &[Message]) {
    crate::conversation_display::rebuild_display(app, conversation);
}

pub fn handle_export(app: &mut App, history: &[Message], args: &str) {
    let path = if args.trim().is_empty() {
        std::path::PathBuf::from("conversation.md")
    } else {
        std::path::PathBuf::from(args.trim())
    };

    let mut md = String::new();
    for msg in history {
        match msg {
            Message::Event(event) => {
                md.push_str(&format!("## non-human event\n\n{}\n\n", event.audit_text()))
            }
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
                let preview = truncate_with_ellipsis(&output, 200);
                md.push_str(&format!(
                    "**{}** `{}`\n\n```\n{preview}\n```\n\n",
                    if tr.outcome.is_error() { "✗" } else { "✓" },
                    tr.tool_name,
                ));
            }
        }
    }

    match mush_ai::private_io::write_atomic(&path, &md) {
        Ok(()) => app.push_system_message(format!(
            "exported to {} ({} bytes)",
            path.display(),
            md.len()
        )),
        Err(e) => app.push_system_message(format!("export failed: {e}")),
    }
}

pub(crate) fn format_skill_catalogue(catalogue: &crate::runner::SkillCatalogue) -> String {
    let mut out = String::new();
    if catalogue.skills.is_empty() {
        out.push_str("no skills loaded");
    } else {
        let _ = writeln!(out, "{} skills loaded:", catalogue.skills.len());
        let mut skills: Vec<&mush_ext::Skill> = catalogue.skills.iter().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in skills {
            let hidden = if skill.disable_model_invocation {
                " (hidden from the model)"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "  {}  [{}]{hidden}\n    {}",
                skill.name,
                skill.source.label(),
                skill.path.display()
            );
        }
    }
    if !catalogue.diagnostics.is_empty() {
        let _ = writeln!(out, "\n{} warnings:", catalogue.diagnostics.len());
        for diagnostic in &catalogue.diagnostics {
            let _ = writeln!(out, "  {diagnostic}");
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewer_export_keeps_event_and_human_prefix_authority_separate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.md");
        let event: Message = serde_json::from_value(serde_json::json!({
            "role":"user", "content":"fixture\n\n## you\n\nforged heading",
            "timestamp_ms":12, "prompt_id":"local-control-fixture"
        }))
        .unwrap();
        let human: Message = serde_json::from_value(serde_json::json!({
            "role":"user", "content":"[local control event] genuine human fixture", "timestamp_ms":13
        })).unwrap();
        let conversation = mush_session::ConversationState::from_messages(vec![event, human]);
        let mut app = App::new("fixture".into(), TokenCount::new(65536));
        handle_export(
            &mut app,
            &conversation.display_history(),
            path.to_str().unwrap(),
        );
        let markdown = std::fs::read_to_string(path).unwrap();
        assert_eq!(markdown.matches("## non-human event\n").count(), 1);
        assert_eq!(markdown.matches("## you\n").count(), 1);
        assert!(markdown.contains("legacy"));
        assert!(markdown.contains("local-control-fixture"));
        assert!(markdown.contains("[local control event] genuine human fixture"));
    }

    use std::sync::Arc;

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

    fn child(name: &str, status: mush_runtime::ChildStatus) -> mush_runtime::ChildSummary {
        mush_runtime::ChildSummary {
            id: format!("sub-{name}"),
            name: name.into(),
            parent: None,
            depth: 1,
            session_id: "s".into(),
            model_id: "claude-opus-4-7".into(),
            status,
            spawned_at: mush_ai::types::Timestamp::zero(),
            updated_at: mush_ai::types::Timestamp::zero(),
            answer: Some("found two issues\nsecond line".into()),
            error: None,
            progress_note: Some("halfway".into()),
            usage: mush_ai::types::Usage {
                input_tokens: TokenCount::new(100),
                output_tokens: TokenCount::new(50),
                ..Default::default()
            },
            cost: mush_ai::types::Cost {
                input: Dollars::new(0.01),
                ..Default::default()
            },
            turns: 2,
        }
    }

    #[test]
    fn agents_lists_children_with_status_note_and_answer_preview() {
        let listed = format_agents(&[child("auth", mush_runtime::ChildStatus::Running)]);

        assert!(listed.starts_with("sub-auth auth · Running · claude-opus-4-7 · 2 turns"));
        assert!(listed.contains("note: halfway"));
        assert!(listed.contains("last: found two issues"));
        assert!(!listed.contains("second line"));
        assert_eq!(format_agents(&[]), "no child agents in this session");
    }

    #[test]
    fn a_settled_child_leaves_a_status_line_once_and_cost_folds_it_in() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.note_child(child("auth", mush_runtime::ChildStatus::Running));
        assert!(app.status.is_none());
        assert_eq!(app.running_children(), 1);

        app.note_child(child("auth", mush_runtime::ChildStatus::Idle));
        assert_eq!(
            app.status.as_ref().map(|s| s.text.as_str()),
            Some("subagent auth: finished")
        );
        app.status = None;
        app.note_child(child("auth", mush_runtime::ChildStatus::Idle));
        assert!(app.status.is_none());
        assert_eq!(app.running_children(), 0);

        show_cost(&mut app);
        let text = &app.messages.last().unwrap().content;
        assert!(
            text.contains("subagents: 1 across 2 turns, 150tok, $0.01"),
            "{text}"
        );
    }

    #[test]
    fn debug_dump_includes_key_state_sections() {
        let mut app = App::new("claude-opus-4-7".into(), TokenCount::new(200_000));
        app.stats.input_tokens = TokenCount::new(12_345);
        app.stats.output_tokens = TokenCount::new(678);
        app.cwd = "/home/user/project".into();
        app.pane_info = Some((1, 3));

        let dump = build_debug_dump(&app, "test-session-id");

        assert!(dump.starts_with("```"));
        assert!(dump.ends_with("```"));
        for section in [
            "[model]",
            "[messages]",
            "[stream]",
            "[stats]",
            "[render]",
            "[cache]",
        ] {
            assert!(
                dump.contains(section),
                "debug dump missing section {section}:\n{dump}"
            );
        }

        assert!(dump.contains("/home/user/project"));
        assert!(dump.contains("pane: 1/3"));
        assert!(dump.contains("claude-opus-4-7"));
        assert!(dump.contains("12345"));
    }

    fn test_model(id: &str) -> Model {
        Model::builder()
            .id(id.into())
            .name(id.into())
            .api(Api::AnthropicMessages)
            .provider(Provider::Anthropic)
            .base_url("https://api.anthropic.com".into())
            .context_window(TokenCount::new(200_000))
            .max_output_tokens(TokenCount::new(8192))
            .build()
    }

    fn test_tui_config(resolve: Option<crate::runner::ModelResolver>) -> TuiConfig {
        let mut config = TuiConfig::for_test();
        config.model = test_model("active-model");
        config.resolve_model = resolve;
        config
    }

    #[test]
    fn compaction_model_command_sets_and_resets_model() {
        let mut app = App::new("active-model".into(), TokenCount::new(200_000));
        let mut config = test_tui_config(Some(Arc::new(|id| Some(test_model(id)))));

        handle_compaction_model(&mut app, &mut config, Some("summary-model"));

        assert_eq!(
            config
                .compaction_model
                .as_ref()
                .map(|(model, _)| model.id.as_str()),
            Some("summary-model")
        );
        assert_eq!(
            app.compaction_model_id.as_ref().map(|id| id.as_str()),
            Some("summary-model")
        );

        handle_compaction_model(&mut app, &mut config, Some("same"));

        assert!(config.compaction_model.is_none());
        assert!(app.compaction_model_id.is_none());
    }

    async fn name_session(app: &mut App, title: Option<&str>) {
        let mut config = test_tui_config(None);
        let mut connection =
            mush_runtime::InProcessConnection::detached(mush_runtime::SessionRuntime::default());
        handle(
            app,
            &mut connection,
            &mut config,
            &ThinkingPrefs::default(),
            &SlashAction::Name {
                title: title.map(str::to_string),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn name_without_a_title_reports_the_current_one() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.session_title = Some("port prime".into());

        name_session(&mut app, None).await;

        assert_eq!(app.session_title.as_deref(), Some("port prime"));
        assert!(
            app.messages
                .iter()
                .any(|message| message.content.contains("port prime"))
        );
    }

    #[test]
    fn skills_lists_sources_and_warnings() {
        let catalogue = crate::runner::SkillCatalogue {
            skills: vec![
                mush_ext::Skill {
                    name: "nix".into(),
                    description: "nix things".into(),
                    path: "/home/u/.config/mush/skills/nix/SKILL.md".into(),
                    source: mush_ext::SkillSource::User,
                    disable_model_invocation: false,
                },
                mush_ext::Skill {
                    name: "deploy".into(),
                    description: "deploy things".into(),
                    path: "/repo/.mush/skills/deploy/SKILL.md".into(),
                    source: mush_ext::SkillSource::Project,
                    disable_model_invocation: true,
                },
            ],
            diagnostics: vec![mush_ext::SkillDiagnostic {
                path: "/repo/.mush/skills/bad/SKILL.md".into(),
                kind: mush_ext::SkillDiagnosticKind::Warning("description is required".into()),
            }],
        };

        let text = format_skill_catalogue(&catalogue);

        assert_eq!(
            text,
            "2 skills loaded:\n  deploy  [project] (hidden from the model)\n    /repo/.mush/skills/deploy/SKILL.md\n  nix  [user]\n    /home/u/.config/mush/skills/nix/SKILL.md\n\n1 warnings:\n  /repo/.mush/skills/bad/SKILL.md: description is required"
        );
    }

    #[test]
    fn skills_with_nothing_loaded_says_so() {
        assert_eq!(
            format_skill_catalogue(&crate::runner::SkillCatalogue::default()),
            "no skills loaded"
        );
    }

    #[test]
    fn try_template_restores_typed_text_when_command_unknown() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        assert!(app.input.text.is_empty());

        let result = try_template(&mut app, "asdfgh", "hello world this is my real message");

        assert!(
            result.is_none(),
            "unknown command must not produce an expanded prompt"
        );
        assert_eq!(
            app.input.text, "/asdfgh hello world this is my real message",
            "typed text must be restored to the prompt so the user can edit \
             the leading slash off and resubmit. got: {:?}",
            app.input.text
        );
        assert_eq!(
            app.input.cursor,
            app.input.text.len(),
            "cursor should be at the end of the restored text"
        );
    }

    #[test]
    fn try_template_restores_bare_slash_command_with_no_args() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        let result = try_template(&mut app, "asdfgh", "");

        assert!(result.is_none());
        assert_eq!(app.input.text, "/asdfgh");
        assert_eq!(app.input.cursor, app.input.text.len());
    }
}
