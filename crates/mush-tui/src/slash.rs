//! slash command handling for the TUI

use std::fmt::Write;

use mush_ai::models;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_session::ConversationState;
use thiserror::Error;

use crate::TuiConfig;
use crate::app::App;
use crate::runner::HintMode;

/// minimum messages before compaction is worthwhile
const MIN_MESSAGES_FOR_COMPACTION: usize = 4;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SlashParseError {
    #[error("slash commands must start with /")]
    MissingPrefix,
    #[error("usage: /resume <session-id>")]
    ResumeUsage,
    #[error("usage: /branch <number> (try /tree first)")]
    BranchUsage,
    #[error("usage: /logs [n]")]
    LogsUsage,
    #[error("usage: /broadcast <message>")]
    BroadcastUsage,
    #[error("usage: /lock <path>")]
    LockUsage,
    #[error("usage: /unlock <path>")]
    UnlockUsage,
    #[error("usage: /task claim <id> <description> | /task release <id> | /task list")]
    TaskUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    Help,
    Keys,
    Clear,
    Model { model_id: Option<String> },
    Sessions,
    Resume { session_id: SessionId },
    Branch { index: Option<usize> },
    Tree,
    Compact,
    ForkCompact,
    Export { path: Option<String> },
    Undo,
    Search { query: String },
    Cost,
    Logs { count: usize },
    Injection,
    Close,
    Broadcast { message: String },
    Lock { path: String },
    Unlock { path: String },
    Locks,
    Label { text: Option<String> },
    Panes,
    Merge,
    Card,
    TaskClaim { id: String, description: String },
    TaskRelease { id: String },
    TaskList,
    Quit,
    Other { name: String, args: String },
}

pub fn parse(input: &str) -> Result<SlashAction, SlashParseError> {
    let Some(command) = input.strip_prefix('/') else {
        return Err(SlashParseError::MissingPrefix);
    };

    let (name, args) = split_name_and_args(command);
    match name {
        "help" => Ok(SlashAction::Help),
        "keys" => Ok(SlashAction::Keys),
        "clear" => Ok(SlashAction::Clear),
        "model" => Ok(SlashAction::Model {
            model_id: (!args.is_empty()).then(|| args.to_string()),
        }),
        "sessions" => Ok(SlashAction::Sessions),
        "resume" if args.is_empty() => Err(SlashParseError::ResumeUsage),
        "resume" => Ok(SlashAction::Resume {
            session_id: SessionId::from(args),
        }),
        "branch" if args.is_empty() => Ok(SlashAction::Branch { index: None }),
        "branch" => args
            .parse::<usize>()
            .map(|index| SlashAction::Branch { index: Some(index) })
            .map_err(|_| SlashParseError::BranchUsage),
        "tree" => Ok(SlashAction::Tree),
        "compact" => Ok(SlashAction::Compact),
        "fork-compact" | "fc" => Ok(SlashAction::ForkCompact),
        "export" => Ok(SlashAction::Export {
            path: (!args.is_empty()).then(|| args.to_string()),
        }),
        "undo" => Ok(SlashAction::Undo),
        "search" => Ok(SlashAction::Search {
            query: args.to_string(),
        }),
        "cost" => Ok(SlashAction::Cost),
        "card" => Ok(SlashAction::Card),
        "logs" if args.is_empty() => Ok(SlashAction::Logs { count: 50 }),
        "logs" => args
            .parse::<usize>()
            .map(|count| SlashAction::Logs { count })
            .map_err(|_| SlashParseError::LogsUsage),
        "injection" => Ok(SlashAction::Injection),
        "close" => Ok(SlashAction::Close),
        "broadcast" if args.is_empty() => Err(SlashParseError::BroadcastUsage),
        "broadcast" => Ok(SlashAction::Broadcast {
            message: args.to_string(),
        }),
        "lock" if args.is_empty() => Err(SlashParseError::LockUsage),
        "lock" => Ok(SlashAction::Lock {
            path: args.to_string(),
        }),
        "unlock" if args.is_empty() => Err(SlashParseError::UnlockUsage),
        "unlock" => Ok(SlashAction::Unlock {
            path: args.to_string(),
        }),
        "locks" => Ok(SlashAction::Locks),
        "label" => Ok(SlashAction::Label {
            text: (!args.is_empty()).then(|| args.to_string()),
        }),
        "panes" => Ok(SlashAction::Panes),
        "merge" => Ok(SlashAction::Merge),
        "task" | "tasks" => parse_task_subcommand(args),
        "quit" | "exit" | "q" => Ok(SlashAction::Quit),
        other => Ok(SlashAction::Other {
            name: other.to_string(),
            args: args.to_string(),
        }),
    }
}

fn parse_task_subcommand(args: &str) -> Result<SlashAction, SlashParseError> {
    let (sub, rest) = split_name_and_args(args);
    match sub {
        "claim" => {
            let (id, description) = split_name_and_args(rest);
            if id.is_empty() || description.is_empty() {
                Err(SlashParseError::TaskUsage)
            } else {
                Ok(SlashAction::TaskClaim {
                    id: id.to_string(),
                    description: description.to_string(),
                })
            }
        }
        "release" if !rest.is_empty() => Ok(SlashAction::TaskRelease {
            id: rest.to_string(),
        }),
        "list" | "" => Ok(SlashAction::TaskList),
        _ => Err(SlashParseError::TaskUsage),
    }
}

fn split_name_and_args(command: &str) -> (&str, &str) {
    let trimmed = command.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (trimmed, ""),
    }
}

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
            help.push_str("  /clear         - clear conversation\n");
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
        SlashAction::Clear => {
            app.clear_messages();
            conversation.replace_messages(vec![]);
            app.status = Some("conversation cleared".into());
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
            app.mode = crate::app::AppMode::Search;
            app.search.query = query.clone();
            app.update_search();
            None
        }
        SlashAction::Cost => {
            app.show_cost = !app.show_cost;
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
    // resolve tier name (e.g. "fast" → "claude-3-5-haiku-...") or use as-is
    let id = tui_config
        .model_tiers
        .get(raw)
        .map(|s| s.as_str())
        .unwrap_or(raw);
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
                msg.push_str(&format!("\n  {tier} → {model_id}"));
            }
        }
        app.push_system_message(msg);
    }
}

fn show_cost(app: &mut App) {
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

    let reuse_base = s.cache_read_tokens + s.input_tokens;
    let reuse_pct = if reuse_base > TokenCount::ZERO {
        s.cache_read_tokens.percent_of(reuse_base) as u64
    } else {
        0
    };

    let total_input = s.cache_read_tokens + s.cache_write_tokens + s.input_tokens;
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

/// run LLM compaction on the conversation
pub async fn handle_compact(
    app: &mut App,
    conversation: &mut ConversationState,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) {
    let messages = conversation.context();
    let before = messages.len();
    if before <= MIN_MESSAGES_FOR_COMPACTION {
        app.push_system_message("conversation too short to compact");
        return;
    }

    app.status = Some("compacting...".into());
    let (compacted_messages, tokens_before, tokens_after, summarised_count) =
        run_compaction(messages, model, options, registry, lifecycle_hooks, cwd).await;

    conversation.replace_messages(compacted_messages);
    let after = conversation.context();
    rebuild_display(app, &after);
    app.status = Some(format!(
        "compacted: {before} → {} messages, ~{tokens_before} → ~{tokens_after} tokens ({summarised_count} summarised)",
        after.len(),
    ));
}

/// fork the session tree then compact the new branch
///
/// the original conversation is preserved in the parent branch.
/// a summary of the old branch is injected at the fork point so
/// the LLM knows the branch happened.
pub async fn handle_fork_compact(
    app: &mut App,
    conversation: &mut ConversationState,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) {
    let before = conversation.context_len();
    if before <= MIN_MESSAGES_FOR_COMPACTION {
        app.push_system_message("conversation too short to fork-compact");
        return;
    }

    app.status = Some("fork-compacting...".into());
    let result = fork_and_compact(
        conversation,
        "forked",
        model,
        options,
        registry,
        lifecycle_hooks,
        cwd,
    )
    .await;
    match result {
        Some((after, tokens_before, tokens_after)) => {
            rebuild_display(app, &conversation.context());
            app.status = Some(format!(
                "fork-compacted: {before} → {after} messages, ~{tokens_before} → ~{tokens_after} tokens (original preserved, /tree to navigate)",
            ));
        }
        None => app.push_system_message("no conversation to fork"),
    }
}

/// fork the session tree at the current leaf and compact the new branch.
/// returns (after_count, tokens_before, tokens_after) or None if no leaf.
pub async fn fork_and_compact(
    conversation: &mut ConversationState,
    label: &str,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> Option<(usize, usize, usize)> {
    let messages = conversation.context();
    let before = messages.len();

    let leaf_id = conversation.leaf_id().cloned()?;

    conversation.branch_with_summary(
        &leaf_id,
        format!("{label} from branch with {before} messages for compaction"),
    );

    let (compacted_messages, tokens_before, tokens_after, _) =
        run_compaction(messages, model, options, registry, lifecycle_hooks, cwd).await;

    conversation.replace_messages(compacted_messages);
    Some((conversation.context_len(), tokens_before, tokens_after))
}

/// shared compaction + hook logic for /compact, /fork-compact, and auto-fork-compact
///
/// returns (compacted_messages, tokens_before, tokens_after, summarised_count)
pub async fn run_compaction(
    messages: Vec<Message>,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> (Vec<Message>, usize, usize, usize) {
    use mush_session::compact;

    let tokens_before = compact::estimate_tokens(&messages);
    let result = compact::llm_compact(messages, registry, model, options, Some(10)).await;
    let mut compacted = result.messages;

    if let Some(hooks) = lifecycle_hooks
        && !hooks.post_compaction.is_empty()
    {
        let hook_results = hooks.run_post_compaction(cwd).await;
        let output: String = hook_results
            .iter()
            .filter(|r| !r.output.is_empty())
            .map(|r| r.output.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !output.is_empty() {
            compacted.push(Message::User(UserMessage {
                content: UserContent::Text(format!("[post-compaction hook output]\n{output}")),
                timestamp_ms: Timestamp::now(),
            }));
        }
        for r in &hook_results {
            if !r.success {
                tracing::warn!(command = %r.command, "post-compaction hook failed: {}", r.output);
            }
        }
    }

    let tokens_after = compact::estimate_tokens(&compacted);
    (
        compacted,
        tokens_before,
        tokens_after,
        result.summarised_count,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fork_compact() {
        assert_eq!(parse("/fork-compact").unwrap(), SlashAction::ForkCompact);
        assert_eq!(parse("/fc").unwrap(), SlashAction::ForkCompact);
    }

    #[test]
    fn parse_model_action() {
        assert_eq!(
            parse("/model claude-sonnet").unwrap(),
            SlashAction::Model {
                model_id: Some("claude-sonnet".into()),
            }
        );
    }

    #[test]
    fn parse_lock_requires_path() {
        assert_eq!(
            parse("/lock").unwrap_err().to_string(),
            "usage: /lock <path>"
        );
    }

    #[test]
    fn parse_task_claim() {
        assert_eq!(
            parse("/task claim fix-auth rewrite the auth module").unwrap(),
            SlashAction::TaskClaim {
                id: "fix-auth".into(),
                description: "rewrite the auth module".into(),
            }
        );
    }

    #[test]
    fn parse_task_release() {
        assert_eq!(
            parse("/task release fix-auth").unwrap(),
            SlashAction::TaskRelease {
                id: "fix-auth".into(),
            }
        );
    }

    #[test]
    fn parse_task_list() {
        assert_eq!(parse("/task list").unwrap(), SlashAction::TaskList);
        assert_eq!(parse("/task").unwrap(), SlashAction::TaskList);
        assert_eq!(parse("/tasks").unwrap(), SlashAction::TaskList);
    }

    #[test]
    fn parse_task_claim_needs_id_and_description() {
        assert!(parse("/task claim").is_err());
        assert!(parse("/task claim myid").is_err());
    }

    #[test]
    fn parse_task_release_needs_id() {
        assert!(parse("/task release").is_err());
    }

    #[test]
    fn parse_other_command_preserves_args() {
        assert_eq!(
            parse("/review src/main.rs").unwrap(),
            SlashAction::Other {
                name: "review".into(),
                args: "src/main.rs".into(),
            }
        );
    }

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
        assert!(msg.content.contains("reuse 60%"));
        assert!(msg.content.contains("write 16%"));
    }
}
