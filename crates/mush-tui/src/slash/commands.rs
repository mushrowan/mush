use std::fmt::Write;

use mush_ai::types::*;

use crate::TuiConfig;
use crate::app::App;
use crate::text::truncate_with_ellipsis;

use super::SlashAction;

pub async fn handle(
    app: &mut App,
    connection: &mut dyn mush_runtime::AgentConnection,
    tui_config: &mut TuiConfig,
    action: &SlashAction,
) -> Option<String> {
    let state = connection.state();
    match action {
        SlashAction::Help => {
            app.push_system_message(HELP_TEXT);
            None
        }
        SlashAction::Keys => {
            app.push_system_message(keys_text(app));
            None
        }
        SlashAction::Subagents { .. }
        | SlashAction::New { .. }
        | SlashAction::Fork { index: Some(_) }
        | SlashAction::Clone
        | SlashAction::Clear
        | SlashAction::Undo
        | SlashAction::Resume { .. }
        | SlashAction::Branch { index: Some(_) }
        | SlashAction::NavigateTree { .. }
        | SlashAction::Name { title: Some(_) }
        | SlashAction::Reload
        | SlashAction::RefreshModels
        | SlashAction::Side { .. }
        | SlashAction::SystemPrompt
        | SlashAction::Goal(_)
        | SlashAction::Heartbeat(_)
        | SlashAction::Schedule(_)
        | SlashAction::Autonomous(_)
        | SlashAction::Credits
        | SlashAction::Endpoints { .. }
        | SlashAction::Login { .. }
        | SlashAction::LoginApiKey { .. }
        | SlashAction::LoginComplete { .. }
        | SlashAction::Compact { .. }
        | SlashAction::Refine(_)
        | SlashAction::Export { .. }
        | SlashAction::Mcp(_) => None,
        SlashAction::Tree => {
            let items = crate::message_picker::tree_items(&connection.tree_summary().await);
            open_message_picker(
                app,
                crate::message_picker::MessagePickerPurpose::Tree,
                items,
            );
            None
        }
        SlashAction::Fork { index: None } => {
            let items = crate::message_picker::user_message_items(&connection.transcript().await);
            open_message_picker(
                app,
                crate::message_picker::MessagePickerPurpose::Fork,
                items,
            );
            None
        }
        SlashAction::Branch { index: None } => {
            let items = crate::message_picker::branch_items(&connection.tree_summary().await);
            open_message_picker(
                app,
                crate::message_picker::MessagePickerPurpose::Branch,
                items,
            );
            None
        }
        SlashAction::Sessions => {
            open_sessions_picker(app, tui_config.agent_roster.clone());
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
            if handle_model_switch(app, tui_config, model_id) {
                let _ = connection
                    .send(mush_runtime::ClientCommand::SetModel(
                        app.model_id.to_string(),
                    ))
                    .await;
            } else if !app.open_model_picker_with_search(model_id) {
                app.push_system_message(format!("unknown model: {model_id}"));
            }
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
        SlashAction::Context => {
            let title = app.session_title.clone().unwrap_or_else(|| {
                let id = tui_config.session_id.to_string();
                format!("session {}", &id[..8.min(id.len())])
            });
            let tree = crate::context_view::agent_tree(
                &title,
                app.model_id.as_str(),
                &app.stats,
                &app.children,
            );
            let text = crate::context_view::format_context(&tree, &app.stats);
            app.push_system_message(text);
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
        SlashAction::RlmMaxDepth {
            max_depth: None, ..
        } => {
            app.push_system_message(format!(
                "rlm max depth: {} ({}, depth {})",
                state.rlm_max_depth, state.rlm_max_depth_source, state.rlm_depth
            ));
            None
        }
        SlashAction::RlmMaxDepth {
            max_depth: Some(max_depth),
            global,
        } => {
            if let Err(error) = connection
                .send(mush_runtime::ClientCommand::SetRlmMaxDepth(*max_depth))
                .await
            {
                app.push_system_message(format!("rlm max depth not set: {error}"));
                return None;
            }
            let saved = global.then(|| crate::settings::save_global_rlm_max_depth(*max_depth));
            match saved {
                Some(Err(error)) => app.push_system_message(format!(
                    "rlm max depth set for this chat, but the global default was not saved: \
                     {error}"
                )),
                Some(Ok(_)) => {
                    app.status = Some(
                        format!("rlm max depth set: {max_depth} and saved as global default")
                            .into(),
                    );
                }
                None => app.status = Some(format!("rlm max depth set: {max_depth}").into()),
            }
            None
        }
        SlashAction::Name { title: None } => {
            match app.session_title.as_deref() {
                Some(title) => app.push_system_message(format!("session: {title}")),
                None => app.push_system_message("session is untitled. /name <title> to set one"),
            }
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
        SlashAction::Settings { args } => {
            handle_settings(app, tui_config, args);
            let _ = connection
                .send(mush_runtime::ClientCommand::SetStreamOptions(Box::new(
                    tui_config.options.clone(),
                )))
                .await;
            None
        }
        SlashAction::SubagentModel { model_id, unset } => {
            let chosen = if *unset {
                None
            } else if let Some(id) = model_id {
                Some(id.clone())
            } else {
                app.open_subagent_model_picker();
                return None;
            };
            app.subagent_model_id = chosen.as_deref().map(mush_ai::types::ModelId::from);
            let _ = connection
                .send(mush_runtime::ClientCommand::SetSubagentModel(
                    chosen.clone(),
                ))
                .await;
            app.push_system_message(match &chosen {
                Some(id) => format!("subagents will use {id}"),
                None => "subagents will use whatever model this session is on".to_string(),
            });
            None
        }
        SlashAction::Effort { level } => {
            handle_effort(app, tui_config.model.reasoning, level.as_deref());
            None
        }
        SlashAction::Account { label } => {
            match label {
                None => app.push_system_message(account_report(&tui_config.model, &app.accounts)),
                Some(label) => match connection
                    .send(mush_runtime::ClientCommand::SetAccount(Some(label.clone())))
                    .await
                {
                    Ok(events) => {
                        for event in events {
                            match event {
                                mush_runtime::ClientEvent::Session(
                                    mush_runtime::SessionEvent::Accounts(accounts),
                                ) => app.accounts = accounts,
                                mush_runtime::ClientEvent::Session(
                                    mush_runtime::SessionEvent::Status(status),
                                ) => app.push_system_message(status),
                                _ => {}
                            }
                        }
                    }
                    Err(error) => app.push_system_message(error.to_string()),
                },
            }
            None
        }
        SlashAction::Pause { reason } => {
            let pause = mush_session::Pause {
                at: mush_ai::types::Timestamp::now(),
                reason: reason.clone(),
                by: Some("user".to_string()),
            };
            app.paused = Some(pause.clone());
            let _ = connection
                .send(mush_runtime::ClientCommand::SetPaused(Some(pause)))
                .await;
            app.push_system_message(match reason {
                Some(reason) => format!("paused: {reason}"),
                None => "paused, no new turns until /unpause".to_string(),
            });
            None
        }
        SlashAction::Resumed => {
            app.paused = None;
            let _ = connection
                .send(mush_runtime::ClientCommand::SetPaused(None))
                .await;
            app.push_system_message("resumed");
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
        SlashAction::Card => {
            if let Some(card) = &tui_config.agent_card {
                app.push_system_message(format!("agent card:\n```json\n{}\n```", card.to_json()));
            } else {
                app.push_system_message("no agent card available");
            }
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
        SlashAction::Other { name, args } => {
            if let Some(skill) = name.strip_prefix("skill:") {
                return try_skill(app, &tui_config.skills.skills, skill, args);
            }
            try_template(app, name, args)
        }
    }
}

fn open_message_picker(
    app: &mut App,
    purpose: crate::message_picker::MessagePickerPurpose,
    items: Vec<crate::message_picker::MessagePickerItem>,
) {
    if items.is_empty() {
        app.push_system_message(format!("no messages to {} from", purpose.verb()));
        return;
    }
    app.open_message_picker(purpose, items);
}

pub fn open_sessions_picker(app: &mut App, roster: Option<crate::runner::AgentRoster>) {
    let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
    match store.list_paths() {
        Ok(paths) => {
            let cwd = std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let (tx, rx) = std::sync::mpsc::channel();
            app.open_session_picker_streaming(rx, cwd);
            if let Some(roster) = roster
                && let Some(picker) = app.interaction.session_picker.as_mut()
            {
                let (live_tx, live_rx) = std::sync::mpsc::channel();
                picker.watch_roster(live_rx);
                std::thread::spawn(move || {
                    let _ = live_tx.send(roster());
                });
            }
            std::thread::spawn(move || {
                for path in paths {
                    if let Ok(meta) = mush_session::load_meta(&path)
                        && crate::session_picker::listed(&meta)
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

const KEYS_TEXT: &str = concat!(
    "  enter          - send message\n",
    "  alt/shift+enter - insert newline\n",
    "  ctrl+c         - quit / abort stream\n",
    "  tab            - autocomplete / command menu / expand @template\n",
    "  ctrl+j/k       - navigate menus\n",
    "  ctrl+v         - paste image from clipboard\n",
    "  ctrl+d         - delete char\n",
    "  alt+k          - edit queued steering message\n",
    "\nmode switches:\n",
    "  ctrl+f         - search\n",
    "  ctrl+t         - cycle thinking level\n",
    "  ctrl+o         - focus the log / back to the composer\n",
    "  ctrl+r         - cycle detail: overview, details, all output\n",
    "\nediting:\n",
    "  ctrl+a         - select all\n",
    "  home           - start of line\n",
    "  ctrl+e / end   - end of line\n",
    "  ctrl+w         - delete word backward\n",
    "  alt+d          - delete word forward\n",
    "  ctrl+u         - delete to start\n",
    "  ctrl+k         - delete to end\n",
    "  alt+b / alt+←  - word left\n",
    "  alt+f / alt+→  - word right\n",
    "\nmodel picker:\n",
    "  ctrl+f         - toggle favourite on highlighted model\n",
    "  ctrl+d         - delete highlighted stale model (with confirm)\n",
    "  alt+d          - delete every stale model (with confirm)\n",
    "  y / n          - confirm / cancel a delete prompt",
);

fn keys_text(app: &App) -> String {
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
    format!(
        "keyboard shortcuts:\n\ngeneral:\n  {panel_keys} - toggle subagent status panel\n{KEYS_TEXT}"
    )
}

const HELP_TEXT: &str = concat!(
    "available commands:\n",
    "  /help          - show this message\n",
    "  /keys          - show keyboard shortcuts\n",
    "  /subagents     - show or register session supervisors\n",
    "  /skill:<name> [args] - send a skill's instructions as your prompt\n",
    "  /new           - save session, start fresh\n",
    "  /model [id]    - show or switch model (--all to reveal hidden codex entries)\n",
    "  /sessions      - browse and resume sessions\n",
    "  /resume [id]   - resume session (most recent in cwd if no id)\n",
    "  /reload        - rebuild AGENTS.md and templates without restart\n",
    "  /refresh-models - refresh discovered model catalogue\n",
    "  /branch [n]    - branch from nth user message, or pick one\n",
    "  /tree          - browse the conversation tree and jump to a message\n",
    "  /refine [--global] [instructions] - refine the continual harness, rollback <id> undoes one\n",
    "  /compact [--context-budget <n>] [instructions] - summarise old messages, optionally focused\n",
    "  /export [file] - save conversation as markdown\n",
    "  /undo          - revert last turn\n",
    "  /search [text] - search conversation (or ctrl+f)\n",
    "  /cost          - show session cost\n",
    "  /context       - tokens, cost and context for this agent and its children\n",
    "  /name [title]  - show or set the session title\n",
    "  /session       - show the session id, file and message counts\n",
    "  /rlm-max-depth [n [--global]] - show or set how deep subagents may nest in this chat\n",
    "  /btw <q>       - ask a side question without adding it to the session\n",
    "  /skills        - list loaded skills, their sources and any warnings\n",
    "  /credits       - show openrouter credit balance\n",
    "  /endpoints [id] - list openrouter providers + cache support for a model\n",
    "  /logs [n]      - show last n log entries (default 50)\n",
    "  /settings      - open runtime settings overlay (j/k, space toggles)\n",
    "  /settings show - print current settings state\n",
    "  /login [p] [key] - oauth + api-key login picker (or pass id to skip, plus a key to save it)\n\n",
    "  /login-complete <code> - finish a paste-the-code oauth flow (fallback)\n",
    "  /card          - show agent capability card\n",
    "  /debug         - dump internal state (for bug reports)\n",
    "  /quit          - exit mush\n",
    "\ntip: type a prompt template name (e.g. /review file.rs) to expand it",
);

fn handle_model_switch(app: &mut App, tui_config: &mut TuiConfig, args: &str) -> bool {
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
        let level = new_model
            .clamp_thinking(&app.thinking_level)
            .normalize_visible();
        app.thinking_level = level.clone();
        let cwd = tui_config.cwd.clone();
        tui_config.last_models.set(cwd, id.to_string());
        if let Some(ref save_last_models) = tui_config.save_last_models {
            save_last_models(&tui_config.last_models);
        }
        let thinking_str = format!("{level:?}").to_lowercase();
        app.push_system_message(format!("switched to {id} (thinking: {thinking_str})"));
    }
    merged.is_some()
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
        let _ = write!(
            text,
            "\nsubagents: {} across {} turn{}, {tokens}tok, {cost}",
            app.children.len(),
            turns,
            if turns == 1 { "" } else { "s" }
        );
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
            let _ = write!(line, "\n  note: {note}");
        }
        if let Some(error) = &child.error {
            let _ = write!(line, "\n  error: {error}");
        }
        if let Some(answer) = &child.answer {
            let preview: String = answer
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect();
            let _ = write!(line, "\n  last: {preview}");
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
    let Some(flag) = crate::settings::MenuItemKind::all()
        .iter()
        .find(|item| item.label() == field)
        .and_then(|item| item.beta_mut(betas))
    else {
        return false;
    };
    *flag = value;
    true
}

fn handle_effort(app: &mut App, reasoning: bool, requested: Option<&str>) {
    if !reasoning {
        app.status = Some("Current model does not support thinking".into());
        return;
    }
    let levels = app.available_thinking_levels();
    let Some(requested) = requested else {
        app.input.text = "/effort ".into();
        app.input.cursor = app.input.text.len();
        app.open_slash_menu();
        return;
    };
    let Some(level) = levels
        .iter()
        .find(|level| level.effort_name().unwrap_or("off") == requested)
    else {
        let names: Vec<&str> = levels
            .iter()
            .map(|level| level.effort_name().unwrap_or("off"))
            .collect();
        app.push_system_message(format!(
            "Unknown thinking level '{requested}'. Available: {}",
            names.join(", ")
        ));
        return;
    };
    app.thinking_level = level.clone();
    app.status = Some(format!("Thinking level: {requested}").into());
}

pub(crate) fn account_report(
    model: &mush_ai::types::Model,
    session_accounts: &mush_ai::types::SessionAccounts,
) -> String {
    let Some(oauth_provider) = mush_ai::auth::oauth_provider_id(&model.provider) else {
        return format!("{} has no account logins", model.provider);
    };
    let store = mush_ai::oauth::load_credentials().unwrap_or_default();
    let Some(accounts) = store.accounts(oauth_provider) else {
        return format!(
            "no {oauth_provider} accounts yet, run /login {oauth_provider} [<label>] to add one"
        );
    };
    let default = accounts.default_label();
    let active = session_accounts.get(oauth_provider).or(default);
    let mut lines = vec![format!("{oauth_provider} accounts:")];
    for label in accounts.labels() {
        let marker = if Some(label) == active { "●" } else { " " };
        let note = if Some(label) == default {
            " (default for new sessions)"
        } else {
            ""
        };
        lines.push(format!("{marker} {label}{note}"));
    }
    lines.push(format!(
        "switch with /account <label>, add one with /login {oauth_provider} <label>"
    ));
    lines.join("\n")
}

pub(crate) async fn handle_login_start(
    app: &mut App,
    tui_config: &TuiConfig,
    provider_id_opt: Option<&str>,
    account: Option<mush_ai::types::AccountLabel>,
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
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.arm_entry_for(&provider_id);
            }
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
        Ok(login) => start_browser_login(app, provider.as_ref(), login, account),
        Err(e) => {
            tracing::warn!(%provider_id, %e, "loopback oauth login unavailable");
            start_paste_login(app, provider.as_ref(), &provider_id, account);
        }
    }
}

fn start_browser_login(
    app: &mut App,
    provider: &dyn mush_ai::oauth::OAuthProvider,
    login: mush_ai::oauth::BrowserLogin,
    account: Option<mush_ai::types::AccountLabel>,
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
    app.pending_browser_login = Some(
        crate::browser_login::PendingBrowserLogin::spawn(
            provider.id(),
            provider.name(),
            login.await_credentials(),
        )
        .for_account(account),
    );
}

fn start_paste_login(
    app: &mut App,
    provider: &dyn mush_ai::oauth::OAuthProvider,
    provider_id: &str,
    account: Option<mush_ai::types::AccountLabel>,
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
        account,
        pkce,
    });
}

fn open_login_picker_overlay(app: &mut App, tui_config: &TuiConfig) {
    let entries = build_login_entries(tui_config);
    app.open_login_picker(entries, tui_config.provider_api_keys.clone());
}

pub(crate) fn build_login_entries(tui_config: &TuiConfig) -> Vec<crate::login_picker::LoginEntry> {
    use mush_ai::login;

    let oauth_present = login::oauth_logins();
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
                    login::oauth_account_id(oauth_provider_id, None)
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
    let arg_list: Vec<&str> = args.split_whitespace().collect();
    let expanded = mush_ext::substitute_args(&tmpl.content, &arg_list);
    app.push_user_message(expanded.clone());
    Some(expanded)
}

pub fn expand_skill(skill: &mush_ext::Skill, args: &str) -> Result<String, std::io::Error> {
    let content = std::fs::read_to_string(&skill.path)?;
    let body = mush_ext::frontmatter_body(&content)
        .unwrap_or(&content)
        .trim();
    let base = skill
        .path
        .parent()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {base}.\n\n{body}\n</skill>",
        skill.name,
        skill.path.display(),
    );
    Ok(if args.trim().is_empty() {
        block
    } else {
        format!("{block}\n\n{}", args.trim())
    })
}

fn try_skill(app: &mut App, skills: &[mush_ext::Skill], name: &str, args: &str) -> Option<String> {
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        app.push_system_message(format!("no skill called {name}. /skills lists them"));
        return None;
    };
    match expand_skill(skill, args) {
        Ok(expanded) => {
            app.push_user_message(expanded.clone());
            Some(expanded)
        }
        Err(error) => {
            app.push_system_message(format!("could not read {}: {error}", skill.path.display()));
            None
        }
    }
}

pub fn expand_template(prompt: &str) -> String {
    let Some(command) = prompt.strip_prefix('/') else {
        return prompt.to_string();
    };

    let cwd = std::env::current_dir().unwrap_or_default();
    let templates = mush_ext::discover_templates(&cwd);

    let (name, args) = command.split_once(' ').unwrap_or((command, ""));
    match mush_ext::find_template(&templates, name) {
        Some(tmpl) => {
            let args: Vec<&str> = args.split_whitespace().collect();
            mush_ext::substitute_args(&tmpl.content, &args)
        }
        None => prompt.to_string(),
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
                let _ = write!(md, "## non-human event\n\n{}\n\n", event.audit_text());
            }
            Message::User(u) => {
                let _ = write!(md, "## you\n\n{}\n\n", u.text());
            }
            Message::Assistant(a) => {
                let _ = write!(md, "## {}\n\n{}\n\n", a.model.as_ref(), a.text());
            }
            Message::ToolResult(tr) => {
                let output: String = tr
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let preview = truncate_with_ellipsis(&output, 200);
                let _ = write!(
                    md,
                    "**{}** `{}`\n\n```\n{preview}\n```\n\n",
                    if tr.outcome.is_error() { "✗" } else { "✓" },
                    tr.tool_name,
                );
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
    fn effort_sets_a_known_level_and_names_the_choices_otherwise() {
        let mut app = App::new("test".into(), mush_ai::types::TokenCount::new(200_000));
        app.supported_thinking_levels = vec![ThinkingLevel::Low, ThinkingLevel::High];

        handle_effort(&mut app, true, Some("high"));
        assert_eq!(app.thinking_level, ThinkingLevel::High);
        assert_eq!(
            app.status.as_ref().map(|status| status.text.as_str()),
            Some("Thinking level: high")
        );

        handle_effort(&mut app, true, Some("xhigh"));
        assert_eq!(app.thinking_level, ThinkingLevel::High);
        assert_eq!(
            app.messages.last().unwrap().content,
            "Unknown thinking level 'xhigh'. Available: off, low, high"
        );

        handle_effort(&mut app, false, Some("low"));
        assert_eq!(app.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn every_help_command_parses_as_a_builtin() {
        let names: Vec<&str> = HELP_TEXT
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix('/'))
            .filter_map(|rest| rest.split([' ', '<', '[']).next())
            .filter(|name| !name.starts_with("skill:"))
            .collect();
        assert!(names.len() > 20, "{names:?}");
        for name in names {
            assert!(
                !matches!(
                    crate::slash::parse(&format!("/{name}")),
                    Ok(SlashAction::Other { .. })
                ),
                "/{name} is in /help but does not parse"
            );
        }
    }

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
            paused: None,
            id: format!("sub-{name}"),
            name: name.into(),
            parent: None,
            depth: 1,
            session_id: "s".into(),
            model_id: "claude-opus-4-7".into(),
            thinking: None,
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
            streaming: false,
            tool_running: None,
            awaiting_confirmation: None,
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
    fn a_settled_child_only_moves_the_count_and_cost_folds_it_in() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.note_child(child("auth", mush_runtime::ChildStatus::Running));
        assert!(app.status.is_none());
        assert_eq!(app.running_children(), 1);

        app.note_child(child("auth", mush_runtime::ChildStatus::Idle));
        assert!(app.status.is_none());
        assert_eq!(app.running_children(), 0);
        assert_eq!(app.children.len(), 1);

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

    #[test]
    fn switching_models_keeps_the_session_thinking_level_clamped() {
        let mut config = test_tui_config(None);
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.thinking_level = ThinkingLevel::High;
        assert!(handle_model_switch(&mut app, &mut config, "gpt-5.2"));
        let model = mush_ai::discovery::find_merged_model_by_id("gpt-5.2")
            .unwrap()
            .model;
        assert_eq!(
            app.thinking_level,
            model
                .clamp_thinking(&ThinkingLevel::High)
                .normalize_visible()
        );
        assert_ne!(app.thinking_level, ThinkingLevel::Off);
    }

    async fn name_session(app: &mut App, title: Option<&str>) {
        let mut config = test_tui_config(None);
        let mut connection =
            mush_runtime::InProcessConnection::detached(mush_runtime::SessionRuntime::default());
        handle(
            app,
            &mut connection,
            &mut config,
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
                    python: None,
                },
                mush_ext::Skill {
                    name: "deploy".into(),
                    description: "deploy things".into(),
                    path: "/repo/.mush/skills/deploy/SKILL.md".into(),
                    source: mush_ext::SkillSource::Project,
                    disable_model_invocation: true,
                    python: None,
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

    fn skill_on_disk(dir: &std::path::Path, name: &str, body: &str) -> mush_ext::Skill {
        let path = dir.join(format!("{name}/SKILL.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: does {name} things\n---\n\n{body}\n"),
        )
        .unwrap();
        mush_ext::Skill {
            name: name.to_string(),
            description: format!("does {name} things"),
            path,
            source: mush_ext::SkillSource::User,
            disable_model_invocation: false,
            python: None,
        }
    }

    #[test]
    fn a_skill_command_expands_to_a_skill_block_with_the_arguments_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let skill = skill_on_disk(dir.path(), "anki", "# anki\n\nmake good cards");

        let block = expand_skill(&skill, "  make ten on rust  ").unwrap();

        assert!(
            block.starts_with(&format!(
                "<skill name=\"anki\" location=\"{}\">",
                skill.path.display()
            )),
            "{block}"
        );
        assert!(
            block.contains(&format!(
                "References are relative to {}.",
                skill.path.parent().unwrap().display()
            )),
            "{block}"
        );
        assert!(block.contains("# anki\n\nmake good cards"), "{block}");
        assert!(!block.contains("description: does anki things"), "{block}");
        assert!(block.ends_with("</skill>\n\nmake ten on rust"), "{block}");

        let bare = expand_skill(&skill, "").unwrap();
        assert!(bare.ends_with("</skill>"), "{bare}");
    }

    #[test]
    fn a_skill_command_sends_the_block_as_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let skills = vec![skill_on_disk(dir.path(), "jj", "use jj not git")];
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        let prompt = try_skill(&mut app, &skills, "jj", "commit it").unwrap();

        assert!(prompt.contains("use jj not git"), "{prompt}");
        assert_eq!(app.messages.last().unwrap().content, prompt);
        assert_eq!(
            app.messages.last().unwrap().role,
            crate::display_types::MessageRole::User
        );
    }

    #[test]
    fn an_unknown_skill_says_so_and_sends_nothing() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        assert!(try_skill(&mut app, &[], "nope", "").is_none());

        let message = app.messages.last().unwrap();
        assert_eq!(message.role, crate::display_types::MessageRole::System);
        assert!(
            message.content.contains("no skill called nope"),
            "{message:?}"
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
