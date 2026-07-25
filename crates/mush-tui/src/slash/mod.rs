//! slash command parsing types and entry point
//!
//! execution lives in `commands`, compaction in `compaction`

pub mod commands;
pub mod compaction;

use mush_ai::types::{SessionId, TokenCount, TokenCountParseError};
use thiserror::Error;

// re-export commonly used items so callers can keep using `slash::handle` etc
pub(crate) use commands::handle_login_start;
pub use commands::{expand_template, handle, handle_export, rebuild_display};
pub use compaction::{
    AppliedCompaction, CompactionTaskResult, MIN_MESSAGES_FOR_COMPACTION, PendingCompaction,
    apply_applied_compaction, apply_compaction_result, extract_fresh_summary,
    fallback_applied_compaction, handle_compact, run_compaction, start_compaction,
};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SlashParseError {
    #[error("slash commands must start with /")]
    MissingPrefix,
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
    #[error("usage: /login-complete <code>")]
    LoginCodeUsage,
    #[error("usage: /compaction-model [same|<model-id>]")]
    CompactionModelUsage,
    #[error("usage: /compact [--context-budget <count>] (e.g. 10k, 1024, 1.5k, 1m)")]
    CompactUsage,
    #[error("invalid context budget: {0}")]
    InvalidContextBudget(#[from] TokenCountParseError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    Help,
    Keys,
    Clear,
    New,
    Model {
        model_id: Option<String>,
        /// `true` when the user passed `/model --all` to opt into seeing
        /// codex entries marked `internal` / `experimental`. ignored when
        /// a `model_id` is supplied (the flag only affects the picker)
        show_all: bool,
    },
    Sessions,
    Resume {
        /// `None` means "most recent session in the current cwd"
        session_id: Option<SessionId>,
    },
    Branch {
        index: Option<usize>,
    },
    Tree,
    /// `/compact` - compact the active conversation. preserves the
    /// pre-compaction tree (navigable via `/tree` + `/branch`) and the
    /// visible scrollback. `context_budget` is an optional target
    /// post-compaction summary size, parsed from `--context-budget`/`-b`
    /// (e.g. `10k`, `1024`). when `None`, the summariser uses its
    /// default behaviour
    Compact {
        context_budget: Option<TokenCount>,
    },
    /// view or change the model used for compaction. None shows current,
    /// `same` resets to the active model, any other string resolves as
    /// a model id
    CompactionModel {
        model_id: Option<String>,
    },
    Export {
        path: Option<String>,
    },
    Undo,
    Search {
        query: String,
    },
    Cost,
    /// fetch and display the authenticated openrouter account's
    /// remaining credit balance + lifetime usage. async; rendered as
    /// a system message in the focused pane
    Credits,
    /// fetch and display the list of upstream providers serving a
    /// model on openrouter, plus which of them accept `cache_control`.
    /// async; rendered as a system message in the focused pane. arg
    /// is the model id (`author/slug` form); empty defaults to the
    /// pane's current model
    Endpoints {
        model_id: Option<String>,
    },
    Logs {
        count: usize,
    },
    /// view or modify runtime settings (anthropic betas, scope)
    Settings {
        args: String,
    },
    /// start an oauth login flow for a provider, or open the centred
    /// login picker overlay when no provider is named
    Login {
        provider: Option<String>,
    },
    /// complete the in-progress oauth login with the authorization code
    LoginComplete {
        code: String,
    },
    Close,
    Broadcast {
        message: String,
    },
    Lock {
        path: String,
    },
    Unlock {
        path: String,
    },
    Locks,
    Label {
        text: Option<String>,
    },
    Panes,
    Merge,
    Card,
    TaskClaim {
        id: String,
        description: String,
    },
    TaskRelease {
        id: String,
    },
    TaskList,
    /// dump internal state for debugging (active panes, streams, token stats)
    Debug,
    /// re-discover AGENTS.md and prompt templates without restart.
    /// rebuilds the system prompt and slash menu, busting the provider
    /// prefix cache as a side effect (intentional and explicit)
    Reload,
    /// refresh the discovered-models cache by hitting `/v1/models` for
    /// every provider with credentials configured. surfaces newly
    /// released models in the static-catalogue + discovery merge.
    RefreshModels,
    Quit,
    /// toggle whether the focused assistant message's thinking text is
    /// expanded. a slash command rather than a keybind so the key stays
    /// available for user binds
    ToggleThinkingExpanded,
    Other {
        name: String,
        args: String,
    },
}

impl SlashAction {
    /// whether this command may run while the LLM is actively streaming.
    ///
    /// conservative default: allow. the block list covers actions that
    /// mutate the conversation history or agent state in ways that would
    /// corrupt an in-flight turn (clear, compact, branch switching, etc).
    /// read-only and "apply on next turn" actions are safe to run anytime
    #[must_use]
    pub fn is_safe_during_stream(&self) -> bool {
        !matches!(
            self,
            Self::Clear
                | Self::New
                | Self::Resume { .. }
                | Self::Branch { .. }
                | Self::Compact { .. }
                | Self::Undo
                | Self::Merge
                | Self::Reload
        )
    }
}

pub fn parse(input: &str) -> Result<SlashAction, SlashParseError> {
    let Some(command) = input.strip_prefix('/') else {
        return Err(SlashParseError::MissingPrefix);
    };

    let (name, args) = split_name_and_args(command);
    match name {
        "help" => Ok(SlashAction::Help),
        "keys" => Ok(SlashAction::Keys),
        "new" => Ok(SlashAction::New),
        "model" => {
            // `--all` is the only flag we currently parse for the picker.
            // when supplied, no model id follows. anything else routes to
            // `model_id` so users keep the freedom to type slugs directly
            if args == "--all" {
                Ok(SlashAction::Model {
                    model_id: None,
                    show_all: true,
                })
            } else {
                Ok(SlashAction::Model {
                    model_id: (!args.is_empty()).then(|| args.to_string()),
                    show_all: false,
                })
            }
        }
        "sessions" => Ok(SlashAction::Sessions),
        "resume" if args.is_empty() => Ok(SlashAction::Resume { session_id: None }),
        "resume" => Ok(SlashAction::Resume {
            session_id: Some(SessionId::from(args)),
        }),
        "branch" if args.is_empty() => Ok(SlashAction::Branch { index: None }),
        "branch" => args
            .parse::<usize>()
            .map(|index| SlashAction::Branch { index: Some(index) })
            .map_err(|_| SlashParseError::BranchUsage),
        "tree" => Ok(SlashAction::Tree),
        "compact" => parse_context_budget(args, SlashParseError::CompactUsage)
            .map(|context_budget| SlashAction::Compact { context_budget }),
        "compaction-model" | "compact-model" => Ok(SlashAction::CompactionModel {
            model_id: (!args.is_empty()).then(|| args.to_string()),
        }),
        "export" => Ok(SlashAction::Export {
            path: (!args.is_empty()).then(|| args.to_string()),
        }),
        "undo" => Ok(SlashAction::Undo),
        "search" => Ok(SlashAction::Search {
            query: args.to_string(),
        }),
        "cost" => Ok(SlashAction::Cost),
        "credits" => Ok(SlashAction::Credits),
        "endpoints" => Ok(SlashAction::Endpoints {
            model_id: (!args.is_empty()).then(|| args.to_string()),
        }),
        "card" => Ok(SlashAction::Card),
        "logs" if args.is_empty() => Ok(SlashAction::Logs { count: 50 }),
        "logs" => args
            .parse::<usize>()
            .map(|count| SlashAction::Logs { count })
            .map_err(|_| SlashParseError::LogsUsage),
        "settings" => Ok(SlashAction::Settings {
            args: args.to_string(),
        }),
        "login" => Ok(SlashAction::Login {
            provider: (!args.is_empty()).then(|| args.to_string()),
        }),
        "login-complete" | "login-code" if args.is_empty() => Err(SlashParseError::LoginCodeUsage),
        "login-complete" | "login-code" => Ok(SlashAction::LoginComplete {
            code: args.to_string(),
        }),
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
        "debug" => Ok(SlashAction::Debug),
        "reload" => Ok(SlashAction::Reload),
        "refresh-models" | "models-refresh" => Ok(SlashAction::RefreshModels),
        "thinking-expand" => Ok(SlashAction::ToggleThinkingExpanded),
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

pub(crate) fn split_name_and_args(command: &str) -> (&str, &str) {
    let trimmed = command.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (trimmed, ""),
    }
}

/// parse the `--context-budget <value>` / `-b <value>` flag for the
/// `/compact` command. accepts an empty arg list (no budget) or a
/// single flag-value pair. anything else is rejected with the
/// caller-supplied usage error.
fn parse_context_budget(
    args: &str,
    usage: SlashParseError,
) -> Result<Option<TokenCount>, SlashParseError> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut tokens = trimmed.split_whitespace();
    let flag = tokens.next().ok_or_else(|| usage.clone())?;
    if !matches!(flag, "--context-budget" | "-b") {
        return Err(usage);
    }
    let value = tokens.next().ok_or_else(|| usage.clone())?;
    if tokens.next().is_some() {
        return Err(usage);
    }
    let budget: TokenCount = value.parse()?;
    Ok(Some(budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_thinking_expand_action() {
        // ctrl+o used to be hardcoded to toggle the focused message's
        // thinking visibility. it's been moved to a slash command so
        // ctrl+o stays free for whatever the user wants to bind it to
        assert_eq!(
            parse("/thinking-expand").unwrap(),
            SlashAction::ToggleThinkingExpanded
        );
    }

    #[test]
    fn parse_compact_bare() {
        // bare `/compact` keeps the historical no-arg behaviour
        assert_eq!(
            parse("/compact").unwrap(),
            SlashAction::Compact {
                context_budget: None
            }
        );
    }

    #[test]
    fn parse_compact_with_context_budget() {
        // `--context-budget` accepts plain ints and human suffixes
        // (k = 1_000, m = 1_000_000) via TokenCount::from_str
        use mush_ai::types::TokenCount;

        assert_eq!(
            parse("/compact --context-budget 10k").unwrap(),
            SlashAction::Compact {
                context_budget: Some(TokenCount::new(10_000))
            }
        );
        assert_eq!(
            parse("/compact --context-budget 1024").unwrap(),
            SlashAction::Compact {
                context_budget: Some(TokenCount::new(1024))
            }
        );
    }

    #[test]
    fn parse_compact_with_short_budget_flag() {
        use mush_ai::types::TokenCount;
        // `-b` is the short form, same semantics as `--context-budget`
        assert_eq!(
            parse("/compact -b 5k").unwrap(),
            SlashAction::Compact {
                context_budget: Some(TokenCount::new(5_000))
            }
        );
    }

    #[test]
    fn parse_fork_compact_is_no_longer_a_known_command() {
        // /fork-compact and /fc used to map to SlashAction::ForkCompact.
        // /compact now preserves the conversation tree natively (the
        // behaviour /fork-compact used to claim) so the alternate
        // command was removed. they now fall through to Other and the
        // runner surfaces an "unknown command" message
        assert!(matches!(
            parse("/fork-compact").unwrap(),
            SlashAction::Other { .. }
        ));
        assert!(matches!(parse("/fc").unwrap(), SlashAction::Other { .. }));
    }

    #[test]
    fn parse_compact_rejects_missing_budget_value() {
        assert!(parse("/compact --context-budget").is_err());
        assert!(parse("/compact -b").is_err());
    }

    #[test]
    fn parse_compact_rejects_bad_budget_value() {
        assert!(parse("/compact --context-budget garbage").is_err());
        assert!(parse("/compact -b 10g").is_err());
    }

    #[test]
    fn parse_compact_rejects_unknown_flag() {
        assert!(parse("/compact --frobnicate 10k").is_err());
    }

    #[test]
    fn parse_compaction_model() {
        assert_eq!(
            parse("/compaction-model").unwrap(),
            SlashAction::CompactionModel { model_id: None }
        );
        assert_eq!(
            parse("/compact-model claude-sonnet-4-20250514").unwrap(),
            SlashAction::CompactionModel {
                model_id: Some("claude-sonnet-4-20250514".into())
            }
        );
    }

    #[test]
    fn parse_model_action() {
        assert_eq!(
            parse("/model claude-sonnet").unwrap(),
            SlashAction::Model {
                model_id: Some("claude-sonnet".into()),
                show_all: false,
            }
        );
    }

    #[test]
    fn parse_model_all_flag_opens_picker_with_hidden_visible() {
        // /model --all is the opt-in for showing internal/experimental codex
        // entries that the default picker filters out. parse it as a flag
        // distinct from a model id named "--all"
        assert_eq!(
            parse("/model --all").unwrap(),
            SlashAction::Model {
                model_id: None,
                show_all: true,
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
    fn parse_new_command() {
        assert_eq!(parse("/new").unwrap(), SlashAction::New);
    }

    #[test]
    fn parse_clear_is_removed() {
        assert_eq!(
            parse("/clear").unwrap(),
            SlashAction::Other {
                name: "clear".into(),
                args: String::new(),
            }
        );
    }

    #[test]
    fn parse_debug_produces_debug_action() {
        assert_eq!(parse("/debug").unwrap(), SlashAction::Debug);
    }

    #[test]
    fn parse_resume_no_arg_means_most_recent_in_cwd() {
        // bare `/resume` is a quick "pick up where i left off" shortcut.
        // the handler resolves None to the most recent session in the
        // current cwd. an explicit id still works for surgical targeting
        assert_eq!(
            parse("/resume").unwrap(),
            SlashAction::Resume { session_id: None }
        );
    }

    #[test]
    fn parse_resume_with_id_keeps_explicit_target() {
        assert_eq!(
            parse("/resume abc123").unwrap(),
            SlashAction::Resume {
                session_id: Some(SessionId::from("abc123")),
            }
        );
    }

    #[test]
    fn parse_reload_command() {
        // /reload is the explicit cache-bust button: refreshes AGENTS.md
        // and template discovery so the user can pick up file edits
        // without restarting mush. opt-in because it does bust the
        // provider-side prefix cache by changing the system prompt
        assert_eq!(parse("/reload").unwrap(), SlashAction::Reload);
    }

    #[test]
    fn parse_refresh_models_command() {
        // /refresh-models triggers an on-demand discovery fetch for every
        // provider with creds, updating ~/.local/share/mush/discovered-models.json
        assert_eq!(
            parse("/refresh-models").unwrap(),
            SlashAction::RefreshModels
        );
        assert_eq!(
            parse("/models-refresh").unwrap(),
            SlashAction::RefreshModels
        );
    }
}
