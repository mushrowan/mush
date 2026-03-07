//! slash command handling for the TUI

use mush_ai::models;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_session::tree::SessionTree;

use crate::TuiConfig;
use crate::app::App;
use crate::runner::HintMode;

/// handle a slash command, returning Some(prompt) if it should trigger the agent
pub fn handle(
    app: &mut App,
    conversation: &mut Vec<Message>,
    session_tree: &mut SessionTree,
    tui_config: &mut TuiConfig,
    thinking_prefs: &std::collections::HashMap<String, ThinkingLevel>,
    name: &str,
    args: &str,
) -> Option<String> {
    match name {
        "help" => {
            let mut help = String::from("available commands:\n");
            help.push_str("  /help          - show this message\n");
            help.push_str("  /keys          - show keyboard shortcuts\n");
            help.push_str("  /clear         - clear conversation\n");
            help.push_str("  /model [id]    - show or switch model\n");
            help.push_str("  /sessions      - browse and resume sessions\n");
            help.push_str("  /branch [n]    - branch from nth user message\n");
            help.push_str("  /tree          - show conversation tree\n");
            help.push_str("  /compact       - summarise old messages to free context\n");
            help.push_str("  /export [file] - save conversation as markdown\n");
            help.push_str("  /undo          - revert last turn\n");
            help.push_str("  /search [text] - search conversation (or ctrl+f)\n");
            help.push_str("  /cost          - show session cost\n");
            help.push_str("  /logs [n]      - show last n log entries (default 50)\n");
            help.push_str("  /injection     - toggle prompt injection preview\n");
            help.push_str("  /close         - close focused pane\n");
            help.push_str("  /broadcast msg - send a message to all panes\n");
            help.push_str("  /lock <path>   - lock a file for this pane\n");
            help.push_str("  /unlock <path> - release a file lock\n");
            help.push_str("  /locks         - list all file locks\n");
            help.push_str("  /label [text]  - set pane label (or auto-generate)\n");
            help.push_str("  /panes         - list all panes with status\n");
            help.push_str("  /quit          - exit mush\n");
            help.push_str("\ntip: type a prompt template name (e.g. /review file.rs) to expand it");
            app.push_system_message(help);
            None
        }
        "keys" => {
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
            keys.push_str("\nmode switches:\n");
            keys.push_str("  ctrl+s         - scroll/copy mode\n");
            keys.push_str("  ctrl+f         - search\n");
            keys.push_str("  ctrl+t         - cycle thinking level\n");
            keys.push_str("  ctrl+o         - toggle thinking text visibility\n");
            keys.push_str("  ctrl+i         - toggle prompt injection preview\n");
            keys.push_str("\nscroll mode:\n");
            keys.push_str("  j/k            - scroll down/up\n");
            keys.push_str("  g/G            - jump to top/bottom\n");
            keys.push_str("  y              - copy selected message\n");
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
        "clear" => {
            app.clear_messages();
            conversation.clear();
            *session_tree = SessionTree::new();
            app.status = Some("conversation cleared".into());
            None
        }
        "tree" => {
            show_tree(app, session_tree);
            None
        }
        "branch" if !args.is_empty() => {
            handle_branch(app, conversation, session_tree, args);
            None
        }
        "branch" => {
            app.push_system_message(
                "usage: /branch <n> — branch from nth user message\ntry /tree to see messages",
            );
            None
        }
        "sessions" => {
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
        "resume" if !args.is_empty() => {
            let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
            let id = mush_session::SessionId::from(args.trim());
            match store.load(&id) {
                Ok(session) => {
                    *conversation = session.messages.clone();
                    *session_tree = session.tree;
                    rebuild_display(app, conversation);
                    let title = session.meta.title.as_deref().unwrap_or("untitled");
                    app.status = Some(format!("resumed: {title}"));
                }
                Err(e) => app.push_system_message(format!("failed to load session: {e}")),
            }
            None
        }
        "model" if args.is_empty() => {
            app.push_system_message(format!("model: {}", app.model_id));
            None
        }
        "model" => {
            handle_model_switch(app, tui_config, thinking_prefs, args);
            None
        }
        "cost" => {
            app.show_cost = !app.show_cost;
            show_cost(app);
            None
        }
        "logs" => {
            let n = args.trim().parse::<usize>().unwrap_or(50);
            if let Some(ref buf) = tui_config.log_buffer {
                let entries = buf(n);
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
        "injection" => {
            app.show_prompt_injection = !app.show_prompt_injection;
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
                if app.show_prompt_injection {
                    "on"
                } else {
                    "off"
                }
            ));
            None
        }
        "undo" => {
            handle_undo(app, conversation, session_tree);
            None
        }
        "quit" | "exit" | "q" => {
            app.should_quit = true;
            None
        }
        other => try_template(app, other, args),
    }
}

fn show_tree(app: &mut App, session_tree: &SessionTree) {
    let user_msgs: Vec<_> = session_tree
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
        session_tree.len(),
        session_tree
            .entries()
            .iter()
            .filter(|e| session_tree.is_branch_point(&e.id))
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
            let marker = if session_tree.is_branch_point(&entry.id) {
                " ⑂"
            } else {
                ""
            };
            info.push_str(&format!("  {}: {preview}{marker}\n", i + 1));
        }
    }
    app.push_system_message(info);
}

fn handle_branch(
    app: &mut App,
    conversation: &mut Vec<Message>,
    session_tree: &mut SessionTree,
    args: &str,
) {
    let Ok(n) = args.trim().parse::<usize>() else {
        app.push_system_message("usage: /branch <number> (try /tree first)");
        return;
    };

    let user_msgs = session_tree.user_messages_in_branch();
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
            session_tree.branch(pid);
        } else {
            session_tree.reset_leaf();
        }
        *conversation = session_tree.build_context();
        rebuild_display(app, conversation);
        app.status = Some(format!("branched before: {preview}"));
    }
}

fn handle_model_switch(
    app: &mut App,
    tui_config: &mut TuiConfig,
    thinking_prefs: &std::collections::HashMap<String, ThinkingLevel>,
    args: &str,
) {
    let id = args.trim();
    if let Some(new_model) = models::find_model_by_id(id) {
        app.model_id = id.into();
        app.stats.context_window = new_model.context_window;
        app.cache_ttl_secs = if tui_config.cache_timer {
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
            .unwrap_or(ThinkingLevel::Off);
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
        app.push_system_message(format!("unknown model: {id}\n\navailable:\n{available}"));
    }
}

fn show_cost(app: &mut App) {
    let ctx = if app.stats.context_tokens > 0 {
        let pct =
            (app.stats.context_tokens as f64 / app.stats.context_window as f64 * 100.0) as u64;
        format!(
            "context: {}k/{}k ({}%)\n",
            app.stats.context_tokens / 1000,
            app.stats.context_window / 1000,
            pct
        )
    } else {
        String::new()
    };

    let reuse_base = app.stats.cache_read_tokens + app.stats.input_tokens;
    let reuse_pct = if reuse_base > 0 {
        (app.stats.cache_read_tokens as f64 / reuse_base as f64 * 100.0) as u64
    } else {
        0
    };

    let total_input =
        app.stats.cache_read_tokens + app.stats.cache_write_tokens + app.stats.input_tokens;
    let write_pct = if total_input > 0 {
        (app.stats.cache_write_tokens as f64 / total_input as f64 * 100.0) as u64
    } else {
        0
    };

    app.push_system_message(format!(
        "{}cumulative: ↑{} ↓{} R{} W{} | reuse {}% write {}% | {}tok, ${:.4}",
        ctx,
        app.stats.input_tokens,
        app.stats.output_tokens,
        app.stats.cache_read_tokens,
        app.stats.cache_write_tokens,
        reuse_pct,
        write_pct,
        app.stats.total_tokens,
        app.stats.total_cost
    ));
}

fn handle_undo(app: &mut App, conversation: &mut Vec<Message>, session_tree: &mut SessionTree) {
    let parent = session_tree
        .user_messages_in_branch()
        .last()
        .map(|e| e.parent_id.clone());
    match parent {
        None => app.push_system_message("nothing to undo"),
        Some(None) => {
            session_tree.reset_leaf();
            *conversation = session_tree.build_context();
            rebuild_display(app, conversation);
            app.status = Some("undid last turn".into());
        }
        Some(Some(pid)) => {
            session_tree.branch(&pid);
            *conversation = session_tree.build_context();
            rebuild_display(app, conversation);
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
    app.clear_messages();
    for msg in conversation {
        match msg {
            Message::User(u) => {
                let text = match &u.content {
                    UserContent::Text(t) => t.clone(),
                    UserContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|p| match p {
                            UserContentPart::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                app.push_user_message(text);
            }
            Message::Assistant(a) => {
                let text = a
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContentPart::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                app.start_streaming();
                app.push_text_delta(&text);
                app.finish_streaming(Some(a.usage), None);
            }
            _ => {}
        }
    }
}

/// export conversation to a markdown file
pub fn handle_export(app: &mut App, conversation: &[Message], args: &str) {
    let path = if args.trim().is_empty() {
        "conversation.md".to_string()
    } else {
        args.trim().to_string()
    };

    let mut md = String::new();
    for msg in conversation {
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

/// run LLM compaction on the conversation
pub async fn handle_compact(
    app: &mut App,
    conversation: &mut Vec<Message>,
    session_tree: &mut SessionTree,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
) {
    use mush_session::compact;

    let before = conversation.len();
    if before <= 4 {
        app.push_system_message("conversation too short to compact");
        return;
    }

    app.status = Some("compacting...".into());
    let result =
        compact::llm_compact(conversation.clone(), registry, model, options, Some(10)).await;

    *conversation = result.messages;
    *session_tree = SessionTree::new();
    for msg in conversation.iter() {
        session_tree.append_message(msg.clone());
    }
    rebuild_display(app, conversation);
    app.status = Some(format!(
        "compacted: {before} → {} messages ({} summarised)",
        conversation.len(),
        result.summarised_count,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_cost_includes_reuse_and_write_percentages() {
        let mut app = App::new("test".into(), 200_000);
        app.stats.input_tokens = 100;
        app.stats.output_tokens = 50;
        app.stats.cache_read_tokens = 150;
        app.stats.cache_write_tokens = 50;
        app.stats.total_tokens = 350;
        app.stats.total_cost = 0.0123;

        show_cost(&mut app);

        let msg = app.messages.last().unwrap();
        assert!(msg.content.contains("reuse 60%"));
        assert!(msg.content.contains("write 16%"));
    }
}
