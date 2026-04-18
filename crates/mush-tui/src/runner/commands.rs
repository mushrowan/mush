use std::collections::HashMap;
use std::path::Path;

use mush_ai::models;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{Dollars, Message, ThinkingLevel, TokenCount};

use crate::file_tracker::FileTracker;
use crate::pane::PaneManager;
use crate::slash::{self, SlashAction};

use super::TuiConfig;
use super::panes::close_focused_pane;

pub(super) struct SlashEnv<'a> {
    pub tui_config: &'a mut TuiConfig,
    pub thinking_prefs: &'a HashMap<String, ThinkingLevel>,
    pub registry: &'a ApiRegistry,
    pub message_bus: &'a crate::messaging::MessageBus,
    pub file_tracker: &'a FileTracker,
    pub lifecycle_hooks: &'a mush_agent::LifecycleHooks,
    pub cwd: &'a Path,
    pub pending_prompt: &'a mut Option<String>,
}

pub(super) async fn handle_slash_action(
    pane_mgr: &mut PaneManager,
    action: SlashAction,
    env: SlashEnv<'_>,
) -> bool {
    let SlashEnv {
        tui_config,
        thinking_prefs,
        registry,
        message_bus,
        file_tracker,
        lifecycle_hooks,
        cwd,
        pending_prompt,
    } = env;
    let mut state_changed = false;

    match action {
        SlashAction::Compact => {
            let pane = pane_mgr.focused_mut();
            let (app, conversation, _) = pane.fields_mut();
            let (compact_model, compact_options) = tui_config
                .compaction_model
                .as_ref()
                .map(|(m, o)| (m.clone(), o.clone()))
                .unwrap_or_else(|| {
                    let m = models::find_model_by_id(app.model_id.as_str())
                        .unwrap_or_else(|| tui_config.model.clone());
                    (m, tui_config.options.clone())
                });
            slash::handle_compact(
                app,
                conversation,
                &compact_model,
                &compact_options,
                registry,
                Some(lifecycle_hooks),
                Some(cwd),
            )
            .await;
            state_changed = true;
        }
        SlashAction::ForkCompact => {
            let pane = pane_mgr.focused_mut();
            let (app, conversation, _) = pane.fields_mut();
            let (compact_model, compact_options) = tui_config
                .compaction_model
                .as_ref()
                .map(|(m, o)| (m.clone(), o.clone()))
                .unwrap_or_else(|| {
                    let m = models::find_model_by_id(app.model_id.as_str())
                        .unwrap_or_else(|| tui_config.model.clone());
                    (m, tui_config.options.clone())
                });
            slash::handle_fork_compact(
                app,
                conversation,
                &compact_model,
                &compact_options,
                registry,
                Some(lifecycle_hooks),
                Some(cwd),
            )
            .await;
            state_changed = true;
        }
        SlashAction::Export { path } => {
            let pane = pane_mgr.focused_mut();
            slash::handle_export(
                &mut pane.app,
                &pane.conversation,
                path.as_deref().unwrap_or(""),
            );
        }
        SlashAction::LoginComplete { code } => {
            let pane = pane_mgr.focused_mut();
            complete_login(&mut pane.app, code).await;
        }
        SlashAction::Broadcast { message } => {
            if !pane_mgr.is_multi_pane() {
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message("no sibling panes to broadcast to");
            } else {
                let from = pane_mgr.focused().id;
                let stats = message_bus.broadcast(from, message);
                let summary = if stats.dropped == 0 {
                    format!("broadcast sent to {} pane(s)", stats.sent)
                } else {
                    format!(
                        "broadcast sent to {} pane(s), {} dropped",
                        stats.sent, stats.dropped
                    )
                };
                pane_mgr.focused_mut().app.push_system_message(summary);
            }
        }
        SlashAction::Lock { path } => {
            let pane_id = pane_mgr.focused().id;
            match file_tracker.lock(pane_id, &path) {
                Ok(()) => {
                    pane_mgr.focused_mut().app.status = Some(format!("locked {path}"));
                }
                Err(owner) => {
                    pane_mgr.focused_mut().app.status =
                        Some(format!("already locked by pane {}", owner.as_u32()));
                }
            }
        }
        SlashAction::Unlock { path } => {
            let pane_id = pane_mgr.focused().id;
            if file_tracker.unlock(pane_id, &path) {
                pane_mgr.focused_mut().app.status = Some(format!("unlocked {path}"));
            } else {
                pane_mgr.focused_mut().app.status = Some("not locked by this pane".into());
            }
        }
        SlashAction::Locks => {
            let locks = file_tracker.list_locks();
            if locks.is_empty() {
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message("no file locks active");
            } else {
                let mut msg = String::from("file locks:\n");
                for (path, owner) in &locks {
                    msg.push_str(&format!("  {} (pane {})\n", path.display(), owner.as_u32()));
                }
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message(msg.trim_end().to_string());
            }
        }
        SlashAction::Label { text } => {
            let pane_id = pane_mgr.focused().id;
            let label = match text {
                Some(label) => label,
                None => pane_mgr
                    .focused()
                    .conversation
                    .context()
                    .into_iter()
                    .find_map(|message| match message {
                        Message::User(user) => {
                            let text = user.text();
                            if text.is_empty() {
                                None
                            } else {
                                Some(text.chars().take(30).collect::<String>())
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| format!("pane {}", pane_id.as_u32())),
            };
            pane_mgr.focused_mut().label = Some(label.clone());
            pane_mgr.focused_mut().app.status = Some(format!("label: {label}"));
        }
        SlashAction::Panes => {
            let mut msg = String::from("active panes:\n");
            for (i, pane) in pane_mgr.panes().iter().enumerate() {
                let idx = i + 1;
                let label = pane.label.as_deref().unwrap_or("(unlabelled)");
                let status = if pane.app.stream.active {
                    "streaming"
                } else {
                    "idle"
                };
                let model = &pane.app.model_id;
                let cost = if pane.app.stats.total_cost > Dollars::ZERO {
                    format!(" {}", pane.app.stats.total_cost)
                } else {
                    String::new()
                };
                let focused = if i == pane_mgr.focused_index() {
                    " *"
                } else {
                    ""
                };
                msg.push_str(&format!(
                    "  {idx}. {label} [{status}] {model}{cost}{focused}\n"
                ));
            }
            pane_mgr
                .focused_mut()
                .app
                .push_system_message(msg.trim_end().to_string());
        }
        SlashAction::Cost if pane_mgr.is_multi_pane() => {
            let focused = &mut pane_mgr.focused_mut().app;
            focused.interaction.show_cost = !focused.interaction.show_cost;
            let show = focused.interaction.show_cost;
            let mut total_cost = Dollars::ZERO;
            let mut total_tokens = TokenCount::ZERO;
            let mut lines = Vec::new();
            for (i, pane) in pane_mgr.panes().iter().enumerate() {
                let idx = i + 1;
                let label = pane.label.as_deref().unwrap_or("(unlabelled)");
                let stats = &pane.app.stats;
                total_cost += stats.total_cost;
                total_tokens += stats.total_tokens;
                if stats.total_tokens > TokenCount::ZERO {
                    lines.push(format!(
                        "  pane {idx} ({label}): {}tok {}",
                        stats.total_tokens, stats.total_cost
                    ));
                }
            }
            if show {
                let mut msg = format!("total: {}tok {}\n", total_tokens, total_cost);
                for line in &lines {
                    msg.push_str(line);
                    msg.push('\n');
                }
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message(msg.trim_end().to_string());
            }
            for pane in pane_mgr.panes_mut() {
                pane.app.interaction.show_cost = show;
            }
        }
        SlashAction::New => {
            // save current session before clearing
            if let Some(ref saver) = tui_config.save_session {
                saver(super::streams::build_session_snapshot(pane_mgr, tui_config));
            }
            // start a new session with a fresh id
            tui_config.session_id = mush_ai::types::SessionId::new();
            let pane = pane_mgr.focused_mut();
            let (app, conversation, _) = pane.fields_mut();
            app.clear_messages();
            *conversation = mush_session::ConversationState::new();
            app.status = Some("new session started (previous session saved)".into());
            state_changed = true;
        }
        SlashAction::Close => {
            close_focused_pane(pane_mgr, None, message_bus, file_tracker, cwd).await;
        }
        SlashAction::Merge => {
            let pane = pane_mgr.focused();
            match &pane.isolation {
                Some(crate::isolation::PaneIsolation::Worktree { .. }) => {
                    let pane_id = pane.id;
                    match crate::isolation::merge_worktree(cwd, pane_id).await {
                        Ok(info) => {
                            let summary = match info.summary {
                                Some(summary) => format!("merged {}: {summary}", info.branch),
                                None => format!("merged {}", info.branch),
                            };
                            pane_mgr.focused_mut().app.push_system_message(summary);
                        }
                        Err(error) => {
                            pane_mgr
                                .focused_mut()
                                .app
                                .push_system_message(format!("merge failed: {error}"));
                        }
                    }
                }
                Some(crate::isolation::PaneIsolation::Jj { change_id }) => {
                    let change_id = change_id.clone();
                    match crate::isolation::squash_jj_change(cwd, &change_id).await {
                        Ok(info) => {
                            let summary = match info.summary {
                                Some(summary) => format!("squashed jj change: {summary}"),
                                None => "squashed jj change".into(),
                            };
                            pane_mgr.focused_mut().app.push_system_message(summary);
                            pane_mgr.focused_mut().isolation = None;
                        }
                        Err(error) => {
                            pane_mgr
                                .focused_mut()
                                .app
                                .push_system_message(format!("squash failed: {error}"));
                        }
                    }
                }
                None => {
                    pane_mgr.focused_mut().app.status =
                        Some("no isolation to merge (mode is none)".into());
                }
            }
        }
        other_action => {
            let pane = pane_mgr.focused_mut();
            let (app, conversation, _) = pane.fields_mut();
            if let Some(prompt) =
                slash::handle(app, conversation, tui_config, thinking_prefs, &other_action)
            {
                app.start_streaming();
                *pending_prompt = Some(prompt);
            }
            if matches!(other_action, SlashAction::Undo | SlashAction::Branch { .. }) {
                state_changed = true;
            }
        }
    }

    state_changed
}

pub(super) fn save_thinking_pref(
    prefs: &mut HashMap<String, ThinkingLevel>,
    saver: &Option<super::ThinkingPrefsSaver>,
    model_id: &str,
    level: ThinkingLevel,
) {
    prefs.insert(model_id.to_string(), level.normalize_visible());
    if let Some(saver) = saver {
        saver(prefs);
    }
}

/// finish the oauth flow started via /login: exchange the user-supplied
/// code for credentials using the PKCE challenge stashed on the app.
/// on success, save credentials so future runs can pick them up
pub(super) async fn complete_login(app: &mut crate::app::App, code: String) {
    use mush_ai::oauth;

    let Some(pending) = app.pending_oauth.take() else {
        app.push_system_message(
            "no in-progress oauth flow. start one with /login [provider] first",
        );
        return;
    };

    let Some(provider) = oauth::get_provider(&pending.provider_id) else {
        app.push_system_message(format!(
            "oauth provider {} is no longer available",
            pending.provider_id
        ));
        return;
    };

    let trimmed = code.trim();
    if trimmed.is_empty() {
        app.push_system_message("no code provided. usage: /login-complete <code>");
        // keep pending_oauth so they can retry
        app.pending_oauth = Some(pending);
        return;
    }

    match provider.exchange_code(trimmed, &pending.pkce).await {
        Ok(creds) => {
            let mut store = oauth::load_credentials().unwrap_or_default();
            store.providers.insert(pending.provider_id.clone(), creds);
            match oauth::save_credentials(&store) {
                Ok(()) => {
                    app.push_system_message(format!(
                        "✓ logged in to {} (credentials saved)",
                        pending.provider_name
                    ));
                }
                Err(e) => {
                    app.push_system_message(format!(
                        "logged in but saving credentials failed: {e}"
                    ));
                }
            }
        }
        Err(e) => {
            app.push_system_message(format!("oauth login failed: {e}"));
            // restore pending state so user can try a different code
            app.pending_oauth = Some(pending);
        }
    }
}
