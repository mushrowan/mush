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
            if pane.app.is_streaming {
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
        let text = std::mem::take(&mut app.input);
        app.cursor = 0;
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
    new_app.cache_ttl_secs = if tui_config.cache_timer {
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
                    let pane_tools = mush_tools::builtin_tools_with_options(
                        info.path.clone(),
                        sink,
                        use_patch,
                        skip_batch,
                    );
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
}
