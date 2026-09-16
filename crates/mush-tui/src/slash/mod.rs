pub mod commands;
pub mod compaction;
pub mod side_question;

use mush_ai::types::{SessionId, TokenCount, TokenCountParseError};
use thiserror::Error;

pub(crate) use commands::handle_login_start;
pub use commands::{expand_template, handle, handle_export, rebuild_display};
pub use compaction::{
    AppliedCompaction, CompactionTaskResult, MIN_MESSAGES_FOR_COMPACTION, PendingCompaction,
    apply_applied_compaction, apply_compaction_result, extract_fresh_summary,
    fallback_applied_compaction, handle_compact, run_compaction, start_compaction,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpQuery {
    Status,
    Tools { server: Option<String> },
    Schema { tool: String },
    Reconnect { server: String },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SlashParseError {
    #[error("slash commands must start with /")]
    MissingPrefix,
    #[error("usage: /branch <number> (try /tree first)")]
    BranchUsage,
    #[error("usage: /fork <number> (try /tree first)")]
    ForkUsage,
    #[error("usage: /logs [n]")]
    LogsUsage,
    #[error("usage: /mcp | /mcp tools [server] | /mcp schema <tool> | /mcp reconnect <server>")]
    McpUsage,
    #[error("usage: /login-complete <code>")]
    LoginCodeUsage,
    #[error("usage: /btw <question>")]
    SideUsage,
    #[error("usage: /rlm-max-depth [<non-negative integer>]")]
    RlmMaxDepthUsage,
    #[error("usage: /compaction-model [same|<model-id>]")]
    CompactionModelUsage,
    #[error(
        "usage: /compact [--context-budget <count>] [instructions] (budget e.g. 10k, 1024, 1.5k, 1m)"
    )]
    CompactUsage,
    #[error("invalid context budget: {0}")]
    InvalidContextBudget(#[from] TokenCountParseError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    Subagents {
        args: String,
    },
    Help,
    Keys,
    Clear,
    New,
    Model {
        model_id: Option<String>,
        show_all: bool,
    },
    Sessions,
    Resume {
        session_id: Option<SessionId>,
    },
    Branch {
        index: Option<usize>,
    },
    Fork {
        index: Option<usize>,
    },
    Clone,
    Tree,
    Compact {
        focus: mush_session::compact::CompactionFocus,
    },
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
    Credits,
    Name {
        title: Option<String>,
    },
    Skills,
    SessionInfo,
    RlmMaxDepth {
        max_depth: Option<u32>,
    },
    Side {
        question: String,
    },
    Endpoints {
        model_id: Option<String>,
    },
    Logs {
        count: usize,
    },
    Settings {
        args: String,
    },
    Login {
        provider: Option<String>,
    },
    LoginComplete {
        code: String,
    },
    Card,
    Mcp(McpQuery),
    TaskClaim {
        id: String,
        description: String,
    },
    TaskRelease {
        id: String,
    },
    TaskList,
    Debug,
    Reload,
    RefreshModels,
    Quit,
    ToggleThinkingExpanded,
    Other {
        name: String,
        args: String,
    },
}

impl SlashAction {
    #[must_use]
    pub fn is_safe_during_stream(&self) -> bool {
        !matches!(
            self,
            Self::Clear
                | Self::New
                | Self::Resume { .. }
                | Self::Branch { .. }
                | Self::Fork { .. }
                | Self::Clone
                | Self::Compact { .. }
                | Self::Undo
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
        "subagents" => Ok(SlashAction::Subagents {
            args: args.to_string(),
        }),
        "help" => Ok(SlashAction::Help),
        "keys" => Ok(SlashAction::Keys),
        "new" => Ok(SlashAction::New),
        "model" => {
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
        "fork" if args.is_empty() => Ok(SlashAction::Fork { index: None }),
        "fork" => args
            .parse::<usize>()
            .map(|index| SlashAction::Fork { index: Some(index) })
            .map_err(|_| SlashParseError::ForkUsage),
        "clone" => Ok(SlashAction::Clone),
        "tree" => Ok(SlashAction::Tree),
        "compact" => parse_compact_focus(args).map(|focus| SlashAction::Compact { focus }),
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
        "skills" => Ok(SlashAction::Skills),
        "session" => Ok(SlashAction::SessionInfo),
        "rlm-max-depth" if args.is_empty() => Ok(SlashAction::RlmMaxDepth { max_depth: None }),
        "rlm-max-depth" => args
            .parse()
            .map(|max_depth| SlashAction::RlmMaxDepth {
                max_depth: Some(max_depth),
            })
            .map_err(|_| SlashParseError::RlmMaxDepthUsage),
        "btw" | "side" if args.is_empty() => Err(SlashParseError::SideUsage),
        "btw" | "side" => Ok(SlashAction::Side {
            question: args.to_string(),
        }),
        "name" | "rename" => Ok(SlashAction::Name {
            title: (!args.is_empty()).then(|| args.to_string()),
        }),
        "credits" => Ok(SlashAction::Credits),
        "endpoints" => Ok(SlashAction::Endpoints {
            model_id: (!args.is_empty()).then(|| args.to_string()),
        }),
        "card" => Ok(SlashAction::Card),
        "mcp" => parse_mcp(args).map(SlashAction::Mcp),
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

pub(crate) fn split_name_and_args(command: &str) -> (&str, &str) {
    let trimmed = command.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (trimmed, ""),
    }
}

fn parse_compact_focus(
    args: &str,
) -> Result<mush_session::compact::CompactionFocus, SlashParseError> {
    let mut rest = args.trim();
    let mut context_budget = None;
    while let Some(flag_rest) = rest
        .strip_prefix("--context-budget")
        .or_else(|| rest.strip_prefix("-b"))
    {
        if !flag_rest.is_empty() && !flag_rest.starts_with(char::is_whitespace) {
            break;
        }
        let mut tokens = flag_rest.trim_start().splitn(2, char::is_whitespace);
        let value = tokens
            .next()
            .filter(|value| !value.is_empty())
            .ok_or(SlashParseError::CompactUsage)?;
        context_budget = Some(value.parse::<TokenCount>()?);
        rest = tokens.next().unwrap_or("").trim();
    }
    if rest.starts_with("--") {
        return Err(SlashParseError::CompactUsage);
    }
    Ok(mush_session::compact::CompactionFocus {
        context_budget,
        instructions: (!rest.is_empty()).then(|| rest.to_string()),
    })
}

fn parse_mcp(args: &str) -> Result<McpQuery, SlashParseError> {
    let (sub, rest) = split_name_and_args(args);
    match sub {
        "" => Ok(McpQuery::Status),
        "tools" => Ok(McpQuery::Tools {
            server: (!rest.is_empty()).then(|| rest.to_string()),
        }),
        "schema" if rest.is_empty() => Err(SlashParseError::McpUsage),
        "schema" => Ok(McpQuery::Schema {
            tool: rest.to_string(),
        }),
        "reconnect" if rest.is_empty() => Err(SlashParseError::McpUsage),
        "reconnect" => Ok(McpQuery::Reconnect {
            server: rest.to_string(),
        }),
        _ => Err(SlashParseError::McpUsage),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rlm_max_depth_shows_or_sets() {
        assert_eq!(
            parse("/rlm-max-depth").unwrap(),
            SlashAction::RlmMaxDepth { max_depth: None }
        );
        assert_eq!(
            parse("/rlm-max-depth 3").unwrap(),
            SlashAction::RlmMaxDepth { max_depth: Some(3) }
        );
        assert!(matches!(
            parse("/rlm-max-depth -1"),
            Err(SlashParseError::RlmMaxDepthUsage)
        ));
    }

    #[test]
    fn parse_mcp_bare_is_status() {
        assert_eq!(parse("/mcp").unwrap(), SlashAction::Mcp(McpQuery::Status));
    }

    #[test]
    fn parse_mcp_tools_with_and_without_a_server() {
        assert_eq!(
            parse("/mcp tools").unwrap(),
            SlashAction::Mcp(McpQuery::Tools { server: None })
        );
        assert_eq!(
            parse("/mcp tools sweethome3d").unwrap(),
            SlashAction::Mcp(McpQuery::Tools {
                server: Some("sweethome3d".into())
            })
        );
    }

    #[test]
    fn parse_mcp_schema_needs_a_tool_name() {
        assert_eq!(
            parse("/mcp schema sweethome3d_render_photo").unwrap(),
            SlashAction::Mcp(McpQuery::Schema {
                tool: "sweethome3d_render_photo".into()
            })
        );
        assert_eq!(parse("/mcp schema"), Err(SlashParseError::McpUsage));
    }

    #[test]
    fn parse_mcp_reconnect_needs_a_server() {
        assert_eq!(
            parse("/mcp reconnect sweethome3d").unwrap(),
            SlashAction::Mcp(McpQuery::Reconnect {
                server: "sweethome3d".into()
            })
        );
        assert_eq!(parse("/mcp reconnect"), Err(SlashParseError::McpUsage));
    }

    #[test]
    fn parse_mcp_rejects_an_unknown_subcommand() {
        assert_eq!(parse("/mcp wibble"), Err(SlashParseError::McpUsage));
    }

    #[test]
    fn mcp_status_is_safe_during_stream() {
        assert!(SlashAction::Mcp(McpQuery::Status).is_safe_during_stream());
    }

    #[test]
    fn parse_thinking_expand_action() {
        assert_eq!(
            parse("/thinking-expand").unwrap(),
            SlashAction::ToggleThinkingExpanded
        );
    }

    fn compact(context_budget: Option<u64>, instructions: Option<&str>) -> SlashAction {
        SlashAction::Compact {
            focus: mush_session::compact::CompactionFocus {
                context_budget: context_budget.map(TokenCount::new),
                instructions: instructions.map(str::to_string),
            },
        }
    }

    #[test]
    fn parse_compact_bare() {
        assert_eq!(parse("/compact").unwrap(), compact(None, None));
    }

    #[test]
    fn parse_compact_with_context_budget() {
        assert_eq!(
            parse("/compact --context-budget 10k").unwrap(),
            compact(Some(10_000), None)
        );
        assert_eq!(
            parse("/compact --context-budget 1024").unwrap(),
            compact(Some(1024), None)
        );
    }

    #[test]
    fn parse_compact_with_short_budget_flag() {
        assert_eq!(parse("/compact -b 5k").unwrap(), compact(Some(5_000), None));
    }

    #[test]
    fn parse_compact_with_instructions() {
        assert_eq!(
            parse("/compact focus on the auth refactor, keep the migration command").unwrap(),
            compact(
                None,
                Some("focus on the auth refactor, keep the migration command")
            )
        );
    }

    #[test]
    fn parse_compact_with_budget_and_instructions() {
        assert_eq!(
            parse("/compact -b 5k keep the failing tests").unwrap(),
            compact(Some(5_000), Some("keep the failing tests"))
        );
    }

    #[test]
    fn parse_compact_instructions_may_start_with_a_dash_word() {
        assert_eq!(
            parse("/compact -bare metal notes").unwrap(),
            compact(None, Some("-bare metal notes"))
        );
    }

    #[test]
    fn parse_fork_compact_is_no_longer_a_known_command() {
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
        assert_eq!(
            parse("/model --all").unwrap(),
            SlashAction::Model {
                model_id: None,
                show_all: true,
            }
        );
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
        assert_eq!(parse("/reload").unwrap(), SlashAction::Reload);
    }

    #[test]
    fn parse_refresh_models_command() {
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
