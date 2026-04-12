use std::path::{Path, PathBuf};
use std::sync::Arc;

use mush_ai::types::{Message, UserContent, UserMessage};
use mush_session::ConversationState;

use crate::app::App;
use crate::event_handler;
use crate::pane::{PaneId, PaneManager};
use crate::slash;

use super::TuiConfig;

pub(super) async fn drain_inboxes(pane_mgr: &mut PaneManager) {
    for pane in pane_mgr.panes_mut() {
        let Some(ref mut inbox) = pane.inbox else {
            continue;
        };
        while let Ok(msg) = inbox.try_recv() {
            let task_suffix = msg
                .task_id
                .as_deref()
                .map(|task| format!(" task={task}"))
                .unwrap_or_default();
            let text = format!(
                "[{} from pane {}{}]: {}",
                msg.intent,
                msg.from.as_u32(),
                task_suffix,
                msg.content,
            );
            let display = format!(
                "↶ {} from pane {}{}: {}",
                msg.intent,
                msg.from.as_u32(),
                task_suffix,
                msg.content,
            );
            if pane.app.stream.active {
                let steering_msg = Message::User(UserMessage {
                    content: UserContent::Text(text.clone()),
                    timestamp_ms: msg.timestamp,
                });
                pane.steering_queue.lock().await.push(steering_msg);
                pane.app.push_system_message(display);
            } else {
                pane.app.push_system_message(display);
                pane.app.push_user_message(text.clone());
                pane.app.start_streaming();
                pane.pending_prompt = Some(text);
            }
        }
    }
}

pub(super) fn build_sibling_prompt(pane_id: PaneId, pane_mgr: &PaneManager) -> String {
    let panes = pane_mgr.panes();
    let total = panes.len();
    let idx = panes
        .iter()
        .position(|pane| pane.id == pane_id)
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut prompt =
        format!("You are pane {idx} of {total} agents working in parallel on the same codebase.");
    prompt.push_str(
        " You have a `send_message` tool to communicate with siblings. \
         Use it to share findings, avoid duplicating work, or ask for help.",
    );

    for pane in panes {
        if pane.id == pane_id {
            continue;
        }
        let pane_idx = panes
            .iter()
            .position(|other| other.id == pane.id)
            .map(|i| i + 1)
            .unwrap_or(0);
        let label = pane.label.as_deref().unwrap_or("idle");
        let label_preview: String = label.chars().take(60).collect();
        prompt.push_str(&format!("\n- Pane {pane_idx}: {label_preview}"));
    }

    prompt
}

pub(super) async fn fork_pane(
    pane_mgr: &mut PaneManager,
    tui_config: &TuiConfig,
    bus: &crate::messaging::MessageBus,
    tool_output_live: &Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
) {
    let prompt = {
        let app = &mut pane_mgr.focused_mut().app;
        let text = app.input.take_text();
        if text.is_empty() {
            None
        } else {
            Some(slash::expand_template(&text))
        }
    };

    let (
        conversation,
        display_msgs,
        model_id,
        context_window,
        thinking_level,
        thinking_display,
        completions,
        slash_commands,
        model_completions,
        cwd,
    ) = {
        let parent = pane_mgr.focused();
        let parent_messages = parent.conversation.context();
        (
            ConversationState::from_messages(event_handler::slim_for_fork(&parent_messages)),
            parent.app.messages.clone(),
            parent.app.model_id.clone(),
            parent.app.stats.context_window,
            parent.app.thinking_level,
            parent.app.thinking_display,
            parent.app.completions.clone(),
            parent.app.slash_commands.clone(),
            parent.app.model_completions.clone(),
            parent.app.cwd.clone(),
        )
    };

    let new_id = pane_mgr.next_id();
    let mut new_app = App::new(model_id.clone(), context_window);
    new_app.messages = display_msgs;
    new_app.thinking_level = thinking_level;
    new_app.thinking_display = thinking_display;
    new_app.completions = completions;
    new_app.slash_commands = slash_commands;
    new_app.model_completions = model_completions;
    new_app.cwd = cwd.clone();
    new_app.show_cost = tui_config.show_cost;
    new_app.cache.ttl_secs = if tui_config.cache_timer {
        crate::app::cache_ttl_secs(
            &tui_config.model.provider,
            tui_config.options.cache_retention.as_ref(),
        )
    } else {
        0
    };

    let mut new_pane = crate::pane::Pane::with_conversation(new_id, new_app, conversation);

    let cwd_path = PathBuf::from(&cwd);
    match tui_config.isolation_mode {
        crate::file_tracker::IsolationMode::Worktree => {
            match crate::isolation::create_worktree(&cwd_path, new_id).await {
                Ok(info) => {
                    let sink: Option<mush_tools::bash::OutputSink> =
                        tool_output_live.as_ref().map(|live| {
                            let live = live.clone();
                            let sink: mush_tools::bash::OutputSink = Arc::new(move |line: &str| {
                                if let Ok(mut guard) = live.lock() {
                                    *guard = Some(line.to_string());
                                }
                            });
                            sink
                        });
                    let use_patch = mush_tools::uses_patch_tool(&model_id);
                    let skip_batch = mush_tools::supports_native_parallel_calls(&model_id);
                    let pane_http = tui_config.http_client.clone().unwrap_or_default();
                    let mut pane_tools = mush_tools::builtin_tools_with_options(
                        info.path.clone(),
                        sink,
                        use_patch,
                        pane_http,
                    );
                    if !skip_batch {
                        mush_tools::add_batch_tool(&mut pane_tools);
                    }
                    new_pane.tools = Some(pane_tools);
                    new_pane.cwd_override = Some(info.path.clone());
                    new_pane.app.cwd = info.path.display().to_string();
                    new_pane.isolation = Some(crate::isolation::PaneIsolation::Worktree {
                        path: info.path,
                        branch: info.branch,
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to create worktree");
                }
            }
        }
        crate::file_tracker::IsolationMode::Jj => {
            match crate::isolation::create_jj_change(&cwd_path).await {
                Ok(info) => {
                    new_pane.isolation = Some(crate::isolation::PaneIsolation::Jj {
                        change_id: info.change_id,
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to create jj change");
                }
            }
        }
        crate::file_tracker::IsolationMode::None => {}
    }

    let inbox = bus.register(new_id);
    new_pane.inbox = Some(inbox);

    {
        let parent = pane_mgr.focused_mut();
        if parent.label.is_none() {
            let fallback = parent
                .conversation
                .context()
                .into_iter()
                .find_map(|message| match message {
                    mush_ai::Message::User(user) => {
                        let text = user.content.text();
                        if text.is_empty() {
                            None
                        } else {
                            Some(text.chars().take(30).collect::<String>())
                        }
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "original".into());
            parent.label = Some(fallback);
        }
    }

    if let Some(ref text) = prompt {
        new_pane.app.push_user_message(text.clone());
        new_pane.app.active_tools.clear();
        new_pane.app.start_streaming();
        new_pane.pending_prompt = Some(text.clone());
        new_pane.label = Some(text.chars().take(20).collect());
    }

    let isolation_label = match &new_pane.isolation {
        Some(crate::isolation::PaneIsolation::Worktree { branch, .. }) => {
            format!(" (worktree: {branch})")
        }
        Some(crate::isolation::PaneIsolation::Jj { change_id }) => {
            let short = &change_id[..change_id.len().min(8)];
            format!(" (jj: {short})")
        }
        None => String::new(),
    };

    let idx = pane_mgr.add_pane(new_pane);
    pane_mgr.focus_index(idx);

    pane_mgr.focused_mut().app.status = Some(if prompt.is_some() {
        format!("forked conversation{isolation_label}")
    } else {
        format!("forked conversation (idle){isolation_label}")
    });
}

/// process pending delegations: fork a pane for each and inject the task as prompt
pub(super) fn process_delegations(
    pane_mgr: &mut PaneManager,
    tui_config: &TuiConfig,
    bus: &crate::messaging::MessageBus,
    queue: &crate::delegate::DelegationQueue,
) {
    let delegations: Vec<_> = {
        let mut q = queue.lock().unwrap();
        std::mem::take(&mut *q)
    };

    for del in delegations {
        let new_id = pane_mgr.next_id();
        let mut new_app = app_from_parent(pane_mgr.focused(), tui_config);

        // resolve model tier or direct model id
        if let Some(ref model_spec) = del.model {
            let model_id = resolve_model_tier(model_spec, &tui_config.model_tiers);
            if let Some(model) = mush_ai::models::find_model_by_id(&model_id) {
                new_app.model_id = model.id.clone();
                new_app.stats.context_window = model.context_window;
            } else {
                // unknown model, set id anyway (will fail at stream time with a clear error)
                new_app.model_id = model_id.into();
            }
        }

        let mut new_pane = crate::pane::Pane::new(new_id, new_app);
        new_pane.label = Some(format!("task: {}", truncate_label(&del.task)));
        new_pane.inbox = Some(bus.register(new_id));
        new_pane.pending_prompt = Some(del.task);
        new_pane.delegation = Some(crate::pane::DelegationInfo {
            from_pane: del.from,
            task_id: del.task_id.clone(),
        });
        pane_mgr.add_pane(new_pane);

        // notify the delegating pane
        let _ = bus.send(
            del.from,
            crate::messaging::InterPaneMessage {
                from: new_id,
                to: Some(del.from),
                intent: crate::messaging::MessageIntent::Info,
                content: format!(
                    "sub-agent pane {} started working on task_id={}",
                    new_id.as_u32(),
                    del.task_id
                ),
                task_id: Some(del.task_id),
                timestamp: mush_ai::types::Timestamp::now(),
            },
        );
    }
}

/// create a fresh App inheriting display settings from a parent pane
fn app_from_parent(parent: &crate::pane::Pane, tui_config: &TuiConfig) -> App {
    let mut app = App::new(parent.app.model_id.clone(), parent.app.stats.context_window);
    app.thinking_level = parent.app.thinking_level;
    app.thinking_display = parent.app.thinking_display;
    app.completions = parent.app.completions.clone();
    app.slash_commands = parent.app.slash_commands.clone();
    app.model_completions = parent.app.model_completions.clone();
    app.cwd = parent.app.cwd.clone();
    app.show_cost = tui_config.show_cost;
    app
}

/// resolve a model spec: check tier map first, fall back to literal model id
fn resolve_model_tier(spec: &str, tiers: &std::collections::HashMap<String, String>) -> String {
    tiers.get(spec).cloned().unwrap_or_else(|| spec.to_string())
}

fn truncate_label(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() <= 40 {
        first_line.to_string()
    } else {
        format!("{}…", &first_line[..39])
    }
}

pub(super) async fn close_focused_pane(
    pane_mgr: &mut PaneManager,
    message_bus: &crate::messaging::MessageBus,
    file_tracker: &crate::file_tracker::FileTracker,
    cwd: &Path,
) -> bool {
    if !pane_mgr.is_multi_pane() {
        pane_mgr.focused_mut().app.status = Some("can't close the last pane".into());
        return false;
    }

    let closed_id = pane_mgr.focused().id;
    let isolation = pane_mgr.focused().isolation.clone();
    file_tracker.release_pane(closed_id);
    message_bus.unregister(closed_id);
    cleanup_pane_isolation(cwd, &isolation).await;
    pane_mgr.close_focused();
    true
}

pub(super) async fn cleanup_pane_isolation(
    cwd: &Path,
    isolation: &Option<crate::isolation::PaneIsolation>,
) {
    match isolation {
        Some(crate::isolation::PaneIsolation::Worktree { path, .. }) => {
            if let Some(name) = path.file_name().and_then(|name| name.to_str())
                && let Some(id_str) = name.strip_prefix("pane-")
                && let Ok(id) = id_str.parse::<u32>()
                && let Err(error) =
                    crate::isolation::remove_worktree(cwd, crate::pane::PaneId::new(id)).await
            {
                tracing::warn!(%error, "failed to remove worktree");
            }
        }
        Some(crate::isolation::PaneIsolation::Jj { change_id }) => {
            if let Err(error) = crate::isolation::abandon_jj_change(cwd, change_id).await {
                tracing::warn!(%error, "failed to abandon jj change");
            }
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mush_ai::types::{ModelId, TokenCount};

    use crate::file_tracker::FileTracker;
    use crate::pane::Pane;

    fn app() -> App {
        App::new(ModelId::from("test-model"), TokenCount::new(4096))
    }

    #[tokio::test]
    async fn close_focused_pane_rejects_last_pane() {
        let mut pane_mgr = PaneManager::new(Pane::new(PaneId::new(1), app()));
        let message_bus = crate::messaging::MessageBus::new();
        let file_tracker = FileTracker::new(Path::new("/tmp").to_path_buf());

        let closed = close_focused_pane(
            &mut pane_mgr,
            &message_bus,
            &file_tracker,
            Path::new("/tmp"),
        )
        .await;

        assert!(!closed);
        assert_eq!(pane_mgr.panes().len(), 1);
        assert_eq!(
            pane_mgr.focused().app.status.as_deref(),
            Some("can't close the last pane")
        );
    }

    #[tokio::test]
    async fn close_focused_pane_closes_multi_pane_focus() {
        let mut pane_mgr = PaneManager::new(Pane::new(PaneId::new(1), app()));
        pane_mgr.add_pane(Pane::new(PaneId::new(2), app()));
        pane_mgr.focus_index(1);

        let message_bus = crate::messaging::MessageBus::new();
        message_bus.register(PaneId::new(1));
        message_bus.register(PaneId::new(2));

        let file_tracker = FileTracker::new(Path::new("/tmp").to_path_buf());
        let closed = close_focused_pane(
            &mut pane_mgr,
            &message_bus,
            &file_tracker,
            Path::new("/tmp"),
        )
        .await;

        assert!(closed);
        assert_eq!(pane_mgr.panes().len(), 1);
        assert_eq!(pane_mgr.focused().id, PaneId::new(1));
        assert_eq!(message_bus.pane_ids(), vec![PaneId::new(1)]);
    }

    #[test]
    fn process_delegations_marks_pane_with_delegation_info() {
        use crate::delegate;

        let mut pane_mgr = PaneManager::new(Pane::new(PaneId::new(1), app()));
        let bus = crate::messaging::MessageBus::new();
        bus.register(PaneId::new(1));
        let queue = delegate::new_queue();

        queue.lock().unwrap().push(delegate::PendingDelegation {
            task: "review code".into(),
            from: PaneId::new(1),
            task_id: "del-test".into(),
            model: None,
        });

        let model = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        let tui_config = super::TuiConfig {
            model,
            system_prompt: None,
            options: mush_ai::types::StreamOptions::default(),
            max_turns: 32,
            initial_messages: vec![],
            initial_panes: vec![],
            theme: crate::theme::Theme::default(),
            prompt_enricher: None,
            hint_mode: crate::runner::HintMode::Message,
            config_path: None,
            provider_api_keys: std::collections::HashMap::new(),
            thinking_prefs: std::collections::HashMap::new(),
            save_thinking_prefs: None,
            save_last_model: None,
            save_session: None,
            update_title: None,
            confirm_tools: false,
            auto_compact: false,
            auto_fork_compact: false,
            show_cost: false,
            debug_cache: false,
            cache_timer: false,
            thinking_display: crate::app::ThinkingDisplay::Collapse,
            tool_output_live: None,
            log_buffer: None,
            isolation_mode: crate::file_tracker::IsolationMode::None,
            terminal_policy: crate::terminal_policy::TerminalPolicy::default(),
            lifecycle_hooks: mush_agent::LifecycleHooks::default(),
            cwd: std::path::PathBuf::from("/tmp"),
            dynamic_system_context: None,
            file_rules: None,
            lsp_diagnostics: None,
            agent_card: None,
            model_tiers: std::collections::HashMap::new(),
            compaction_model: None,
            http_client: None,
            session_id: mush_ai::types::SessionId::new(),
        };

        process_delegations(&mut pane_mgr, &tui_config, &bus, &queue);

        assert_eq!(pane_mgr.pane_count(), 2);
        let new_pane = &pane_mgr.panes()[1];
        let del = new_pane
            .delegation
            .as_ref()
            .expect("delegation info should be set");
        assert_eq!(del.from_pane, PaneId::new(1));
        assert_eq!(del.task_id, "del-test");
    }

    #[test]
    fn resolve_tier_from_map() {
        let mut tiers = std::collections::HashMap::new();
        tiers.insert("fast".into(), "claude-haiku-3-5-20241022".into());
        tiers.insert("strong".into(), "claude-opus-4-6".into());

        assert_eq!(
            resolve_model_tier("fast", &tiers),
            "claude-haiku-3-5-20241022"
        );
        assert_eq!(resolve_model_tier("strong", &tiers), "claude-opus-4-6");
    }

    #[test]
    fn resolve_tier_falls_back_to_literal() {
        let tiers = std::collections::HashMap::new();
        assert_eq!(resolve_model_tier("gpt-4o", &tiers), "gpt-4o");
    }
}
